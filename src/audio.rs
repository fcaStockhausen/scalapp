use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::atomicf32::AtomicF32;
use crate::devices;

pub fn scale_volume(volume: f32) -> f32 {
    let volume = volume.clamp(0.0, 1.0);
    let base = if volume <= 0.0 {
        0.0
    } else if volume <= 0.8 {
        volume.powf(1.2) * 1.25
    } else {
        0.64 + (volume - 0.8) * 1.8
    };
    if volume >= 0.95 {
        1.0
    } else if volume >= 0.85 {
        (0.85 + (volume - 0.85) * 1.0).min(1.0)
    } else {
        base
    }
}

/// Rango físico útil del volumen: la escala de UI (0-100%) se mapea a
/// este rango — arriba de ~30% la señal satura y no se usa. Así la
/// slider recorre con resolución el intervalo que realmente se ajusta.
pub const VOLUME_RANGE: f32 = 0.30;

/// usuario (0.0-1.0) → volumen físico del motor (0.0-VOLUME_RANGE)
pub fn user_volume_to_actual(u: f32) -> f32 {
    u.clamp(0.0, 1.0) * VOLUME_RANGE
}

/// volumen físico → escala de usuario (para defaults y display)
pub fn actual_volume_to_user(a: f32) -> f32 {
    (a / VOLUME_RANGE).clamp(0.0, 1.0)
}

struct Ring {
    buf: Vec<f32>,
    write: usize,
    read: usize,
}

impl Ring {
    fn new(capacity_frames: usize, channels: usize) -> Self {
        Self {
            buf: vec![0.0; capacity_frames * channels],
            write: 0,
            read: 0,
        }
    }

    fn available(&self, capacity_frames: usize) -> usize {
        if self.write >= self.read {
            self.write - self.read
        } else {
            capacity_frames - self.read + self.write
        }
    }
}

fn write_ring(ring: &mut Ring, data: &[f32], channels: usize, capacity_frames: usize) {
    let frames = data.len() / channels;
    let end = ring.write + frames;
    if end <= capacity_frames {
        ring.buf[ring.write * channels..end * channels].copy_from_slice(data);
    } else {
        let first = capacity_frames - ring.write;
        ring.buf[ring.write * channels..].copy_from_slice(&data[..first * channels]);
        ring.buf[..(end - capacity_frames) * channels].copy_from_slice(&data[first * channels..]);
    }
    ring.write = end % capacity_frames;
}

#[derive(Clone)]
struct NotchCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

/// Cadena de filtrado del hum del chip (--notch-hum): cuando streamea
/// video, la MS2131 acopla su refresco vertical al path de audio —
/// medido 29/10 con resolución fina: tono a 60.06 Hz + armónicos
/// (120/180) + subarmónico ~30 Hz, +7dB sobre el nivel audio-only.
/// Cadena: high-pass 45 Hz (rumor 28-35 Hz) + notches a 60/120/180.
pub struct NotchBank {
    fs: f32,
    coeffs: Vec<NotchCoeffs>,
    /// [biquad][canal] = [w1, w2]
    states: Vec<[[f32; 2]; 2]>,
}

impl NotchBank {
    pub fn new(sample_rate: u32, freqs: &[f32], q: f32) -> Self {
        let coeffs = freqs
            .iter()
            .map(|&f0| {
                let w0 = 2.0 * std::f32::consts::PI * f0 / sample_rate as f32;
                let alpha = w0.sin() / (2.0 * q);
                let a0 = 1.0 + alpha;
                NotchCoeffs {
                    b0: 1.0 / a0,
                    b1: (-2.0 * w0.cos()) / a0,
                    b2: 1.0 / a0,
                    a1: (-2.0 * w0.cos()) / a0,
                    a2: (1.0 - alpha) / a0,
                }
            })
            .collect();
        let states = vec![[[0.0f32; 2]; 2]; freqs.len()];
        Self {
            fs: sample_rate as f32,
            coeffs,
            states,
        }
    }

    /// Notches quirúrgicos del hum (medidos, crystal-estables):
    /// - 29/10: tono a 60.06 + armónicos 120.12/180.18.
    /// - 14/11 (thump "tum tum" ~1Hz): el acople en 1080p60 nativo es un
    ///   PAR de tonos 59.5/60.5 (18/18 ventanas estables, diagnostico
    ///   con tools/audio_diag.py) que BATE a ~1Hz — dos inaudibles que
    ///   se suman/cancelan una vez por segundo. Sus 2º armónicos
    ///   119.0/121.0 por las dudas.
    /// Q: 25 para la terna histórica; 100 para el par (estabilidad
    /// medida ±0.1Hz en 146s → Q alto seguro): media banda -3dB de
    /// ±0.3Hz → Bb1 (58.3Hz) y B1 (61.7Hz) intactos.
    pub fn hum_notches(sample_rate: u32) -> Self {
        let mut bank = Self::new(sample_rate, &[], 1.0);
        for (f0, q) in [
            (60.06f32, 25.0),
            (120.12, 25.0),
            (180.18, 25.0),
            (59.5, 100.0),
            (60.5, 100.0),
            (119.0, 100.0),
            (121.0, 100.0),
        ] {
            bank.push_notch(f0, q);
        }
        // + slots del par adaptativo (estacionados en defaults; el
        // tracker los re-afina en caliente al medir el par real)
        bank.with_auto_hum_slots()
    }

    /// Agrega un notch con Q propio (los tonos de acople del chip son
    /// cristal-estables: Q alto = más quirúrgico sin riesgo).
    pub fn push_notch(&mut self, f0: f32, q: f32) {
        let w0 = 2.0 * std::f32::consts::PI * f0 / self.fs;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        self.coeffs.push(NotchCoeffs {
            b0: 1.0 / a0,
            b1: (-2.0 * w0.cos()) / a0,
            b2: 1.0 / a0,
            a1: (-2.0 * w0.cos()) / a0,
            a2: (1.0 - alpha) / a0,
        });
        self.states.push([[0.0f32; 2]; 2]);
    }

    /// Slots del par adaptativo — 6 frecuencias × 2 biquads EN CASCADA
    /// (profundidad doble: un solo biquad f32 deja colita audible).
    /// Orden: [f1, f2, sub1, sub2, 2f1, 2f2] cada uno ×2. Estacionados
    /// en los defaults medidos hasta que el tracker fija los reales.
    pub fn with_auto_hum_slots(mut self) -> Self {
        for f in [59.55f32, 60.45, 29.77, 30.72, 119.1, 120.9] {
            self.push_notch(f, 100.0);
            self.push_notch(f, 100.0);
        }
        self
    }

    /// Re-afina los slots adaptativos a las frecuencias MEDIDAS (Goertzel
    /// en vivo): par de 60 + par SUBARMÓNICO (~30Hz, el leak del 18/11)
    /// + 2º armónicos. Coeficientes recomputados IN PLACE conservando
    /// estados (sin click).
    pub fn update_auto(&mut self, f1: f64, f2: f64, sub1: f64, sub2: f64) {
        let base = self.coeffs.len().saturating_sub(12);
        let freqs = [
            f1, f1, f2, f2, sub1, sub1, sub2, sub2, f1 * 2.0, f1 * 2.0, f2 * 2.0, f2 * 2.0,
        ];
        for (i, f) in freqs.into_iter().enumerate() {
            let f0 = f as f32;
            let q = 100.0f32;
            let w0 = 2.0 * std::f32::consts::PI * f0 / self.fs;
            let alpha = w0.sin() / (2.0 * q);
            let a0 = 1.0 + alpha;
            if let Some(c) = self.coeffs.get_mut(base + i) {
                *c = NotchCoeffs {
                    b0: 1.0 / a0,
                    b1: (-2.0 * w0.cos()) / a0,
                    b2: 1.0 / a0,
                    a1: (-2.0 * w0.cos()) / a0,
                    a2: (1.0 - alpha) / a0,
                };
            }
        }
    }

    /// Agrega un high-pass 4º orden (2 biquads cascados). Opt-in via
    /// --hpf: mata el rumor 28-38Hz del chip PERO también el sub-bajo
    /// del contenido (a 41Hz/E1 ≈ -10dB con hpf=48) — decisión del
    /// usuario según qué esté monitoreando.
    pub fn with_highpass(mut self, f0: f32) -> Self {
        self.push_highpass(f0);
        self.push_highpass(f0);
        self
    }

    fn push_highpass(&mut self, f0: f32) {
        let w0 = 2.0 * std::f32::consts::PI * f0 / self.fs;
        // Butterworth: Q = 1/sqrt(2) → alpha = sin(w0)/(2Q) = sin(w0)/sqrt(2)
        let alpha = w0.sin() / std::f32::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let cosw = w0.cos();
        self.coeffs.push(NotchCoeffs {
            b0: ((1.0 + cosw) / 2.0) / a0,
            b1: (-(1.0 + cosw)) / a0,
            b2: ((1.0 + cosw) / 2.0) / a0,
            a1: (-2.0 * cosw) / a0,
            a2: (1.0 - alpha) / a0,
        });
        self.states.push([[0.0f32; 2]; 2]);
    }

    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let mut y = x;
        for (i, c) in self.coeffs.iter().enumerate() {
            let s = &mut self.states[i][ch];
            let out = c.b0 * y + s[0];
            s[0] = c.b1 * y - c.a1 * out + s[1];
            s[1] = c.b2 * y - c.a2 * out;
            y = out;
        }
        y
    }

    /// Filtra un buffer interleaved estéolo in-place.
    /// Clona coeficientes+estados (para tests de re-afinado).
    #[cfg(test)]
    pub(crate) fn clone_bank_for_test(&self) -> Self {
        Self {
            fs: self.fs,
            coeffs: self.coeffs.clone(),
            states: self.states.clone(),
        }
    }

    pub(crate) fn process_interleaved(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            let (l, r) = (frame[0], frame[1]);
            frame[0] = self.process(0, l);
            frame[1] = self.process(1, r);
        }
    }
}

pub struct AudioStreams {
    input: Arc<Mutex<Option<Stream>>>,
    playback_output: Option<Stream>,
    // Parche de negociación (29/10): la entrada puede morir cuando el
    // dispositivo renegocia (cambio de modo USB, etc.). El watchdog
    // (solo atómicos) levanta la bandera; el MAIN thread (renderer)
    // atiende via poll_rebuild().
    rebuild_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    rebuild_next_ms: Arc<AtomicU64>,
    rebuild_fn: Option<Box<dyn Fn() -> Result<Stream>>>,
    wd_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    wd_handle: Option<std::thread::JoinHandle<()>>,
    /// Tracker del hum (notch adaptativo).
    hum_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    hum_handle: Option<std::thread::JoinHandle<()>>,
}

/// Loop del notch adaptativo: cada 3s mide el par de acople real
/// (Goertzel sobre los últimos 4s de entrada) y re-afina los slots
/// del NotchBank. Confirma con 2 ventanas consecutivas estables antes
/// de tocar; luego solo actualiza si se movió >0.03Hz.
fn hum_auto_loop(
    hb: Arc<Mutex<HumTrackerBuf>>,
    playback: Arc<AudioPlayback>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    fs: f64,
) {
    let mut last: Option<((f64, f64), (f64, f64))> = None;
    let mut confirmed = false;
    let mut committed: Option<((f64, f64), (f64, f64))> = None;
    let mut last_update = Instant::now();
    loop {
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let snap = hb.lock().unwrap().snapshot();
        // par de 60 (simétrico, estricto) + par subarmónico ~30 (laxo:
        // los 2 picos prominentes — NO es simétrico, medido 29.77/30.72)
        let pair60 = detect_hum_pair(&snap, fs);
        let pair30 = detect_sub_pair(&snap, fs);
        if let Some(((f1, f2), (s1, s2))) = pair60.zip(pair30).map(|(a, b)| (a, b)) {
            let cur = ((f1, f2), (s1, s2));
            let stable = last.map_or(false, |(p, q)| {
                (p.0 - f1).abs() < 0.06
                    && (p.1 - f2).abs() < 0.06
                    && (q.0 - s1).abs() < 0.08
                    && (q.1 - s2).abs() < 0.08
            });
            last = Some(cur);
            if !stable && !confirmed {
                continue; // primera medición: exigir confirmación
            }
            confirmed = true;
            let moved = committed.map_or(f64::MAX, |((c1, c2), (d1, d2))| {
                (c1 - f1)
                    .abs()
                    .max((c2 - f2).abs())
                    .max((d1 - s1).abs())
                    .max((d2 - s2).abs())
            });
            if moved > 0.1 || last_update.elapsed().as_secs() >= 8 {
                let handle = playback.notch_handle.lock().unwrap().clone();
                if let Some(bank) = handle.as_ref() {
                    bank.lock().unwrap().update_auto(f1, f2, s1, s2);
                    println!(
                        "[notch-auto @{}] 60Hz: {f1:.2}/{f2:.2} (batido {b60:.2}Hz) | 30Hz: {s1:.2}/{s2:.2} (batido {b30:.2}Hz = período {p30:.2}s) → 12 slots re-afinados ×2 cascada",
                        wall_clock(),
                        b60 = f2 - f1,
                        b30 = s2 - s1,
                        p30 = 1.0 / (s2 - s1),
                    );
                    committed = Some(cur);
                    last_update = Instant::now();
                }
            }
        } else {
            // sin pares claros (audio sin acople) — reset de confirmación
            confirmed = false;
        }
    }
}

fn now_ms() -> u64 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// HH:MM:SS de reloj de pared LOCAL — para correlacionar los eventos de
/// audio con logs externos (formatguard, consola).
pub(crate) fn wall_clock() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::from("??:??:??");
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

impl AudioStreams {
    pub fn stop(&mut self) {
        if let Some(f) = self.wd_stop.take() {
            f.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.wd_handle.take() {
            let _ = h.join();
        }
        if let Some(f) = self.hum_stop.take() {
            f.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.hum_handle.take() {
            let _ = h.join();
        }
        drop(self.input.lock().unwrap().take());
        drop(self.playback_output.take());
        println!("Audio playback stopped");
    }

    /// Atiende pedidos de rebuild del watchdog. Llamar periódicamente
    /// desde el main thread (el renderer lo bombea en cada frame).
    pub fn poll_rebuild(&mut self) {
        let flag = match &self.rebuild_flag {
            Some(f) => f,
            None => return,
        };
        if !flag.load(Ordering::Relaxed) {
            return;
        }
        let now = now_ms();
        if now < self.rebuild_next_ms.load(Ordering::Relaxed) {
            return;
        }
        flag.store(false, Ordering::Relaxed);
        self.rebuild_next_ms.store(now + 2000, Ordering::Relaxed);
        let make = match &self.rebuild_fn {
            Some(m) => m,
            None => return,
        };
        println!("[watchdog] reconstruyendo stream de audio...");
        let old = self.input.lock().unwrap().take();
        drop(old);
        match make().and_then(|s| {
            s.play().context("failed to restart audio input")?;
            Ok(s)
        }) {
            Ok(s) => {
                *self.input.lock().unwrap() = Some(s);
                println!("[watchdog] entrada de audio reconstruida ✓");
            }
            Err(e) => {
                eprintln!("[watchdog] fallo rebuild audio: {e:#} — reintento en 2s");
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Potencia Goertzel de `f` sobre buf mono (rect window, f64).
fn goertzel_power(buf: &[f32], fs: f64, f: f64) -> f64 {
    let coeff = 2.0 * (2.0 * std::f64::consts::PI * f / fs).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for chunk in buf.chunks(8192) {
        for &v in chunk {
            let s = f64::from(v) + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// Detecta el PAR de acople video→audio en una ventana mono (≥3s).
///
/// El par deriva ±0.15Hz entre sesiones (medido: 59.549/60.451 el
/// 14/11, 59.64/60.34 el 17/11 — SIEMPRE simétrico alrededor de
/// 60.000): los notches fijos lo agarran de refilón. Esto lo mide:
/// barrido Goertzel 58.5-61.5, pico dominante a cada lado de 60,
/// refinado parabólico. Requisitos (anti-falso-positivo contra bajo
/// del contenido): prominencia ≥ +8dB sobre la mediana, simetría
/// (f1+f2 = 120.000±0.25), cada pico en [59.45,59.95]/[60.05,60.55].
pub fn detect_hum_pair(buf: &[f32], fs: f64) -> Option<(f64, f64)> {
    let (l, r) = detect_symmetric_pair(buf, fs, 58.5, 61.5, 60.0, 0.25, 6.3)?;
    if !(59.45..=59.95).contains(&l) || !(60.05..=60.55).contains(&r) {
        return None;
    }
    Some((l, r))
}

/// Versión genérica: par simétrico alrededor de `center` (el de 60Hz).
fn detect_symmetric_pair(
    buf: &[f32],
    fs: f64,
    lo: f64,
    hi: f64,
    center: f64,
    sym_tol: f64,
    prominence: f64,
) -> Option<(f64, f64)> {
    let need = (fs * 3.0) as usize;
    if buf.len() < need {
        return None;
    }
    let win_len = (fs as usize * 4).min(buf.len());
    let win = &buf[buf.len() - win_len..];
    let mean = win.iter().map(|v| f64::from(*v)).sum::<f64>() / win_len as f64;
    let win: Vec<f32> = win.iter().map(|v| *v - mean as f32).collect();

    let step = 0.02f64;
    let mut freqs = Vec::new();
    let mut powers = Vec::new();
    let mut f = lo;
    while f <= hi {
        freqs.push(f);
        powers.push(goertzel_power(&win, fs, f));
        f += step;
    }
    let mut sorted = powers.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = sorted[sorted.len() / 2].max(1e-20);

    let refine = |i: usize| -> f64 {
        if i == 0 || i + 1 >= powers.len() {
            return freqs[i];
        }
        let (y0, y1, y2) = (powers[i - 1], powers[i], powers[i + 1]);
        let denom = y0 - 2.0 * y1 + y2;
        if denom.abs() < 1e-20 {
            freqs[i]
        } else {
            freqs[i] + 0.5 * (y0 - y2) / denom * step
        }
    };
    let (mut li, mut lp) = (0usize, 0.0f64);
    let (mut ri, mut rp) = (0usize, 0.0f64);
    for (i, &p) in powers.iter().enumerate() {
        if p < med * prominence {
            continue;
        }
        let is_peak = i > 0 && i + 1 < powers.len() && p >= powers[i - 1] && p >= powers[i + 1];
        if !is_peak {
            continue;
        }
        if freqs[i] < center && p > lp {
            li = i;
            lp = p;
        }
        if freqs[i] > center && p > rp {
            ri = i;
            rp = p;
        }
    }
    if lp == 0.0 || rp == 0.0 {
        return None;
    }
    let (l, r) = (refine(li), refine(ri));
    if (l + r - 2.0 * center).abs() > 2.0 * sym_tol {
        return None;
    }
    Some((l, r))
}

/// Detecta el par SUBARMÓNICO (~30Hz): el leak medido el 18/11 — el
/// par de 60 cae +18dB, pero 29.77/30.72 queda a -3/-5dB del contenido
/// con su PROPIO batido ~0.96Hz (el thump que seguía audible). Este par
/// NO es simétrico alrededor de 30.000 → detección laxa: los 2 picos
/// más prominentes de la banda, separados ≥0.3Hz, cada uno ≥+8dB sobre
/// la mediana. Banda acotada (28.8-31.8) para no confundir con bajo:
/// E1=41.2Hz queda lejos; B0=30.87 solo sufre si cae EXACTO en un pico
/// (banda Q=100 = ±0.3Hz).
pub fn detect_sub_pair(buf: &[f32], fs: f64) -> Option<(f64, f64)> {
    let need = (fs * 3.0) as usize;
    if buf.len() < need {
        return None;
    }
    let win_len = (fs as usize * 4).min(buf.len());
    let win = &buf[buf.len() - win_len..];
    let mean = win.iter().map(|v| f64::from(*v)).sum::<f64>() / win_len as f64;
    let win: Vec<f32> = win.iter().map(|v| *v - mean as f32).collect();

    let step = 0.02f64;
    let (lo, hi) = (28.8f64, 31.8f64);
    let mut freqs = Vec::new();
    let mut powers = Vec::new();
    let mut f = lo;
    while f <= hi {
        freqs.push(f);
        powers.push(goertzel_power(&win, fs, f));
        f += step;
    }
    let mut sorted = powers.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = sorted[sorted.len() / 2].max(1e-20);

    let mut peaks: Vec<(f64, f64)> = Vec::new();
    for i in 1..powers.len().saturating_sub(1) {
        let p = powers[i];
        if p < med * 6.3 {
            continue;
        }
        if p >= powers[i - 1] && p >= powers[i + 1] {
            let (y0, y1, y2) = (powers[i - 1], p, powers[i + 1]);
            let denom = y0 - 2.0 * y1 + y2;
            let fr = if denom.abs() < 1e-20 {
                freqs[i]
            } else {
                freqs[i] + 0.5 * (y0 - y2) / denom * step
            };
            peaks.push((fr, p));
        }
    }
    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    if peaks.len() < 2 {
        return None;
    }
    let (f1, _) = peaks[0];
    // el segundo pico más fuerte que esté ≥0.3Hz del primero
    let (f2, _) = peaks[1..]
        .iter()
        .find(|(fr, _)| (fr - f1).abs() >= 0.3)
        .copied()
        .unwrap_or(peaks[1]);
    if (f2 - f1).abs() < 0.3 {
        return None;
    }
    Some((f1.min(f2), f1.max(f2)))
}

/// Buffer mono (downmix L/R) de la entrada para el tracker del hum.
pub struct HumTrackerBuf {
    buf: Vec<f32>,
    cap: usize,
}

impl HumTrackerBuf {
    pub fn new(fs: u32) -> Self {
        let cap = fs as usize * 5;
        Self {
            buf: Vec::with_capacity(cap),
            cap,
        }
    }

    pub fn feed(&mut self, interleaved: &[f32]) {
        for p in interleaved.chunks_exact(2) {
            self.buf.push((p[0] + p[1]) * 0.5);
        }
        if self.buf.len() > self.cap {
            let drop = self.buf.len() - self.cap;
            self.buf.drain(..drop);
        }
    }

    pub fn snapshot(&self) -> Vec<f32> {
        self.buf.clone()
    }
}

pub struct AudioPlayback {
    buffer: Arc<Mutex<VecDeque<Vec<f32>>>>,
    multiplier: Arc<AtomicF32>,
    low_latency: bool,
    channels: usize,
    /// Tope de la cola en SAMPLES (fix 29/10: con bloques Default de
    /// ~4096 frames, el tope "30 bloques" permitía ~2.5s de latencia).
    cap_samples: Arc<std::sync::atomic::AtomicUsize>,
    /// Handle al NotchBank de la salida (para el tracker adaptativo:
    /// re-afina las frecuencias en caliente sin reconstruir estados).
    notch_handle: Mutex<Option<Arc<Mutex<NotchBank>>>>,
}

impl AudioPlayback {
    pub fn new(channels: usize, low_latency: bool) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            multiplier: Arc::new(AtomicF32::new(0.2)),
            low_latency,
            channels,
            cap_samples: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            notch_handle: Mutex::new(None),
        }
    }

    pub fn start(
        &self,
        sample_rate: u32,
        block_size: u32,
        cushion_ms: Arc<AtomicU64>,
        notch_hum: bool,
        hpf_hz: u32,
        notch_override: Option<Vec<f32>>,
    ) -> Result<Stream> {
        // tope de seguridad por tiempo (~0.75s) — con deriva compensada
        // la cola vive cerca del objetivo, no del tope
        self.cap_samples.store(
            (0.75 * sample_rate as f32) as usize * self.channels,
            Ordering::Relaxed,
        );
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default output audio device")?;
        let config = StreamConfig {
            channels: self.channels as u16,
            sample_rate: SampleRate(sample_rate),
            buffer_size: BufferSize::Fixed(block_size),
        };
        // Motor 29/10: consumidor con DERIVA COMPENSADA portado del
        // passthrough (validado semanas en audio-only). Los relojes de
        // captura y salida derivan (cristales distintos): sin compensación
        // la cola crece hasta el tope y el descarte = crackle digital.
        // Interpolación lineal ±0.2% (inaudible) manteniendo la cola en
        // ~TARGET_MS — delay bajo y estable, sin glitches.
        struct Drift {
            frac_pos: f32,
            block: Vec<f32>,
            primed: bool,
        }
        // Diagnóstico (30/10): blip auditivo ~1/min. Instrumentamos
        // underruns (silencio insertado), trayectoria de la cola y el
        // dispositivo de salida — con timestamps para casar con lo que
        // se escucha.
        struct Diag {
            start: Instant,
            min_q: usize,
            max_q: usize,
            sum_q: f64,
            count: u64,
            window: Instant,
            last_underrun_log: Instant,
        }
        impl Diag {
            fn new() -> Self {
                Self {
                    start: Instant::now(),
                    min_q: usize::MAX,
                    max_q: 0,
                    sum_q: 0.0,
                    count: 0,
                    window: Instant::now(),
                    last_underrun_log: Instant::now()
                        - std::time::Duration::from_secs(10),
                }
            }
            fn t(&self) -> f64 {
                self.start.elapsed().as_secs_f64()
            }
        }
        let drift = Arc::new(Mutex::new(Drift {
            frac_pos: 0.0,
            block: Vec::new(),
            primed: false,
        }));
        let diag = Arc::new(Mutex::new(Diag::new()));
        let buffer = self.buffer.clone();
        let drift_out = drift.clone();
        let diag_out = diag.clone();
        let cushion_out = cushion_ms.clone();
        // Filtro anti-hum (opt-in) + grabador de SALIDA para validación
        // (SCALAPP_REC_OUT, post-filtro).
        let notch_out = (notch_hum || hpf_hz > 0).then(|| {
            let mut bank = if let Some(freqs) = &notch_override {
                NotchBank::new(sample_rate, freqs, 25.0)
            } else if notch_hum {
                NotchBank::hum_notches(sample_rate)
            } else {
                NotchBank::new(sample_rate, &[], 1.0)
            };
            if hpf_hz > 0 {
                bank = bank.with_highpass(hpf_hz as f32);
            }
            Arc::new(Mutex::new(bank))
        });
        let notch_cb = notch_out.clone();
        // handle para el tracker adaptativo (re-afinado en caliente)
        *self.notch_handle.lock().unwrap() = notch_out.clone();
        let rec_out_file = std::env::var("SCALAPP_REC_OUT").ok().map(|path| {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap_or_else(|e| panic!("SCALAPP_REC_OUT {path}: {e}"));
            println!("[rec] grabando salida post-notch a {path}");
            Arc::new(Mutex::new(f))
        });
        let rec_out_cb = rec_out_file.clone();
        println!(
            "Audio playback device: {}",
            device.name().unwrap_or_else(|_| "?".into())
        );
        if let Some(freqs) = &notch_override {
            println!(
                "✓ Notch anti-hum OVERRIDE: {} Hz (Q=25, quirúrgico)",
                freqs
                    .iter()
                    .map(|f| format!("{f:.2}"))
                    .collect::<Vec<_>>()
                    .join("/")
            );
        } else if notch_hum {
            println!(
                "✓ Notch anti-hum activo: 59.5/60.06/60.5/119/120.12/121/180.18 Hz (Q=25, quirúrgico) — par 59.5/60.5 = batido ~1Hz (thump 14/11)"
            );
        }
        if hpf_hz > 0 {
            println!("✓ High-pass {hpf_hz}Hz activo (ojo: atenúa el sub-bajo del contenido)");
        }
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = buffer.lock().unwrap();
                    let mut d = drift_out.lock().unwrap();
                    let mut dg = diag_out.lock().unwrap();
                    let channels = 2usize;
                    let target_ms = (cushion_out.load(Ordering::Relaxed) as f32).max(20.0);
                    let target_samples =
                        (target_ms / 1000.0 * sample_rate as f32) as usize * channels;
                    let queued: usize = buf.iter().map(|b| b.len()).sum();
                    if d.primed {
                        if queued < dg.min_q {
                            dg.min_q = queued;
                        }
                        if queued > dg.max_q {
                            dg.max_q = queued;
                        }
                        dg.sum_q += queued as f64;
                        dg.count += 1;
                        if dg.window.elapsed().as_secs() >= 5 {
                            let avg = dg.sum_q / dg.count.max(1) as f64;
                            let ms = |s: usize| {
                                s as f64 * 1000.0 / (sample_rate as f64 * channels as f64)
                            };
                            println!(
                                "[audio t={:.0}s @{}] cola: min={:.1}ms avg={:.1}ms max={:.1}ms (target {:.0}ms)",
                                dg.t(),
                                wall_clock(),
                                ms(dg.min_q),
                                ms(avg as usize),
                                ms(dg.max_q),
                                target_ms
                            );
                            dg.min_q = usize::MAX;
                            dg.max_q = 0;
                            dg.sum_q = 0.0;
                            dg.count = 0;
                            dg.window = Instant::now();
                        }
                    }
                    // priming: arrancar solo con colchón
                    if !d.primed {
                        if queued < target_samples {
                            out.fill(0.0);
                            return;
                        }
                        d.primed = true;
                    }
                    // corrección de deriva: exceso → consumir más rápido;
                    // déficit → más lento (mantener ~target). Clamp
                    // asimétrico (30/10): el device dropea audio en ráfagas
                    // (~3ms/s promedio con video activo) y el refill a
                    // -0.2% (2ms/s) no recuperaba entre ráfagas → la cola
                    // vivía desangrada y los underruns seguían. Con -1% en
                    // déficit profundo se recuperan 200ms en ~30s; cerca
                    // del target el control es lineal (~-0.07% constante,
                    // imperceptible) y el clamp ni se nota.
                    let err = queued as f32 - target_samples as f32;
                    let rate = 1.0 + (err / (target_samples as f32 * 40.0))
                        .clamp(-0.01, 0.002);
                    let frames_out = out.len() / channels;
                    let mut silent_frames = 0usize;
                    for fi in 0..frames_out {
                        loop {
                            let need_next =
                                d.block.len() > (d.frac_pos as usize + 2) * channels;
                            if need_next
                                || buf.is_empty()
                                    && d.block.len() > (d.frac_pos as usize + 1) * channels
                            {
                                break;
                            }
                            match buf.pop_front() {
                                Some(next) => {
                                    if d.block.is_empty() {
                                        d.block = next;
                                        d.frac_pos = 0.0;
                                    } else {
                                        d.block.extend_from_slice(&next);
                                    }
                                }
                                None => break,
                            }
                        }
                        let base = (d.frac_pos as usize) * channels;
                        if base + channels >= d.block.len() {
                            // sin más datos: silencio (hambre real)
                            silent_frames += 1;
                            for ch in 0..channels {
                                out[fi * channels + ch] = 0.0;
                            }
                            continue;
                        }
                        let t = d.frac_pos.fract();
                        for ch in 0..channels {
                            let a = d.block[base + ch];
                            let b = d.block[base + channels + ch];
                            out[fi * channels + ch] = a + (b - a) * t;
                        }
                        d.frac_pos += rate;
                    }
                    let consumed = (d.frac_pos as usize).saturating_sub(4) * channels;
                    if consumed > 0 && consumed < d.block.len() {
                        d.block.drain(..consumed);
                        d.frac_pos -= (consumed / channels) as f32;
                    }
                    if silent_frames > 0 && dg.last_underrun_log.elapsed().as_secs() >= 1 {
                        println!(
                            "[audio t={:.0}s @{}] UNDERRUN: {silent_frames} frames silenciados ({:.1}ms), cola al inicio={queued} samples",
                            dg.t(),
                            wall_clock(),
                            silent_frames as f64 * 1000.0 / sample_rate as f64
                        );
                        dg.last_underrun_log = Instant::now();
                    }
                    // notch del hum + grabación de salida (validación)
                    if let Some(nb) = &notch_cb {
                        let mut bank = nb.lock().unwrap();
                        bank.process_interleaved(out);
                    }
                    if let Some(rec) = &rec_out_cb {
                        use std::io::Write;
                        let bytes: Vec<u8> =
                            out.iter().flat_map(|s| s.to_le_bytes()).collect();
                        let _ = rec.lock().unwrap().write_all(&bytes);
                    }
                },
                |err| eprintln!("Audio output status: {err}"),
                None,
            )
            .context("failed to build audio output stream")?;
        stream
            .play()
            .context("failed to start audio output stream")?;
        let mode = if self.low_latency { "low-latency" } else { "normal" };
        println!(
            "Audio playback started ({mode}): {sample_rate}Hz, {} channels, blocksize={block_size}",
            self.channels
        );
        Ok(stream)
    }

    pub fn set_volume(&self, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        self.multiplier.set(v);
        println!("Volume set to: {}%", (v * 100.0) as i32);
    }

    pub fn add_audio_data(&self, mut data: Vec<f32>) {
        // Auditado 24/10: el tope de 3 bloques (~17ms) cortaba el audio
        // porque los relojes de captura y salida derivan siempre (cristales
        // distintos) y al llenar el tope se borraba un bloque = corte.
        // Nuevo: sin tope agresivo; catch-up solo si se acumula GROSERA
        // cantidad (>30 bloques, un callback dormido >~170ms) para no
        // acumular latencia infinita.
        // Fix 29/10: tope por SAMPLES (~0.75s) — el tope de bloques
        // dependía del tamaño de bloque de entrada (Default ~4096 frames)
        let m = self.multiplier.get();
        if m != 1.0 {
            for s in &mut data {
                *s *= m;
            }
        }
        let mut buf = self.buffer.lock().unwrap();
        buf.push_back(data);
        let cap = self.cap_samples.load(Ordering::Relaxed);
        if cap > 0 {
            let mut total: usize = buf.iter().map(|b| b.len()).sum();
            let mut dropped_blocks = 0usize;
            while total > cap && buf.len() > 1 {
                if let Some(d) = buf.pop_front() {
                    total -= d.len();
                    dropped_blocks += 1;
                }
            }
            if dropped_blocks > 0 {
                println!("[audio] tope de cola: descartados {dropped_blocks} bloques (total {total} samples)");
            }
        }
    }
}

#[allow(dead_code)]
pub struct AudioCapture {
    pub sample_rate: u32,
    pub channels: usize,
    pub block_size: u32,
    pub buffer_size: f32,
    pub low_latency_mode: bool,
    pub enable_playback: bool,
    device_index: Option<usize>,
    latency_buffer: Arc<AtomicF32>,
    ring: Arc<Mutex<Ring>>,
    buffer_size_samples: usize,
    total_captured: Arc<AtomicU64>,
    total_played: AtomicU64,
    start_time: Mutex<Option<Instant>>,
    playback: Arc<Mutex<Option<Arc<AudioPlayback>>>>,
    volume: Mutex<f32>,
    /// Colchón base del playback en ms (0 = auto: ~2.2 bloques de entrada).
    /// El colchón adaptativo puede elevarlo por encima si el device
    /// dropea audio (MS2131: gaps ~100-220ms cada ~1min con video activo).
    cushion_base_ms: AtomicU64,
    /// Notch del hum del chip (59.94/120/180 Hz con video activo).
    notch_hum: std::sync::atomic::AtomicBool,
    /// High-pass opcional en Hz (0 = off). Mata el rumor 28-38Hz del
    /// chip, también el sub-bajo del contenido — decisión del usuario.
    hpf_hz: AtomicU32,
    /// Override de frecuencias de notch (--notch-hum-at).
    notch_override: std::sync::Mutex<Option<Vec<f32>>>,
    /// Notch adaptativo ON (mide el par real y re-afina en caliente).
    notch_auto: std::sync::atomic::AtomicBool,
}

impl AudioCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_rate: u32,
        channels: usize,
        block_size: u32,
        buffer_size: f32,
        latency_buffer: f32,
        device: Option<usize>,
        low_latency_mode: bool,
    ) -> Self {
        let buffer_size_samples = (buffer_size * sample_rate as f32) as usize;
        Self {
            sample_rate,
            channels,
            block_size,
            buffer_size,
            low_latency_mode,
            enable_playback: true,
            device_index: device,
            latency_buffer: Arc::new(AtomicF32::new(latency_buffer)),
            ring: Arc::new(Mutex::new(Ring::new(buffer_size_samples, channels))),
            buffer_size_samples,
            total_captured: Arc::new(AtomicU64::new(0)),
            total_played: AtomicU64::new(0),
            start_time: Mutex::new(None),
            playback: Arc::new(Mutex::new(None)),
            volume: Mutex::new(0.2),
            cushion_base_ms: AtomicU64::new(0),
            notch_hum: std::sync::atomic::AtomicBool::new(false),
            hpf_hz: AtomicU32::new(0),
            notch_override: std::sync::Mutex::new(None),
            notch_auto: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Fija el colchón base del playback (--audio-cushion-ms).
    pub fn set_cushion_base_ms(&self, ms: u64) {
        self.cushion_base_ms.store(ms.clamp(20, 500), Ordering::Relaxed);
    }

    /// Activa el notch del hum (--notch-hum).
    pub fn set_notch_hum(&self, on: bool) {
        self.notch_hum.store(on, Ordering::Relaxed);
    }

    /// Override de frecuencias de notch (--notch-hum-at "59.5,60.5"):
    /// el acople video→audio del chip SE MUDA de frecuencia al cambiar
    /// de modo (29/10: 60.06 solo; 14/11 en 1080p60 nativo: par
    /// 59.5/60.5). Re-medir con tools/audio_diag.py y fijar acá.
    pub fn set_notch_override(&self, freqs: Vec<f32>) {
        *self.notch_override.lock().unwrap() = Some(freqs);
    }

    /// Notch adaptativo on/off (--no-notch-auto).
    pub fn set_notch_auto(&self, on: bool) {
        self.notch_auto.store(on, Ordering::Relaxed);
    }

    /// High-pass opcional (--hpf, 0 = off).
    pub fn set_hpf(&self, hz: u32) {
        self.hpf_hz.store(hz, Ordering::Relaxed);
    }

    pub fn start(&self) -> Result<AudioStreams> {
        let device = devices::resolve_input_device(self.device_index)?;
        if self.device_index.is_some() {
            println!(
                "Using audio device: {}",
                device.name().unwrap_or_else(|_| "Unknown".into())
            );
        }
        // Auditado 29/10 (port del fix 25/10 de passthrough.rs): la interfaz
        // de audio de la capturadora corre a su PROPIA tasa (48000Hz) y
        // renegocia tras cambios de modo USB. Forzar 44100 =
        // "configuration not supported". Adoptamos la tasa REAL.
        let default_cfg = device
            .default_input_config()
            .context("no se pudo leer la config del dispositivo de entrada")?;
        let sample_rate = default_cfg.sample_rate().0;
        if sample_rate != self.sample_rate {
            println!(
                "⚠ El dispositivo corre a {sample_rate}Hz (la app pedía {}Hz) — adaptando",
                self.sample_rate
            );
        }

        // Latencia mínima (29/10): el bloque de entrada define el piso del
        // colchón (el consumidor sobrevive entre bloque y bloque). Pedimos
        // bloques CHICOS con fallback: 256 → 512 → 1024 → Default.
        // (El problema histórico era la TASA forzada, no el tamaño.)
        let mut input_block: Option<(u32, bool)> = None;
        let mut last_err = None;
        for (bs, is_fixed) in [(256u32, true), (512, true), (1024, true)] {
            let cfg = StreamConfig {
                channels: self.channels as u16,
                sample_rate: SampleRate(sample_rate),
                buffer_size: BufferSize::Fixed(bs),
            };
            match device.build_input_stream(&cfg, |_d: &[f32], _i: &cpal::InputCallbackInfo| {}, |e| eprintln!("probe: {e}"), None) {
                Ok(s) => {
                    drop(s);
                    input_block = Some((bs, is_fixed));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let (input_bs, input_fixed) = input_block.unwrap_or((0, false));
        if input_block.is_none() {
            if let Some(e) = last_err {
                println!("⚠ Fixed buffers rechazados ({e}) — usando Default");
            }
        }
        let config = StreamConfig {
            channels: self.channels as u16,
            sample_rate: SampleRate(sample_rate),
            buffer_size: if input_fixed { BufferSize::Fixed(input_bs) } else { BufferSize::Default },
        };
        // target del colchón: ~2.2 bloques de entrada, mínimo 50ms; o el
        // fijado por --audio-cushion-ms. El colchón ADAPTATIVO puede
        // elevarlo después si el device dropea audio (ver abajo).
        let input_block_ms = if input_fixed {
            input_bs as f32 * 1000.0 / sample_rate as f32
        } else {
            85.0 // Default típico de CoreAudio (~4096 frames @48k)
        };
        let base_ms = self.cushion_base_ms.load(Ordering::Relaxed);
        let auto_ms = (input_block_ms * 2.2).max(50.0) as u64;
        let cushion_ms = Arc::new(AtomicU64::new(if base_ms > 0 { base_ms } else { auto_ms }));
        let target_ms = cushion_ms.load(Ordering::Relaxed) as f32;

        let ring = self.ring.clone();
        let total = self.total_captured.clone();
        let playback_slot = self.playback.clone();
        let channels = self.channels;
        let capacity = self.buffer_size_samples;
        let last_input_ms = Arc::new(AtomicU64::new(0));
        let last_input_ms_wd = last_input_ms.clone();
        let cushion_in = cushion_ms.clone();
        // Grabador de diagnóstico: SCALAPP_REC=/path/dump.f32 graba el
        // audio CAPTURADO (pre-playback) como f32 LE interleaved. Solo
        // para análisis (escritura en callback RT = no productivo).
        let rec_file = std::env::var("SCALAPP_REC").ok().map(|path| {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap_or_else(|e| panic!("SCALAPP_REC {path}: {e}"));
            println!("[rec] grabando captura cruda a {path}");
            Arc::new(Mutex::new(f))
        });

        // Constructor extraído: el watchdog reconstruye la entrada con los
        // MISMOS Arcs. `move`, dueño de sus capturas; vive en main thread
        // (cpal Device no es Send).
        let device_for_builder = device.clone();
        let config_for_builder = config.clone();
        // Tracker del hum (notch adaptativo): buffer mono de la entrada
        let hum_buf = Arc::new(Mutex::new(HumTrackerBuf::new(sample_rate)));
        let hum_buf_in = hum_buf.clone();
        // Diagnóstico (30/10): gap entre callbacks de entrada (stall del
        // dispositivo) y cambios de tamaño de bloque (renegociación).
        let last_block_len = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let last_block_len_in = last_block_len.clone();
        let cushion_gap = cushion_in.clone();
        let rec_in = rec_file.clone();
        let t0_input = Instant::now();
        let make_input: Box<dyn Fn() -> Result<Stream>> = Box::new(move || {
            let ring = ring.clone();
            let total = total.clone();
            let playback_slot = playback_slot.clone();
            let last_input_ms = last_input_ms.clone();
            let last_block_len = last_block_len_in.clone();
            let cushion_gap = cushion_gap.clone();
            let rec_cb = rec_in.clone();
            let hum_buf_cb = hum_buf_in.clone();
            let config = config_for_builder.clone();
            device_for_builder
                .build_input_stream(
                    &config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let now = now_ms();
                        let prev = last_input_ms.swap(now, Ordering::Relaxed);
                        if prev != 0 && now.saturating_sub(prev) > 15 {
                            let gap_ms = now - prev;
                            println!(
                                "[audio t={:.0}s @{}] INPUT GAP: {gap_ms}ms desde el callback anterior",
                                t0_input.elapsed().as_secs_f64(),
                                wall_clock()
                            );
                            // Colchón adaptativo: el gap observado define el
                            // colchón necesario (gap + 50ms de margen, tope
                            // 350ms). Monotónico: en una sesión solo sube.
                            let cur = cushion_gap.load(Ordering::Relaxed);
                            let needed = (gap_ms + 50).min(350);
                            if needed > cur {
                                cushion_gap.store(needed, Ordering::Relaxed);
                                println!(
                                    "[audio t={:.0}s @{}] colchón adaptativo: {cur}ms → {needed}ms (gap {gap_ms}ms)",
                                    t0_input.elapsed().as_secs_f64(),
                                    wall_clock()
                                );
                            }
                        }
                        let prev_len = last_block_len.swap(data.len(), Ordering::Relaxed);
                        if prev_len != 0 && prev_len != data.len() {
                            println!(
                                "[audio t={:.0}s @{}] bloque de entrada cambió: {prev_len} -> {} samples (¿renegociación?)",
                                t0_input.elapsed().as_secs_f64(),
                                wall_clock(),
                                data.len()
                            );
                        }
                        let frames = data.len() / channels;
                        if let Some(rec) = &rec_cb {
                            use std::io::Write;
                            let bytes: Vec<u8> =
                                data.iter().flat_map(|s| s.to_le_bytes()).collect();
                            let _ = rec.lock().unwrap().write_all(&bytes);
                        }
                        write_ring(&mut ring.lock().unwrap(), data, channels, capacity);
                        // feed del tracker (lock corto: append de ~5ms)
                        hum_buf_cb.lock().unwrap().feed(data);
                        total.fetch_add(frames as u64, Ordering::Relaxed);
                        if let Some(p) = playback_slot.lock().unwrap().as_ref() {
                            p.add_audio_data(data.to_vec());
                        }
                    },
                    |err| eprintln!("Audio callback status: {err}"),
                    None,
                )
                .context("failed to build audio input stream")
        });

        let stream = make_input()?;
        stream.play().context("failed to start audio input stream")?;
        *self.start_time.lock().unwrap() = Some(Instant::now());
        let input_desc = if input_fixed {
            format!("Fixed({input_bs}) ~{input_block_ms:.1}ms")
        } else {
            "Default (CoreAudio)".to_string()
        };
        println!(
            "Audio capture started: {sample_rate}Hz, {} channels, bloque de entrada: {input_desc}",
            self.channels
        );

        let mut playback_output = None;
        // notch adaptativo: stop/handle del tracker
        let mut hum_stop = None;
        let mut hum_handle = None;
        if self.enable_playback {
            let playback = Arc::new(AudioPlayback::new(self.channels, self.low_latency_mode));
            let volume = *self.volume.lock().unwrap();
            playback.set_volume(scale_volume(volume));
            // bloque de salida pequeño también: aporta un bloque de latencia
            let out_bs = self.block_size.min(512);
            // OJO: el clone ANTES del match — un guard temporario en el
            // scrutinee del match vive TODO el cuerpo del match, y
            // re-lockear notch_override adentro (condición del notch
            // adaptativo) se autodeadlockeaba (hallado con sample+atos).
            let notch_override_freqs = self.notch_override.lock().unwrap().clone();
            match playback.start(
                sample_rate,
                out_bs,
                cushion_ms.clone(),
                self.notch_hum.load(Ordering::Relaxed),
                self.hpf_hz.load(Ordering::Relaxed),
                notch_override_freqs,
            ) {
                Ok(out_stream) => {
                    *self.playback.lock().unwrap() = Some(playback.clone());
                    playback_output = Some(out_stream);
                    // Notch ADAPTATIVO: el par de acople deriva ±0.15Hz
                    // entre sesiones — midámoslo en vivo y re-afinemos.
                    if self.notch_auto.load(Ordering::Relaxed)
                        && self.notch_hum.load(Ordering::Relaxed)
                        && self.notch_override.lock().unwrap().is_none()
                    {
                        let stop_f = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let st = stop_f.clone();
                        let hb = hum_buf.clone();
                        let pb = playback.clone();
                        let fs = f64::from(sample_rate);
                        if let Ok(h) = std::thread::Builder::new()
                            .name("hum-auto".into())
                            .spawn(move || hum_auto_loop(hb, pb, st, fs))
                        {
                            hum_stop = Some(stop_f);
                            hum_handle = Some(h);
                            println!("✓ Notch adaptativo activo (mide el par real y re-afina en caliente)");
                        }
                    }
                    println!("✓ Audio playback/monitoring enabled (target ~{target_ms:.0}ms)");
                }
                Err(e) => {
                    println!("⚠ Could not start audio playback: {e:#}");
                    println!("  Video will work but you won't hear audio");
                }
            }
        }

        // Watchdog (parche de negociación): solo atómicos cruzan threads;
        // el renderer atiende la bandera via poll_rebuild().
        let mut rebuild_flag = None;
        let mut wd_stop = None;
        let mut wd_handle = None;
        let input_holder: Arc<Mutex<Option<Stream>>> = Arc::new(Mutex::new(Some(stream)));
        {
            let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_c = stop_flag.clone();
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag_c = flag.clone();
            let last_wd = last_input_ms_wd.clone();
            let stall_ms: u64 = 2000;
            let handle = std::thread::Builder::new()
                .name("av-audio-watchdog".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        if stop_c.load(Ordering::Relaxed) {
                            return;
                        }
                        let last = last_wd.load(Ordering::Relaxed);
                        if last == 0 {
                            continue;
                        }
                        let now = now_ms();
                        if now.saturating_sub(last) > stall_ms
                            && !flag_c.swap(true, Ordering::Relaxed)
                        {
                            println!("[watchdog] entrada de audio estancada {}ms — pidiendo rebuild", now - last);
                        }
                    }
                })
                .ok();
            if handle.is_some() {
                wd_handle = handle;
                wd_stop = Some(stop_flag);
                rebuild_flag = Some(flag);
                println!("✓ Watchdog de audio activo (stall > {stall_ms}ms → rebuild)");
            }
        }

        Ok(AudioStreams {
            input: input_holder,
            playback_output,
            rebuild_flag,
            rebuild_next_ms: Arc::new(AtomicU64::new(0)),
            rebuild_fn: Some(make_input),
            wd_stop,
            wd_handle,
            hum_stop,
            hum_handle,
        })
    }

    pub fn stop(&self) {
        if let Some(playback) = self.playback.lock().unwrap().take() {
            drop(playback);
        }
        println!("Audio capture stopped");
    }

    pub fn get_audio_time(&self) -> f64 {
        let start = self.start_time.lock().unwrap();
        let Some(start) = *start else { return 0.0 };
        let elapsed = start.elapsed().as_secs_f64();
        let offset = if self.low_latency_mode {
            0.005
        } else {
            self.latency_buffer.get() as f64
        };
        (elapsed - offset).max(0.0)
    }

    pub fn get_available_samples(&self) -> usize {
        self.ring
            .lock()
            .unwrap()
            .available(self.buffer_size_samples)
    }

    pub fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        let scaled = scale_volume(volume);
        *self.volume.lock().unwrap() = volume;
        if let Some(playback) = self.playback.lock().unwrap().as_ref() {
            playback.set_volume(scaled);
        }
        println!(
            "Volume: {}% (actual: {}%)",
            (volume * 100.0) as i32,
            (scaled * 100.0) as i32
        );
    }

    pub fn set_latency_buffer(&self, latency: f64) {
        let latency = latency.clamp(0.01, 2.0);
        self.latency_buffer.set(latency as f32);
        let mode = if latency <= 0.03 {
            "ultra-low"
        } else if latency <= 0.05 {
            "low"
        } else {
            "normal"
        };
        println!(
            "Audio latency set to: {:.0}ms ({mode}-latency mode)",
            latency * 1000.0
        );
    }

    #[allow(dead_code)]
    pub fn read_samples(&self, num_samples: usize) -> Option<Vec<f32>> {
        let mut ring = self.ring.lock().unwrap();
        if ring.available(self.buffer_size_samples) < num_samples {
            return None;
        }
        let end = ring.read + num_samples;
        let mut data = vec![0.0f32; num_samples * self.channels];
        if end <= self.buffer_size_samples {
            data.copy_from_slice(&ring.buf[ring.read * self.channels..end * self.channels]);
        } else {
            let first = self.buffer_size_samples - ring.read;
            data[..first * self.channels]
                .copy_from_slice(&ring.buf[ring.read * self.channels..]);
            data[first * self.channels..]
                .copy_from_slice(&ring.buf[..(end - self.buffer_size_samples) * self.channels]);
        }
        ring.read = end % self.buffer_size_samples;
        self.total_played
            .fetch_add(num_samples as u64, Ordering::Relaxed);
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn volume_user_scale_maps_to_physical_range() {
        // 0-100 de la UI recorre 0-30% físico completo
        assert!((user_volume_to_actual(0.0)).abs() < 1e-6);
        assert!((user_volume_to_actual(1.0) - VOLUME_RANGE).abs() < 1e-6);
        assert!((user_volume_to_actual(0.5) - 0.15).abs() < 1e-6);
        // roundtrip: el default 20% físico se expresa como 2/3 de la UI
        assert!((actual_volume_to_user(0.2) - 0.2 / VOLUME_RANGE).abs() < 1e-6);
        assert!(user_volume_to_actual(actual_volume_to_user(0.25)) - 0.25 < 1e-6);
    }

    #[test]
    fn volume_scaling_matches_python() {
        assert!(approx(scale_volume(0.0), 0.0));
        assert!(approx(scale_volume(0.3), 0.3f32.powf(1.2) * 1.25));
        assert!(approx(scale_volume(0.5), 0.5f32.powf(1.2) * 1.25));
        assert!(approx(scale_volume(0.8), 0.8f32.powf(1.2) * 1.25));
        assert!(approx(scale_volume(0.9), 0.9));
        assert!(approx(scale_volume(1.0), 1.0));
        assert!(approx(scale_volume(1.5), 1.0));
    }

    #[test]
    fn ring_write_wraps() {
        let mut ring = Ring::new(4, 1);
        write_ring(&mut ring, &[1.0, 2.0], 1, 4);
        assert_eq!(ring.write, 2);
        write_ring(&mut ring, &[3.0, 4.0, 5.0, 6.0], 1, 4);
        assert_eq!(ring.write, 2);
        assert_eq!(ring.buf, vec![5.0, 6.0, 3.0, 4.0]);
        assert_eq!(ring.available(4), 2);
    }

    #[test]
    fn ring_read_advances_available() {
        let mut ring = Ring::new(4, 1);
        write_ring(&mut ring, &[1.0, 2.0, 3.0], 1, 4);
        ring.read = 1;
        assert_eq!(ring.available(4), 2);
    }

    #[test]
    fn hum_pair_detector_encuentra_el_par_sintetico() {
        // 4s @48k: par 59.66/60.34 (batido 0.68Hz) + ruido + contenido
        let fs = 48000.0f64;
        let n = fs as usize * 4;
        let mut buf = Vec::with_capacity(n);
        for t in 0..n {
            let tt = t as f64 / fs;
            let v = 0.02 * (2.0 * std::f64::consts::PI * 59.66 * tt).sin()
                + 0.015 * (2.0 * std::f64::consts::PI * 60.34 * tt).sin()
                + 0.05 * (2.0 * std::f64::consts::PI * 44.0 * tt).sin() // bajo del juego
                + 0.002 * (tt * 48000.0 * 0.37).sin(); // ruido determinista
            buf.push(v as f32);
        }
        let (f1, f2) = detect_hum_pair(&buf, fs).expect("debe detectar el par");
        assert!((f1 - 59.66).abs() < 0.05, "f1={f1}");
        assert!((f2 - 60.34).abs() < 0.05, "f2={f2}");
    }

    #[test]
    fn sub_pair_detector_encuentra_el_par_de_30() {
        // el leak del 18/11: 29.77/30.72 (NO simétrico alrededor de 30)
        let fs = 48000.0f64;
        let n = fs as usize * 4;
        let mut buf = Vec::with_capacity(n);
        for t in 0..n {
            let tt = t as f64 / fs;
            let v = 0.012 * (2.0 * std::f64::consts::PI * 29.77 * tt).sin()
                + 0.010 * (2.0 * std::f64::consts::PI * 30.72 * tt).sin()
                + 0.05 * (2.0 * std::f64::consts::PI * 44.0 * tt).sin()
                + 0.002 * (tt * 48000.0 * 0.61).sin();
            buf.push(v as f32);
        }
        let (s1, s2) = detect_sub_pair(&buf, fs).expect("debe detectar el par sub");
        assert!((s1 - 29.77).abs() < 0.05, "s1={s1}");
        assert!((s2 - 30.72).abs() < 0.05, "s2={s2}");
    }

    #[test]
    fn hum_pair_detector_rechaza_tono_solo() {
        // un solo tono (sin par simétrico) = no actualizar
        let fs = 48000.0f64;
        let n = fs as usize * 4;
        let mut buf = Vec::with_capacity(n);
        for t in 0..n {
            let tt = t as f64 / fs;
            buf.push((0.05 * (2.0 * std::f64::consts::PI * 60.4 * tt).sin()) as f32);
        }
        assert!(detect_hum_pair(&buf, fs).is_none());
    }

    #[test]
    fn notch_update_auto_caza_el_par_movil() {
        // el par DERIVA entre sesiones (59.55/60.45 → 59.66/60.34...):
        // update_auto re-afina los slots y el par medido debe morir
        // SIN tocar Bb1/B1 — y ahora con CASCADA ×2 (más profundidad)
        // y el par SUB de ~30Hz cubierto.
        let fs = 48000u32;
        let mut bank = NotchBank::hum_notches(fs);
        bank.update_auto(59.66, 60.34, 29.77, 30.72);
        let n = fs as usize * 6; // 6s: los notches Q=100 a ~30Hz tardan ~1.1s en asentarse
        let sine = |f: f32, t: usize| (2.0 * std::f32::consts::PI * f * t as f32 / fs as f32).sin();
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let mut b = bank.clone_bank_for_test();
        let atten = |bank: &mut NotchBank, f: f32| {
            let mut x: Vec<f32> = (0..n).flat_map(|t| {
                let s = sine(f, t);
                [s, s]
            })
            .collect();
            let rin = rms(&x);
            bank.process_interleaved(&mut x);
            20.0 * (rin / rms(&x[x.len() * 5 / 6..]).max(1e-12)).log10()
        };
        // par de 60 + armónicos: cascada profunda
        for f0 in [59.66f32, 60.34, 119.32, 120.68] {
            let a = atten(&mut b, f0);
            assert!(a > 35.0, "par 60/arm {f0}Hz: solo {a:.1}dB (cascada)");
        }
        // par sub: a ~30Hz el asentamiento de la cascada es lento (τ≈1.1s
        // ×2 polos) — el test mide transitorio+régimen; >24dB honrado
        for f0 in [29.77f32, 30.72] {
            let a = atten(&mut b, f0);
            assert!(a > 24.0, "par sub {f0}Hz: solo {a:.1}dB (cascada)");
        }
        for (f, nota) in [(58.27, "Bb1"), (61.74, "B1"), (55.0, "A1"), (41.2, "E1")] {
            let a = atten(&mut b, f);
            assert!(a < 4.0, "{nota} ({f}Hz) dañado: {a:.1}dB");
        }
    }

    #[test]
    fn notch_is_surgical_for_game_audio() {
        // el hum medido: tono a 60.06 Hz con armónicos. El notch debe
        // matarlos SIN tocar notas reales del contenido.
        let fs = 48000u32;
        let mut bank = NotchBank::hum_notches(fs);
        let n = fs as usize * 3; // 3s: los notches Q=100 tardan ~0.5s en asentarse
        let sine = |f: f32, t: usize| (2.0 * std::f32::consts::PI * f * t as f32 / fs as f32).sin();
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        // buffer INTERLEAVED estéreo (canal L y R con la misma señal):
        // process_interleaved trata pares consecutivos como (L, R)
        let atten_db = |bank: &mut NotchBank, f: f32| {
            let mut x: Vec<f32> = (0..n).flat_map(|t| {
                let s = sine(f, t);
                [s, s]
            })
            .collect();
            let rin = rms(&x);
            bank.process_interleaved(&mut x);
            20.0 * (rin / rms(&x[x.len() / 2..]).max(1e-12)).log10()
        };
        // el hum muere
        for f0 in [60.06f32, 120.12, 180.18] {
            let a = atten_db(&mut bank, f0);
            assert!(a > 20.0, "hum {f0}Hz: solo {a:.1}dB");
        }
        // el PAR del batido (thump 14/11, 1080p60 nativo): dos tonos
        // ~1Hz aparte batiendo = "tum tum" — ambos deben caer solos
        for f0 in [59.5f32, 60.5, 119.0, 121.0] {
            let a = atten_db(&mut bank, f0);
            assert!(a > 20.0, "par de batido {f0}Hz: solo {a:.1}dB");
        }
        // ...y el contenido del juego sobrevive (<3dB)
        for (f, nota) in [
            (41.2, "E1 sub-bajo"),
            (55.0, "A1"),
            (58.3, "Bb1 (borde inferior del par)"),
            (61.7, "B1 (borde superior del par)"),
            (82.4, "E2"),
            (123.5, "B2 (pegado al notch de 120)"),
            (185.0, "F#3 (pegado al notch de 180)"),
            (200.0, "200Hz"),
            (440.0, "A4"),
        ] {
            let a = atten_db(&mut bank, f);
            assert!(a < 4.0, "{nota} ({f}Hz) dañado: {a:.1}dB");
        }
    }

    #[test]
    fn optional_hpf_kills_rumble_not_content_above_it() {
        let fs = 48000u32;
        let mut bank = NotchBank::hum_notches(fs).with_highpass(48.0);
        let n = fs as usize;
        let sine = |f: f32, t: usize| (2.0 * std::f32::consts::PI * f * t as f32 / fs as f32).sin();
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let atten_db = |bank: &mut NotchBank, f: f32| {
            let mut x: Vec<f32> = (0..n)
                .flat_map(|t| {
                    let s = sine(f, t);
                    [s, s]
                })
                .collect();
            let rin = rms(&x);
            bank.process_interleaved(&mut x);
            20.0 * (rin / rms(&x[x.len() / 2..]).max(1e-12)).log10()
        };
        // el rumor del chip (28-38Hz) muere
        let a30 = atten_db(&mut bank, 30.0);
        let a35 = atten_db(&mut bank, 35.0);
        eprintln!("[hpf-test] 30Hz: {a30:.1}dB | 35Hz: {a35:.1}dB");
        assert!(a30 > 12.0, "30Hz: {a30:.1}dB");
        assert!(a35 > 10.0, "35Hz: {a35:.1}dB"); // diseño: -13.1dB
        // el contenido sobre el HP sobrevive
        assert!(atten_db(&mut bank, 82.4) < 3.0); // E2
        assert!(atten_db(&mut bank, 200.0) < 1.0);
    }
}
