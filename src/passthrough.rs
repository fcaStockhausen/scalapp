use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::atomicf32::AtomicF32;
use crate::devices;

#[allow(dead_code)]
pub struct PassthroughStats {
    pub blocks_processed: u64,
    pub block_size: u32,
    pub sample_rate: u32,
    pub theoretical_latency_ms: f32,
    pub estimated_total_latency_ms: f32,
    pub volume_processing: bool,
    pub buffer_size: usize,
}

pub struct PassthroughStreams {
    /// Dueño del stream de entrada. Solo el MAIN thread toca este mutex
    /// (cpal Device/Stream no son Send); el watchdog solo levanta flags.
    input: Arc<Mutex<Option<Stream>>>,
    output: Option<Stream>,
    virtual_output: Option<Stream>,
    blocks_processed: Arc<AtomicU64>,
    watchdog_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    watchdog_handle: Option<std::thread::JoinHandle<()>>,
    /// El watchdog levanta esto cuando la entrada se estanca; el main
    /// thread lo atiende en poll_rebuild().
    rebuild_needed: Option<Arc<std::sync::atomic::AtomicBool>>,
    rebuild_next_ms: Arc<AtomicU64>,
    make_input: Option<Box<dyn Fn() -> Result<Stream>>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl PassthroughStreams {
    pub fn stop(&mut self) {
        if let Some(flag) = self.watchdog_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }
        drop(self.input.lock().unwrap().take());
        drop(self.output.take());
        drop(self.virtual_output.take());
        println!("✓ Ultra-low latency passthrough V4 stopped");
        println!(
            "  Blocks processed: {}",
            self.blocks_processed.load(Ordering::Relaxed)
        );
    }

    /// Atiende pedidos de rebuild del watchdog. Llamar periódicamente
    /// desde el main thread (ej: loop de consola cada 200ms).
    pub fn poll_rebuild(&mut self) {
        let flag = match &self.rebuild_needed {
            Some(f) => f,
            None => return,
        };
        if !flag.load(Ordering::Relaxed) {
            return;
        }
        let now = now_ms();
        if now < self.rebuild_next_ms.load(Ordering::Relaxed) {
            return; // cooldown: reintento más tarde
        }
        flag.store(false, Ordering::Relaxed);
        self.rebuild_next_ms.store(now + 2000, Ordering::Relaxed);
        let make = match &self.make_input {
            Some(m) => m,
            None => return,
        };
        println!("[watchdog] reconstruyendo stream de entrada...");
        let old = self.input.lock().unwrap().take();
        drop(old);
        match make().and_then(|s| {
            s.play().context("failed to restart input stream")?;
            Ok(s)
        }) {
            Ok(s) => {
                *self.input.lock().unwrap() = Some(s);
                println!("[watchdog] entrada reconstruida ✓");
            }
            Err(e) => {
                eprintln!("[watchdog] fallo rebuild: {e:#} — reintento en 2s");
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

struct PassthroughV4 {
    sample_rate: u32,
    channels: usize,
    block_size: u32,
    enable_volume: bool,
    virtual_out_name: Option<String>,
    /// Colchón deliberado en ms (--delay-ms): absorbe stalls de la
    /// entrada cuando otro cliente (Discord) renegocia el dispositivo.
    /// 0 = comportamiento legacy (~8 bloques).
    delay_ms: u32,
    /// Watchdog: si el callback de entrada no llega en stall_ms, se
    /// reconstruye el stream de entrada (revive tras la muerte por
    /// doble cliente). 0 = watchdog desactivado.
    stall_ms: u64,
    /// Notch del hum del chip (--notch-hum): 60/120/180 Hz + HP opcional
    /// (--hpf) para el rumor 28-38 Hz.
    notch_hum: bool,
    hpf_hz: u32,
    buffer: Arc<Mutex<VecDeque<Vec<f32>>>>,
    virtual_buffer: Arc<Mutex<VecDeque<Vec<f32>>>>,
    volume: Arc<AtomicF32>,
    blocks_processed: Arc<AtomicU64>,
}

impl PassthroughV4 {
    fn new(sample_rate: u32, channels: usize, block_size: u32, volume: f32, enable_volume: bool) -> Self {
        Self {
            sample_rate,
            channels,
            block_size,
            enable_volume,
            virtual_out_name: None,
            delay_ms: 0,
            stall_ms: 0,
            notch_hum: false,
            hpf_hz: 0,
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            virtual_buffer: Arc::new(Mutex::new(VecDeque::new())),
            volume: Arc::new(AtomicF32::new(volume)),
            blocks_processed: Arc::new(AtomicU64::new(0)),
        }
    }

    fn with_virtual_out(mut self, name: Option<String>) -> Self {
        self.virtual_out_name = name;
        self
    }

    fn with_cushion(mut self, delay_ms: u32, stall_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self.stall_ms = stall_ms;
        self
    }

    fn with_notch_hum(mut self, on: bool) -> Self {
        self.notch_hum = on;
        self
    }

    fn with_hpf(mut self, hz: u32) -> Self {
        self.hpf_hz = hz;
        self
    }

    fn start(&self, input_device: Option<usize>) -> Result<PassthroughStreams> {
        let total_latency_ms = (self.block_size as f32 / self.sample_rate as f32) * 1000.0;
        println!("Starting Ultra-Low Latency Passthrough V4 (Low Latency Clone)...");
        println!("  Input device: {input_device:?}");
        println!("  Output device: default");
        println!("  Sample rate: {}Hz", self.sample_rate);
        println!(
            "  Block size: {} samples (~{total_latency_ms:.2}ms)",
            self.block_size
        );
        println!(
            "  Volume processing: {}",
            if self.enable_volume { "enabled" } else { "DISABLED" }
        );
        println!("  Theoretical min latency: ~{total_latency_ms:.2}ms");
        println!("  Stream mode: Separate streams (like low-latency mode)");

        let input = devices::resolve_input_device(input_device)?;
        let output = devices::resolve_output_device(None)?;

        // Auditado 25/10: la interfaz de audio de la capturadora puede
        // renegociar su tasa (ej. 44100 -> 48000 tras cambios de modo USB).
        // Forzar una tasa no soportada = "configuration not supported".
        // Solucion: adoptar la config REAL del dispositivo (tasa por
        // defecto) y buffer Default (CoreAudio elige el optimo).
        let default_cfg = input
            .default_input_config()
            .context("no se pudo leer la config del dispositivo de entrada")?;
        let sample_rate = default_cfg.sample_rate().0;
        if sample_rate != self.sample_rate {
            println!(
                "⚠ El dispositivo corre a {sample_rate}Hz (la app pedia {}Hz) — adaptando",
                self.sample_rate
            );
        }
        let config = StreamConfig {
            channels: self.channels as u16,
            sample_rate: default_cfg.sample_rate(),
            buffer_size: BufferSize::Default,
        };

        let buffer = self.buffer.clone();
        let virtual_buffer = self.virtual_buffer.clone();
        let virtual_enabled = self.virtual_out_name.is_some();
        let volume = self.volume.clone();
        let enable_volume = self.enable_volume;
        let channels_dbg = self.channels;
        let first_in = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let first_in_c = first_in.clone();
        // Bug A (27/10): colchon dinamico + marca de vida de la entrada.
        let delay_ms = self.delay_ms;
        let target_fill = Arc::new(std::sync::atomic::AtomicUsize::new(8));
        let target_fill_in = target_fill.clone();
        let last_input_ms = Arc::new(AtomicU64::new(0));
        let last_input_ms_in = last_input_ms.clone();
        let sample_rate_for_target = default_cfg.sample_rate().0;
        // Grabador de diagnóstico: SCALAPP_REC=/path/dump.f32 graba el
        // audio CAPTURADO (pre-playback) como f32 LE interleaved.
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

        // Constructor extraido: el watchdog reconstruye la entrada con los
        // MISMOS Arcs (misma cola, mismo volumen) cuando Discord la mata.
        // Es `move` y dueño de sus capturas (Arcs + config clonada): se
        // puede clonar a un thread y llamar N veces.
        let config_for_builder = config.clone();
        let make_input_stream = move |input: &cpal::Device| -> Result<Stream> {
            let buffer = buffer.clone();
            let virtual_buffer = virtual_buffer.clone();
            let volume = volume.clone();
            let first_in_c = first_in_c.clone();
            let target_fill_in = target_fill_in.clone();
            let last_input_ms_in = last_input_ms_in.clone();
            let rec_cb = rec_file.clone();
            let config = config_for_builder.clone();
            input
                .build_input_stream(
                    &config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Some(rec) = &rec_cb {
                            use std::io::Write;
                            let bytes: Vec<u8> =
                                data.iter().flat_map(|s| s.to_le_bytes()).collect();
                            let _ = rec.lock().unwrap().write_all(&bytes);
                        }
                        if first_in_c.swap(false, Ordering::Relaxed) {
                            println!("[diag] bloque ENTRADA: {} frames ({:.1}ms)",
                                     data.len() / channels_dbg,
                                     data.len() as f32 / channels_dbg as f32 / 48.0);
                        }
                        // marca de vida + colchon dinamico (una sola vez)
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let prev = last_input_ms_in.swap(now_ms, Ordering::Relaxed);
                        if prev > 0 && now_ms.saturating_sub(prev) > 300 {
                            println!("[watchdog] gap de entrada {}ms — data reanuda (posible bloque corrupto)",
                                     now_ms - prev);
                        }
                        if delay_ms > 0 {
                            let frames = (data.len() / channels_dbg).max(1);
                            let blocks_needed = ((delay_ms as u64 * sample_rate_for_target as u64)
                                .div_ceil(1000) / frames as u64) as usize;
                            let want = blocks_needed.max(8);
                            if target_fill_in.load(Ordering::Relaxed) < want {
                                target_fill_in.store(want, Ordering::Relaxed);
                            }
                        }
                        let mut data = data.to_vec();
                        if enable_volume {
                            let v = volume.get();
                            for s in &mut data {
                                *s *= v;
                            }
                        }
                        let mut buf = buffer.lock().unwrap();
                        // Auditado 24/10: el tope de 3 bloques (~17ms) cortaba el
                        // audio — los relojes de captura y salida derivan siempre
                        // (cristales distintos) y al llenarse el tope se borraba
                        // un bloque = corte audible. Nuevo: sin tope agresivo;
                        // catch-up solo >30 bloques (callback dormido >~170ms)
                        // para no acumular latencia infinita.
                        // Con colchon (bug A): el tope crece con el target.
                        let cap = target_fill_in.load(Ordering::Relaxed) * 2 + 14;
                        while buf.len() > cap {
                            buf.pop_front();
                        }
                        // Router virtual (Discord lee por BlackHole): misma data,
                        // cola propia con catch-up generoso.
                        if virtual_enabled {
                            let mut vbuf = virtual_buffer.lock().unwrap();
                            while vbuf.len() > 60 {
                                vbuf.pop_front();
                            }
                            vbuf.push_back(data.clone());
                        }
                        buf.push_back(data);
                    },
                    |err| eprintln!("Input status: {err}"),
                    None,
                )
                .context("failed to build input stream")
        };

        let input_stream = make_input_stream(&input)?;

        let buffer = self.buffer.clone();
        let blocks = self.blocks_processed.clone();
        let target_fill_out = target_fill.clone();
        // Auditado 25/10 (3): la cola se estaciona en 0-1 porque la salida
        // consume ~0.0x% más rápido que la entrada produce (deriva de
        // cristales). Priming solo posterga el problema. Solución REAL:
        // compensación de deriva — resampling micro por interpolación lineal,
        // consumiendo a una tasa ajustada para mantener la cola en el
        // objetivo (8 bloques). Corrección máxima ±0.2% (inaudible).
        struct Drift {
            frac_pos: f32,   // posición fraccional dentro del bloque actual
            block: Vec<f32>, // bloque en consumo
            primed: bool,
        }
        let drift = Arc::new(Mutex::new(Drift {
            frac_pos: 0.0,
            block: Vec::new(),
            primed: false,
        }));
        let drift_out = drift.clone();
        // Notch del hum (--notch-hum): el refresco vertical del chip se
        // acopla al audio cuando streamea video (60/120/180 Hz).
        let notch_out = (self.notch_hum || self.hpf_hz > 0).then(|| {
            let mut bank = if self.notch_hum {
                crate::audio::NotchBank::hum_notches(sample_rate)
            } else {
                crate::audio::NotchBank::new(sample_rate, &[], 1.0)
            };
            if self.hpf_hz > 0 {
                bank = bank.with_highpass(self.hpf_hz as f32);
            }
            Arc::new(Mutex::new(bank))
        });
        let notch_cb = notch_out.clone();
        if self.notch_hum {
            println!("  Notch anti-hum: 60.06/120.12/180.18 Hz (Q=25)");
        }
        if self.hpf_hz > 0 {
            println!("  High-pass: {}Hz (atenúa el sub-bajo del contenido)", self.hpf_hz);
        }
        let output_stream = output
            .build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = buffer.lock().unwrap();
                    let mut d = drift_out.lock().unwrap();
                    {
                        static ONCE: std::sync::Once = std::sync::Once::new();
                        ONCE.call_once(|| println!("[diag] bloque SALIDA: {} frames", out.len() / 2));
                    }
                    let n_blk = blocks.fetch_add(1, Ordering::Relaxed) + 1;
                    if n_blk % 200 == 0 {
                        println!("[diag] cola={} bloques", buf.len());
                    }
                    // objetivo dinamico: 8 bloques legacy, o colchon --delay-ms
                    let target = target_fill_out.load(Ordering::Relaxed).max(1);
                    // arrancar solo con colchón
                    if !d.primed {
                        if buf.len() < target {
                            out.fill(0.0);
                            return;
                        }
                        d.primed = true;
                    }
                    // corrección de deriva: si hay exceso, consumir más rápido;
                    // si hay déficit, más lento (mantener ~target)
                    let err = buf.len() as f32 - target as f32;
                    let rate = 1.0 + (err / (target as f32 * 40.0)).clamp(-0.002, 0.002);
                    let channels = 2usize;
                    // recorrer el buffer de salida por CANAL (frame-wise)
                    let frames_out = out.len() / channels;
                    for fi in 0..frames_out {
                        // asegurar que hay bloque en consumo y UNO siguiente (para interp)
                        loop {
                            let need_next = d.block.len() > (d.frac_pos as usize + 2) * channels;
                            if need_next || buf.is_empty() && d.block.len() > (d.frac_pos as usize + 1) * channels {
                                break;
                            }
                            match buf.pop_front() {
                                Some(next) => {
                                    if d.block.is_empty() {
                                        d.block = next;
                                        d.frac_pos = 0.0;
                                    } else {
                                        // concatenar lógicamente: extendemos block con next
                                        d.block.extend_from_slice(&next);
                                    }
                                }
                                None => break,
                            }
                        }
                        let base = (d.frac_pos as usize) * channels;
                        if base + channels >= d.block.len() {
                            // sin más datos: silencio (hambre real)
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
                    // liberar memoria del bloque consumido
                    let consumed = (d.frac_pos as usize).saturating_sub(4) * channels;
                    if consumed > 0 && consumed < d.block.len() {
                        d.block.drain(..consumed);
                        d.frac_pos -= (consumed / channels) as f32;
                    }
                    if let Some(nb) = &notch_cb {
                        let mut bank = nb.lock().unwrap();
                        bank.process_interleaved(out);
                    }
                    blocks.fetch_add(1, Ordering::Relaxed);
                },
                |err| eprintln!("Output status: {err}"),
                None,
            )
            .context("failed to build output stream")?;

        input_stream.play().context("failed to start input stream")?;
        output_stream.play().context("failed to start output stream")?;

        // === Watchdog de entrada (bug A, 27/10) ===
        // Discord (u otro cliente) puede matar nuestro stream de entrada al
        // renegociar el dispositivo. El watchdog (solo atomics: cpal Device
        // no es Send) detecta el estancamiento y levanta rebuild_needed;
        // el MAIN thread ejecuta el rebuild via poll_rebuild().
        let mut watchdog_handle = None;
        let mut watchdog_stop = None;
        let mut rebuild_needed = None;
        let rebuild_next_ms = Arc::new(AtomicU64::new(0));
        let input_holder: Arc<Mutex<Option<Stream>>> =
            Arc::new(Mutex::new(Some(input_stream)));
        if self.stall_ms > 0 {
            let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_c = stop_flag.clone();
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag_c = flag.clone();
            let last_input_ms_wd = last_input_ms.clone();
            let stall_ms = self.stall_ms;
            let handle = std::thread::Builder::new()
                .name("input-watchdog".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        if stop_c.load(Ordering::Relaxed) {
                            return;
                        }
                        let last = last_input_ms_wd.load(Ordering::Relaxed);
                        if last == 0 {
                            continue; // aún no arrancó
                        }
                        let now = now_ms();
                        let stalled = now.saturating_sub(last);
                        if stalled > stall_ms {
                            if !flag_c.swap(true, Ordering::Relaxed) {
                                println!("[watchdog] entrada estancada {stalled}ms — pidiendo rebuild (¿Discord?)");
                            }
                        }
                    }
                })
                .ok();
            watchdog_handle = handle;
            watchdog_stop = Some(stop_flag);
            rebuild_needed = Some(flag);
            println!("✓ Watchdog de entrada activo (stall > {stall_ms}ms → rebuild)");
        }
        if self.delay_ms > 0 {
            println!("✓ Colchón deliberado: {}ms (absorbe renegociaciones)", self.delay_ms);
        }

        // Router virtual (Discord lee por BlackHole): segunda salida simple
        // con priming — el dispositivo virtual es tolerante pero ordenado.
        let mut virtual_output = None;
        if let Some(ref name) = self.virtual_out_name {
            match devices::resolve_output_device_by_name(name) {
                Ok(vdev) => {
                    let vbuf = self.virtual_buffer.clone();
                    let vprimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let vprimed_c = vprimed.clone();
                    let vstream = vdev
                        .build_output_stream(
                            &config,
                            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                let mut b = vbuf.lock().unwrap();
                                if !vprimed_c.swap(true, Ordering::Relaxed) && b.len() < 8 {
                                    out.fill(0.0);
                                    return;
                                }
                                let mut pos = 0;
                                while pos < out.len() {
                                    match b.pop_front() {
                                        Some(d) => {
                                            let n = (d.len()).min(out.len() - pos);
                                            out[pos..pos + n].copy_from_slice(&d[..n]);
                                            pos += n;
                                            if n < d.len() {
                                                b.push_front(d[n..].to_vec());
                                            }
                                        }
                                        None => {
                                            for s in &mut out[pos..] {
                                                *s = 0.0;
                                            }
                                            pos = out.len();
                                        }
                                    }
                                }
                                while b.len() > 60 {
                                    b.pop_front();
                                }
                            },
                            |err| eprintln!("Virtual output status: {err}"),
                            None,
                        )
                        .context("failed to build virtual output stream")?;
                    vstream.play().context("failed to start virtual output")?;
                    println!("✓ Salida virtual activa: \"{name}\" (configurala como entrada/mic de Discord)");
                    virtual_output = Some(vstream);
                }
                Err(e) => {
                    println!("⚠ Salida virtual no disponible: {e:#}");
                    println!("  (continuando sin router — Discord podria pelear el dispositivo)");
                }
            }
        }

        println!("✓ Ultra-low latency passthrough V4 started");
        println!(
            "✓ Expected total latency: ~{:.1}ms (including OS overhead)",
            total_latency_ms + 2.0
        );

        // Reconstrucción de entrada (main thread): device + builder quedan
        // guardados en PassthroughStreams para poll_rebuild().
        let rebuild_fn: Box<dyn Fn() -> Result<Stream>> = {
            let dev = input.clone();
            let mk = make_input_stream.clone();
            Box::new(move || mk(&dev))
        };

        Ok(PassthroughStreams {
            input: input_holder,
            output: Some(output_stream),
            virtual_output,
            blocks_processed: self.blocks_processed.clone(),
            watchdog_stop,
            watchdog_handle,
            rebuild_needed,
            rebuild_next_ms,
            make_input: Some(rebuild_fn),
        })
    }

    fn set_volume(&self, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        self.volume.set(v);
        // debug 25/10: verificar propagacion (el input callback lee self.volume)
        println!(
            "Volume set to: {}% (atomic={} | cola={})",
            (v * 100.0) as i32,
            self.volume.get(),
            self.buffer.lock().unwrap().len()
        );
    }
}

pub struct PassthroughWithVolume {
    passthrough: PassthroughV4,
    current_volume: f32,
    is_muted: bool,
    volume_before_mute: f32,
}

#[allow(dead_code)]
impl PassthroughWithVolume {
    pub fn new(sample_rate: u32, channels: usize, block_size: u32, initial_volume: f32) -> Self {
        Self {
            passthrough: PassthroughV4::new(sample_rate, channels, block_size, initial_volume, true),
            current_volume: initial_volume,
            is_muted: false,
            volume_before_mute: initial_volume,
        }
    }

    pub fn with_virtual_out(mut self, name: Option<String>) -> Self {
        self.passthrough = self.passthrough.with_virtual_out(name);
        self
    }

    pub fn with_notch_hum(mut self, on: bool) -> Self {
        self.passthrough = self.passthrough.with_notch_hum(on);
        self
    }

    pub fn with_hpf(mut self, hz: u32) -> Self {
        self.passthrough = self.passthrough.with_hpf(hz);
        self
    }

    pub fn with_cushion(self, delay_ms: u32, stall_ms: u64) -> Self {
        Self {
            passthrough: self.passthrough.with_cushion(delay_ms, stall_ms),
            ..self
        }
    }

    pub fn start(&self, input_device: Option<usize>) -> Result<PassthroughStreams> {
        self.passthrough.start(input_device)
    }

    pub fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.current_volume = volume;
        self.is_muted = false;
        self.passthrough.set_volume(volume);
    }

    pub fn volume_up(&mut self, delta: f32) {
        let new_vol = (self.current_volume + delta).min(1.0);
        self.set_volume(new_vol);
    }

    pub fn volume_down(&mut self, delta: f32) {
        let new_vol = (self.current_volume - delta).max(0.0);
        self.set_volume(new_vol);
    }

    pub fn toggle_mute(&mut self) {
        if self.is_muted {
            self.current_volume = self.volume_before_mute;
            self.passthrough.set_volume(self.current_volume);
            self.is_muted = false;
            println!("Unmuted - Volume: {}%", (self.current_volume * 100.0) as i32);
        } else {
            self.volume_before_mute = self.current_volume;
            self.passthrough.set_volume(0.0);
            self.is_muted = true;
            println!("Muted");
        }
    }

    pub fn get_volume_percent(&self) -> i32 {
        (self.current_volume * 100.0) as i32
    }

    pub fn stats(&self) -> PassthroughStats {
        let total_latency_ms =
            (self.passthrough.block_size as f32 / self.passthrough.sample_rate as f32) * 1000.0;
        PassthroughStats {
            blocks_processed: self
                .passthrough
                .blocks_processed
                .load(Ordering::Relaxed),
            block_size: self.passthrough.block_size,
            sample_rate: self.passthrough.sample_rate,
            theoretical_latency_ms: total_latency_ms,
            estimated_total_latency_ms: total_latency_ms + 2.0,
            volume_processing: self.passthrough.enable_volume,
            buffer_size: self.passthrough.buffer.lock().unwrap().len(),
        }
    }
}
