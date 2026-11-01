mod atomicf32;
mod audio;
mod devices;
mod passthrough;
mod renderer;
mod sync;
mod video;

use anyhow::{bail, Result};
use clap::Parser;
use std::io::BufRead;
use std::sync::Arc;

use audio::AudioCapture;
use nokhwa::utils::CameraIndex;
use passthrough::PassthroughWithVolume;
use sync::{AVMonitor, SyncController};
use video::VideoCapture;

trait VolumeTarget {
    fn set_volume(&mut self, volume: f32);
}

impl VolumeTarget for AudioCapture {
    fn set_volume(&mut self, volume: f32) {
        AudioCapture::set_volume(self, volume);
    }
}

impl VolumeTarget for Arc<AudioCapture> {
    fn set_volume(&mut self, volume: f32) {
        AudioCapture::set_volume(self, volume);
    }
}

impl VolumeTarget for PassthroughWithVolume {
    fn set_volume(&mut self, volume: f32) {
        PassthroughWithVolume::set_volume(self, volume);
    }
}

enum VolumeCmd {
    Down,
    Up,
    Mute,
    Quit,
    Set(f32),
}

fn parse_volume_cmd(raw: &str) -> Option<VolumeCmd> {
    match raw {
        "q" | "Q" => Some(VolumeCmd::Quit),
        "v" => Some(VolumeCmd::Down),
        "V" => Some(VolumeCmd::Up),
        "m" | "M" => Some(VolumeCmd::Mute),
        s if s.len() == 1 && s.as_bytes()[0].is_ascii_digit() => {
            Some(VolumeCmd::Set((s.as_bytes()[0] - b'0') as f32 * 0.1))
        }
        _ => None,
    }
}

fn volume_console_loop<T: VolumeTarget>(target: &mut T, mut streams: Option<&mut passthrough::PassthroughStreams>) {
    // escala de UI 0.0-1.0 → rango físico 0-VOLUME_RANGE (30%):
    // toda la resolución en el intervalo que se usa
    let mut current_volume = audio::actual_volume_to_user(0.2); // = 20% físico
    let mut is_muted = false;
    let mut muted_volume = audio::actual_volume_to_user(0.2);
    let apply = |t: &mut T, u: f32| t.set_volume(audio::user_volume_to_actual(u));
    apply(target, current_volume);
    println!(
        "✓ Volume set to {}% (real {:.0}%)",
        (current_volume * 100.0) as i32,
        audio::user_volume_to_actual(current_volume) * 100.0
    );
    println!("Volume: {}% (real {:.0}%) {} | Command (v/V/m/q/0-9): ",
             (current_volume * 100.0) as i32,
             audio::user_volume_to_actual(current_volume) * 100.0,
             if is_muted { "(MUTED)" } else { "" });

    // stdin en un thread + tick de 200ms: el tick atiende los pedidos de
    // rebuild del watchdog (bug A) sin bloquear la consola.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => { let _ = tx.send("q".to_string()); break; }
                Ok(_) => { if tx.send(line).is_err() { break; } }
                Err(_) => break,
            }
        }
    });
    loop {
        let line = match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(s) = streams.as_deref_mut() {
                    s.poll_rebuild();
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let cmd = line.trim();
        match parse_volume_cmd(cmd) {
            Some(VolumeCmd::Quit) => break,
            Some(VolumeCmd::Down) => {
                current_volume = (current_volume - 0.1).max(0.0);
                is_muted = false;
            }
            Some(VolumeCmd::Up) => {
                current_volume = (current_volume + 0.1).min(1.0);
                is_muted = false;
            }
            Some(VolumeCmd::Mute) => {
                if is_muted {
                    current_volume = muted_volume;
                    is_muted = false;
                } else {
                    muted_volume = current_volume;
                    current_volume = 0.0;
                    is_muted = true;
                }
            }
            Some(VolumeCmd::Set(v)) => {
                current_volume = v;
                is_muted = false;
            }
            None => {}
        }
        apply(target, current_volume);
        println!("Volume: {}% (real {:.0}%) {} | Command (v/V/m/q/0-9): ",
                 (current_volume * 100.0) as i32,
                 audio::user_volume_to_actual(current_volume) * 100.0,
                 if is_muted { "(MUTED)" } else { "" });
    }
    println!("\nStopping audio monitor...");
}

#[derive(Parser)]
#[command(
    name = "scalapp",
    about = "ScalApp AV Monitor (Rust) - Latency Tolerant HDMI USB Capture",
    after_help = "Examples:\n  scalapp                          # Audio-only (default), Direct Passthrough V4\n  scalapp --video                  # Full A/V mode (auto-detected capture)\n  scalapp --audio-device 2         # Use specific audio device\n  scalapp --video-device 1         # Use specific video device (implies --video)\n  scalapp --list-devices           # List available devices\n  scalapp --monitor --latency 1.0  # Enable monitoring, 1s buffer"
)]
struct Args {
    #[arg(short = 'a', long = "audio-device")]
    audio_device: Option<usize>,

    #[arg(short = 'v', long = "video-device")]
    video_device: Option<usize>,

    #[arg(short = 'l', long = "latency", default_value_t = 0.5)]
    latency: f64,

    #[arg(short = 'm', long = "monitor")]
    monitor: bool,

    /// Full A/V mode (default is audio-only)
    #[arg(long)]
    video: bool,

    /// Accepted for compatibility; audio-only is already the default
    #[arg(long, hide = true)]
    audio_only: bool,

    #[arg(long)]
    direct_passthrough: bool,

    #[arg(long)]
    no_direct_passthrough: bool,

    /// Salida virtual por nombre parcial (ej: --virtual-out BlackHole).
    /// Router de audio: Discord toma esta salida como su mic — deja de
    /// pelear la capturadora con scalapp.
    #[arg(long)]
    virtual_out: Option<String>,

    /// Colchón deliberado en ms (bug A: coexistencia con Discord).
    /// Absorbe las renegociaciones del dispositivo sin cortes audibles.
    /// 0 = colchón mínimo legacy (~8 bloques).
    #[arg(long = "delay-ms", default_value_t = 0)]
    delay_ms: u32,

    /// Watchdog de entrada: si la entrada se estanca más de este umbral
    /// (ms), scalapp reconstruye el stream solo (revive tras la
    /// renegociación de Discord). 0 = desactivado.
    #[arg(long = "stall-ms", default_value_t = 2000)]
    stall_ms: u64,

    /// Colchón base del audio en modo video (ms). Por defecto auto (~50ms)
    /// + colchón adaptativo: si el device dropea audio (MS2131 con video
    /// activo: gaps ~100-220ms), el colchón sube solo hasta cubrirlos
    /// (tope 350ms). Este flag fija la base; el adaptativo sigue subiendo
    /// si hace falta. Más colchón = más latencia de audio.
    #[arg(long = "audio-cushion-ms", default_value_t = 0)]
    audio_cushion_ms: u64,

    /// Notch anti-hum del chip — DEFAULT ON: con video streameando, la
    /// MS2131 acopla su refresco vertical al audio (tono puro ~60 Hz +
    /// armónicos, medido +7dB). Filtra 60.06/120.12/180.18 Hz con Q=25
    /// (quirúrgico: B1/B2 quedan a -1.8dB, el contenido del juego
    /// sobrevive). Usá --no-notch-hum para desactivarlo.
    #[arg(long = "no-notch-hum", action = clap::ArgAction::SetTrue, default_value_t = false)]
    no_notch_hum: bool,

    /// High-pass opcional en Hz (default off): mata el rumor 28-38 Hz del
    /// chip PERO también colorea el bajo del contenido (HP 48: E1/41Hz
    /// −9dB, A1/55Hz −4dB, B1/62Hz −3dB) — decisión del usuario.
    #[arg(long = "hpf", default_value_t = 0)]
    hpf: u32,

    /// Iniciar con ventana borderless: SIN barra de título ni bordes (no
    /// es fullscreen — la ventana conserva tamaño y posición). Igual que
    /// presionar F; F/Esc la devuelven a ventana normal.
    #[arg(long = "borderless", default_value_t = false)]
    borderless: bool,

    #[arg(long)]
    ultra_low_latency: bool,

    #[arg(long)]
    audio_latency: Option<f64>,

    #[arg(long)]
    block_size: Option<u32>,

    #[arg(long)]
    test_audio: bool,

    #[arg(long)]
    list_devices: bool,

    #[arg(long = "audio-rate", default_value_t = 44100)]
    audio_rate: u32,

    #[arg(long = "video-width", default_value_t = 1920)]
    video_width: u32,

    #[arg(long = "video-height", default_value_t = 1080)]
    video_height: u32,

    #[arg(long = "video-fps", default_value_t = 60)]
    video_fps: u32,
}

/// Default: audio-only. `--video` o `--video-device` activan el modo A/V.
/// `--audio-only` se acepta por compat (ya es el default).
fn is_audio_only(args: &Args) -> bool {
    (!args.video && args.video_device.is_none()) || args.audio_only
}

fn run(args: Args) -> Result<()> {
    let audio_only = is_audio_only(&args);
    let ultra_low_latency = args.ultra_low_latency;

    let latency_buffer = if let Some(audio_latency) = args.audio_latency {
        audio_latency
    } else if ultra_low_latency {
        0.008
    } else if audio_only {
        0.015
    } else {
        args.latency
    };

    if !audio_only && !ultra_low_latency && !(0.1..=2.0).contains(&latency_buffer) {
        bail!("Latency must be between 0.1 and 2.0 seconds");
    }

    let mut direct_passthrough = if audio_only { !args.no_direct_passthrough } else { false };
    if args.direct_passthrough {
        direct_passthrough = true;
    }
    if ultra_low_latency {
        direct_passthrough = false;
    }
    if direct_passthrough && !audio_only {
        bail!("--direct-passthrough requires --audio-only");
    }

    let audio_device_index = if args.audio_device.is_none() {
        let candidates = devices::auto_detect_audio();
        candidates
            .first()
            .map(|c| {
                println!(
                    "Auto-detected HDMI audio device: {} (ID: {})",
                    c.name, c.id
                );
                c.id
            })
    } else {
        args.audio_device
    };

    if direct_passthrough {
        run_direct_passthrough(audio_device_index, args.block_size, args.virtual_out.clone(),
                               args.delay_ms, args.stall_ms, !args.no_notch_hum, args.hpf)?;
        return Ok(());
    }

    let block_size = if let Some(bs) = args.block_size {
        println!("Using custom block size: {bs}");
        bs
    } else if ultra_low_latency {
        let bs = 128;
        println!("Using ultra-low latency block size: {bs}");
        bs
    } else if audio_only {
        let bs = 256;
        println!("Using low-latency block size: {bs}");
        bs
    } else {
        let bs = 1024;
        println!("Using normal block size: {bs}");
        bs
    };

    println!("Initializing AV Monitor components...");
    let audio_capture = Arc::new(AudioCapture::new(
        args.audio_rate,
        2,
        block_size,
        2.0,
        latency_buffer as f32,
        audio_device_index,
        audio_only || ultra_low_latency,
    ));
    if args.audio_cushion_ms > 0 {
        audio_capture.set_cushion_base_ms(args.audio_cushion_ms);
        println!("✓ Audio cushion base: {}ms", args.audio_cushion_ms);
    }
    audio_capture.set_notch_hum(!args.no_notch_hum);
    audio_capture.set_hpf(args.hpf);
    println!(
        "✓ Audio capture initialized: {}Hz, {} channels",
        args.audio_rate, 2
    );

    let video_capture = if !audio_only {
        // Identidad del dispositivo: uniqueID del autodetect (determinista;
        // el orden de índices de AVFoundation baila entre sesiones) o el
        // índice explícito del usuario.
        let device = match args.video_device {
            Some(id) => CameraIndex::Index(id as u32),
            None => {
                let candidates = devices::auto_detect_video();
                match candidates.first() {
                    Some(c) => {
                        println!(
                            "Auto-detected HDMI video device: {} (ID: {})",
                            c.name, c.id
                        );
                        CameraIndex::String(c.unique_id.clone())
                    }
                    None => {
                        // ANTES: fallback a ciegas al índice 1 — llegó a
                        // entregar la FaceTime a 30fps cuando Discord tenía
                        // tomada la capturadora. Mejor fallar con diagnóstico.
                        bail!(
                            "no se pudo abrir ninguna capturadora HDMI de video.\n\
                             \x20 Si Discord/OBS está corriendo, puede tener el device tomado \
                             (Lock Rejected en el log) — cerralo o quitale la cámara y reintentá."
                        );
                    }
                }
            }
        };
        let mut capture = VideoCapture::new(
            device,
            args.video_width,
            args.video_height,
            args.video_fps,
        );
        // si el usuario fijó --video-device N, respetarlo (sin escaneo)
        capture.set_allow_index_scan(args.video_device.is_none());
        let capture = Arc::new(capture);
        println!(
            "✓ Video capture initialized: {}x{} @ {}fps",
            args.video_width, args.video_height, args.video_fps
        );
        Some(capture)
    } else {
        println!("✓ Video capture skipped (audio-only mode)");
        None
    };

    let sync_controller = Arc::new(SyncController::new(
        audio_capture.clone(),
        video_capture.clone(),
    ));
    if audio_only {
        println!("✓ Sync controller initialized (audio-only)");
    } else {
        println!("✓ Sync controller initialized (audio + video)");
    }

    let av_monitor = if args.monitor {
        println!("✓ AV Monitor enabled");
        Some(AVMonitor::new(sync_controller.clone()))
    } else {
        None
    };

    println!("\n{}", "=".repeat(50));
    println!("ScalApp AV Monitor - Latency Tolerant (Rust)");
    println!("{}", "=".repeat(50));
    let latency_mode = if ultra_low_latency {
        "ultra-low (5ms)".to_string()
    } else if audio_only {
        "low-latency (10ms)".to_string()
    } else {
        format!("normal ({:.0}ms)", latency_buffer * 1000.0)
    };
    println!("Latency mode: {latency_mode}");
    println!(
        "Audio device: {}",
        audio_device_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "default".into())
    );
    println!(
        "Video device: {}",
        args.video_device
            .map(|i| i.to_string())
            .unwrap_or_else(|| "default".into())
    );
    println!("{}\n", "=".repeat(50));

    let audio_streams = std::sync::Arc::new(std::sync::Mutex::new(audio_capture.start()?));
    println!("✓ Audio capture started");

    if let Some(video) = &video_capture {
        match video.start() {
            Ok((w, h, fps)) => println!("✓ Video capture started: {w}x{h} @ {fps:.2}fps"),
            Err(e) => {
                println!("✗ Video capture failed: {e}");
                bail!("failed to start video capture");
            }
        }
    } else {
        println!("✓ Video capture skipped (audio-only mode)");
    }

    sync_controller.start();
    println!("✓ Sync controller started");

    if let Some(monitor) = &av_monitor {
        monitor.start_monitoring(2.0);
        println!("✓ AV monitoring started");
    }

    let mode_text = if ultra_low_latency {
        "ultra-low latency audio-only mode"
    } else if audio_only {
        "audio-only mode"
    } else {
        "audio + video mode"
    };
    println!("\nAV Monitor is running in {mode_text}. Close the window to stop.\n");

    let result = if audio_only {
        if ultra_low_latency {
            audio_capture.set_latency_buffer(0.008);
            println!("✓ Audio latency set to 8ms (ultra-low latency mode)");
        } else {
            audio_capture.set_latency_buffer(0.015);
            println!("✓ Audio latency set to 15ms (low latency mode)");
        }
        println!("\n🎵 Audio Monitor Mode - Ultra Low Latency");
        println!("{}", "=".repeat(50));
        println!("Controls:");
        println!("  v/V: Volume Down/Up");
        println!("  0-9: Set volume (0% to 90%)");
        println!("  m  : Mute/Unmute");
        println!("  q  : Quit");
        println!("{}\n", "=".repeat(50));

        let mut target = Arc::clone(&audio_capture);
        volume_console_loop(&mut target, None);
        Ok::<(), anyhow::Error>(())
    } else {
        let video = video_capture
            .as_ref()
            .expect("A/V mode always builds a video capture");
        renderer::run(
            sync_controller.clone(),
            Arc::clone(video),
            Some(audio_streams.clone()),
            args.borderless,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))
    };

    println!("\nStopping AV Monitor...");
    sync_controller.stop();
    println!("✓ Sync controller stopped");

    if let Some(monitor) = &av_monitor {
        monitor.stop_monitoring();
        println!("✓ AV monitoring stopped");
        println!("\n{}", monitor.get_report());

        let stats = sync_controller.get_performance_stats();
        println!("\nPerformance Statistics:");
        println!("  Total sync checks: {}", stats.total_sync_checks);
        println!("  Frames dropped: {}", stats.video_frames_dropped);
        println!("  Average drift: {:.1}ms", stats.avg_drift_ms);
        println!("  Maximum drift: {:.1}ms", stats.max_drift_ms);
    }

    if let Some(video) = &video_capture {
        video.stop();
        println!("✓ Video capture stopped");
    }

    audio_streams.lock().unwrap().stop();
    audio_capture.stop();
    println!("✓ Audio capture stopped");

    result
}

fn run_direct_passthrough(audio_device_index: Option<usize>, block_size_arg: Option<u32>, virtual_out: Option<String>,
                          delay_ms: u32, stall_ms: u64, notch_hum: bool, hpf: u32) -> Result<()> {
    let block_size = block_size_arg.unwrap_or(256);
    let block_ms = (block_size as f64 / 44100.0) * 1000.0;

    println!("\n{}", "=".repeat(50));
    println!("Direct Passthrough Mode - Low Latency V4");
    println!("{}", "=".repeat(50));
    println!("  Input device: {:?}", audio_device_index);
    println!("  Sample rate: 44100Hz");
    println!("  Block size: {block_size} samples (~{block_ms:.2}ms)");
    println!("  Volume processing: enabled");
    println!("  Stream mode: Separate streams (like low-latency mode)");
    println!("  Theoretical min latency: ~{block_ms:.2}ms");
    println!("  Expected total latency: ~{:.1}ms (including OS overhead)", block_ms + 1.0);
    println!("{}\n", "=".repeat(50));

    let passthrough = PassthroughWithVolume::new(44100, 2, block_size, 0.2)
        .with_cushion(delay_ms, stall_ms)
        .with_virtual_out(virtual_out)
        .with_notch_hum(notch_hum)
        .with_hpf(hpf);
    let mut streams = passthrough.start(audio_device_index)?;
    println!("✓ Direct passthrough V4 started\n");

    println!("\n🎵 Audio Monitor Mode - Direct Passthrough V4 (~{:.0}ms)", block_ms + 1.0);
    println!("{}", "=".repeat(50));
    println!("Controls:");
    println!("  v/V: Volume Down/Up");
    println!("  0-9: Set volume (0% to 90%)");
    println!("  m  : Mute/Unmute");
    println!("  q  : Quit");
    println!("{}\n", "=".repeat(50));

    let mut target = passthrough;
    volume_console_loop(&mut target, Some(&mut streams));

    println!("\nStopping AV Monitor...");
    streams.stop();
    println!("✓ Direct passthrough stopped");
    println!("\nAV Monitor stopped successfully.");
    Ok(())
}

fn main() {
    let args = Args::parse();

    let _ = ctrlc::set_handler(|| {
        println!("\nReceived signal, shutting down...");
        std::process::exit(0);
    });

    if args.list_devices {
        devices::list_devices();
        return;
    }

    if args.test_audio {
        devices::test_audio_devices();
        return;
    }

    if let Err(e) = run(args) {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_commands_parse() {
        assert!(matches!(parse_volume_cmd("v"), Some(VolumeCmd::Down)));
        assert!(matches!(parse_volume_cmd("V"), Some(VolumeCmd::Up)));
        assert!(matches!(parse_volume_cmd("m"), Some(VolumeCmd::Mute)));
        assert!(matches!(parse_volume_cmd("q"), Some(VolumeCmd::Quit)));
        assert!(matches!(parse_volume_cmd("7"), Some(VolumeCmd::Set(0.7))));
        assert!(parse_volume_cmd("10").is_none());
        assert!(parse_volume_cmd("").is_none());
    }

    #[test]
    fn audio_only_is_the_default() {
        let def = Args::try_parse_from(["scalapp"]).unwrap();
        assert!(is_audio_only(&def));

        let video = Args::try_parse_from(["scalapp", "--video"]).unwrap();
        assert!(!is_audio_only(&video));

        // --video-device implica modo A/V
        let vd = Args::try_parse_from(["scalapp", "--video-device", "1"]).unwrap();
        assert!(!is_audio_only(&vd));

        // compat: --audio-only sigue aceptado
        let ao = Args::try_parse_from(["scalapp", "--audio-only"]).unwrap();
        assert!(is_audio_only(&ao));

        // los flags de audio puro no activan video
        let ull = Args::try_parse_from(["scalapp", "--ultra-low-latency"]).unwrap();
        assert!(is_audio_only(&ull));
    }
}
