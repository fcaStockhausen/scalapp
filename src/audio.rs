use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
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

pub struct AudioStreams {
    input: Option<Stream>,
    playback_output: Option<Stream>,
}

impl AudioStreams {
    pub fn stop(&mut self) {
        drop(self.input.take());
        drop(self.playback_output.take());
        println!("Audio playback stopped");
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
}

impl AudioPlayback {
    pub fn new(channels: usize, low_latency: bool) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            multiplier: Arc::new(AtomicF32::new(0.2)),
            low_latency,
            channels,
            cap_samples: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn start(&self, sample_rate: u32, block_size: u32) -> Result<Stream> {
        // tope por tiempo: ~0.75s de audio buffered, sea cual sea el
        // tamaño de bloque de entrada
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
        let buffer = self.buffer.clone();
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Fix 29/10: consumir FRAME-EXACTO acumulando entre
                    // bloques de entrada (la entrada corre con
                    // BufferSize::Default ~4096 frames y el match
                    // bloque-a-bloque descartaba 3/4 de cada bloque =
                    // distorsion total). Patron del router virtual del
                    // passthrough: pop, copiar lo que entra, DEVOLVER el
                    // resto a la cabeza de la cola.
                    let mut buf = buffer.lock().unwrap();
                    let mut pos = 0;
                    while pos < out.len() {
                        match buf.pop_front() {
                            Some(data) => {
                                let n = data.len().min(out.len() - pos);
                                out[pos..pos + n].copy_from_slice(&data[..n]);
                                if n < data.len() {
                                    buf.push_front(data[n..].to_vec());
                                }
                                pos += n;
                            }
                            None => {
                                for s in &mut out[pos..] {
                                    *s = 0.0;
                                }
                                pos = out.len();
                            }
                        }
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
            while total > cap && buf.len() > 1 {
                if let Some(d) = buf.pop_front() {
                    total -= d.len();
                }
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
            volume: Mutex::new(0.5),
        }
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
        // "configuration not supported". Adoptamos la config REAL del
        // dispositivo y buffer Default (CoreAudio elige el óptimo).
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
        let config = StreamConfig {
            channels: self.channels as u16,
            sample_rate: default_cfg.sample_rate(),
            buffer_size: BufferSize::Default,
        };
        let ring = self.ring.clone();
        let total = self.total_captured.clone();
        let playback_slot = self.playback.clone();
        let channels = self.channels;
        let capacity = self.buffer_size_samples;
        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let frames = data.len() / channels;
                    write_ring(&mut ring.lock().unwrap(), data, channels, capacity);
                    total.fetch_add(frames as u64, Ordering::Relaxed);
                    if let Some(p) = playback_slot.lock().unwrap().as_ref() {
                        p.add_audio_data(data.to_vec());
                    }
                },
                |err| eprintln!("Audio callback status: {err}"),
                None,
            )
            .context("failed to build audio input stream")?;
        stream.play().context("failed to start audio input stream")?;
        *self.start_time.lock().unwrap() = Some(Instant::now());
        let mode = if self.low_latency_mode { "low-latency" } else { "normal" };
        println!(
            "Audio capture started ({mode}): {sample_rate}Hz, {} channels",
            self.channels
        );

        let mut playback_output = None;
        if self.enable_playback {
            let playback = Arc::new(AudioPlayback::new(self.channels, self.low_latency_mode));
            let volume = *self.volume.lock().unwrap();
            playback.set_volume(volume);
            match playback.start(sample_rate, self.block_size) {
                Ok(out_stream) => {
                    *self.playback.lock().unwrap() = Some(playback);
                    playback_output = Some(out_stream);
                    println!("✓ Audio playback/monitoring enabled");
                }
                Err(e) => {
                    println!("⚠ Could not start audio playback: {e:#}");
                    println!("  Video will work but you won't hear audio");
                }
            }
        }

        Ok(AudioStreams {
            input: Some(stream),
            playback_output,
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
}
