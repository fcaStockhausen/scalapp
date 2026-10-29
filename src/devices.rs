use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::{query, Camera};

/// Abre una cámara tragando las NSException del binding (que llegan como
/// panic). El binding macOS de nokhwa puede lanzar en set_all cuando el
/// modo pedido no calza con la lista del device (visto: MS2131 en USB2,
/// donde no existe YUYV 1080p60) — eso debe ser un intento fallido, no
/// un crash de la app.
pub fn try_camera_new(
    index: CameraIndex,
    requested: RequestedFormat,
) -> Result<Camera, String> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Camera::new(index, requested)
    }));
    std::panic::set_hook(prev_hook);
    match result {
        Ok(Ok(camera)) => Ok(camera),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("excepción NSException del binding al setear el formato".into()),
    }
}

#[derive(Clone)]
pub struct AudioDevInfo {
    pub id: usize,
    pub name: String,
    pub channels: u16,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct VideoCandidate {
    pub id: u32,
    /// uniqueID de AVFoundation: la forma determinista de reabrir el
    /// dispositivo (el orden de índices cambia entre sesiones de discovery).
    pub unique_id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

pub fn audio_host() -> Host {
    cpal::default_host()
}

pub fn enumerate_input_devices() -> Result<Vec<AudioDevInfo>> {
    let host = audio_host();
    let mut out = Vec::new();
    for (id, device) in host.input_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "Unknown".into());
        let channels = device
            .default_input_config()
            .map(|c| c.channels())
            .unwrap_or(0);
        out.push(AudioDevInfo { id, name, channels });
    }
    Ok(out)
}

pub fn resolve_input_device(index: Option<usize>) -> Result<Device> {
    let host = audio_host();
    match index {
        Some(i) => host
            .input_devices()?
            .nth(i)
            .with_context(|| format!("audio input device {i} not found")),
        None => host.default_input_device().context("no default input device"),
    }
}

pub fn resolve_output_device(index: Option<usize>) -> Result<Device> {
    let host = audio_host();
    match index {
        Some(i) => host
            .output_devices()?
            .nth(i)
            .with_context(|| format!("audio output device {i} not found")),
        None => host
            .default_output_device()
            .context("no default output device"),
    }
}

pub fn auto_detect_audio() -> Vec<AudioDevInfo> {
    println!("Buscando audio HDMI (incluyendo dispositivos genéricos)...");

    let keywords = [
        "usb", "hdmi", "capture", "digital", "elgato", "magewell", "blackmagic", "avermedia",
        "hdmic", "hdmia", "usb-capture", "video", "audio", "uac", "usb audio", "usb device",
        "unspecified", "recording", "input", "line in",
    ];
    let exclusions = [
        "iphone", "facetime", "built-in", "webcam", "camera", "microphone", "speaker",
        "bluetooth", "airpods", "headphones", "output",
    ];

    let devices = enumerate_input_devices().unwrap_or_default();
    let mut candidates = Vec::new();
    for dev in devices {
        if dev.channels < 2 {
            continue;
        }
        let name = dev.name.to_lowercase();
        let is_capture = keywords.iter().any(|k| name.contains(k));
        let is_excluded = exclusions.iter().any(|e| name.contains(e));
        if is_capture && !is_excluded {
            println!("  -> Encontrado: {} (ID: {})", dev.name, dev.id);
            candidates.push(dev);
        }
    }

    let usbn: Vec<AudioDevInfo> = candidates
        .iter()
        .filter(|c| c.name.to_lowercase().contains("usbn"))
        .cloned()
        .collect();
    if !usbn.is_empty() {
        let rest: Vec<AudioDevInfo> = candidates
            .iter()
            .filter(|c| !c.name.to_lowercase().contains("usbn"))
            .cloned()
            .collect();
        candidates = usbn;
        candidates.extend(rest);
    } else {
        candidates.sort_by_key(|c| std::cmp::Reverse(c.channels));
    }

    candidates
}

pub fn probe_camera_by_id(unique_id: &str) -> Option<(u32, u32, f32, String)> {
    // Mejor modo REAL del device (1080p60 exacto si existe; si no — USB2 —
    // el más cercano). Pedir solo modos que existen: el set_all de nokhwa
    // lanza NSException con modos fantasma y deja el device lockeado.
    let mode = match best_mode_for_device(unique_id, (1920, 1080, 60)) {
        Some(m) => m,
        None => {
            eprintln!("[probe] {unique_id}: sin modos YUYV/MJPEG en la lista");
            return None;
        }
    };
    eprintln!(
        "[probe] {unique_id}: mejor modo {}x{}@{} {:?}",
        mode.resolution().width_x,
        mode.resolution().height_y,
        mode.frame_rate(),
        mode.format()
    );
    let camera = match try_camera_new(
        CameraIndex::String(unique_id.to_string()),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(mode)),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[probe] {unique_id}: open falló: {e}");
            return None;
        }
    };
    let resolution = camera.resolution();
    Some((
        resolution.width_x,
        resolution.height_y,
        camera.frame_rate() as f32,
        camera.info().human_name().to_string(),
    ))
}

#[allow(dead_code)]
pub fn probe_camera(index: u32) -> Option<(u32, u32, f32)> {
    probe_camera_by_index(index).map(|(w, h, fps, _)| (w, h, fps))
}

fn probe_camera_by_index(index: u32) -> Option<(u32, u32, f32, String)> {
    // resolver índice → uniqueID por la enumeración de AVFoundation
    let uid = nokhwa_bindings_macos::query_avfoundation()
        .ok()?
        .into_iter()
        .nth(index as usize)
        .map(|info| info.misc().to_string())?;
    probe_camera_by_id(&uid)
}

fn system_profiler_camera_names() -> Vec<String> {
    let output = std::process::Command::new("system_profiler")
        .args(["SPCameraDataType", "-json"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if let Some(cameras) = data["SPCameraDataType"].as_array() {
        for camera in cameras {
            if camera.get("_items").is_some() {
                continue;
            }
            if let Some(name) = camera["_name"].as_str() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Enumeración por query(): índice, nombre y uniqueID de AVFoundation en
/// una sola sesión — uniqueID es la identidad estable para reabrir (el
/// orden de índices cambia entre sesiones de discovery).
fn query_devices_full() -> Vec<(u32, String, String)> {
    query(ApiBackend::Auto)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|info| match info.index() {
            CameraIndex::Index(i) => Some((*i, info.human_name().to_string(), info.misc().to_string())),
            _ => None,
        })
        .collect()
}

/// Elige el mejor modo SEGURO del device (por uniqueID) para el request
/// `want` (w, h, fps). Lee la lista por AVFoundation — sin HID (no toca
/// el isoch).
///
/// Regla de seguridad (bug del binding nokhwa, 29/10): su set_all deja
/// como formato activo la ÚLTIMA entrada con las dimensiones pedidas
/// pero el rango de fps puede venir de un duplicado anterior → par
/// inconsistente → NSException (visto: MS2131 en USB2, donde 1080p@50
/// está en la lista pero explota). Pedir exactamente una entrada que
/// sea la última de sus dimensiones es seguro por construcción.
///
/// Scoring si no hay exacto: fps más cercano primero, luego área
/// (monitoreo: fluidez > tamaño).
pub fn best_mode_for_device(uid: &str, want: (u32, u32, u32)) -> Option<CameraFormat> {
    let device =
        nokhwa_bindings_macos::AVCaptureDevice::new(&CameraIndex::String(uid.to_string())).ok()?;
    let entries: Vec<CameraFormat> = device
        .supported_formats()
        .ok()?
        .into_iter()
        .filter(|f| matches!(f.format(), FrameFormat::YUYV | FrameFormat::MJPEG))
        .collect();
    if entries.is_empty() {
        return None;
    }
    // última entrada por dimensiones (orden de lista preservado)
    let mut last_by_dims: Vec<CameraFormat> = Vec::new();
    for f in &entries {
        match last_by_dims
            .iter_mut()
            .find(|e| e.resolution() == f.resolution())
        {
            Some(slot) => *slot = *f,
            None => last_by_dims.push(*f),
        }
    }
    if let Some(exact) = last_by_dims.iter().find(|f| {
        f.resolution().width_x == want.0
            && f.resolution().height_y == want.1
            && f.frame_rate() == want.2
    }) {
        return Some(*exact);
    }
    let want_area = (want.0 as i64) * (want.1 as i64);
    last_by_dims.sort_by_key(|f| {
        let area = (f.resolution().width_x as i64) * (f.resolution().height_y as i64);
        let fps_diff = (f.frame_rate() as i64 - want.2 as i64).abs();
        (fps_diff, (area - want_area).abs())
    });
    last_by_dims.first().copied()
}

fn is_capture_device_name(name_lower: &str) -> bool {
    ["hdmi", "capture", "usb", "elgato", "magewell", "blackmagic",
        "avermedia", "hdmic", "video", "usb-video", "composite", "component"]
        .iter()
        .any(|k| name_lower.contains(k))
}

fn is_excluded_device_name(name_lower: &str) -> bool {
    ["iphone", "facetime", "built-in", "webcam", "camera",
        "isight", "macbook", "imac", "obs virtual"]
        .iter()
        .any(|e| name_lower.contains(e))
}

pub fn auto_detect_video() -> Vec<VideoCandidate> {
    println!("Buscando capturadoras USB HDMI (incluyendo genéricas)...");

    let mut candidates: Vec<VideoCandidate> = Vec::new();

    if cfg!(target_os = "macos") {
        // 1) Fuente primaria: query() del mismo backend que usará la
        //    captura. Identidad por uniqueID, no por posición.
        for (index, name, unique_id) in query_devices_full() {
            let name_lower = name.to_lowercase();
            if !is_capture_device_name(&name_lower) || is_excluded_device_name(&name_lower) {
                continue;
            }
            if let Some((w, h, fps, _dev_name)) = probe_camera_by_id(&unique_id) {
                // la identidad ya la dio el nombre; aceptar el mejor modo
                // que el device entregue (en USB2 no hay ≥720p usable)
                if w > 0 && h > 0 {
                    let device_name = format!("{name} {w}x{h}@{fps:.0}fps");
                    println!("  -> Encontrado: {device_name} (ID: {index})");
                    candidates.push(VideoCandidate {
                        id: index,
                        unique_id,
                        name: device_name,
                        width: w,
                        height: h,
                        fps,
                    });
                }
            }
        }

        // 2) Fallback: nombres de system_profiler + sondeo posicional
        if candidates.is_empty() {
            for name in system_profiler_camera_names() {
                let name_lower = name.to_lowercase();
                if !is_capture_device_name(&name_lower) || is_excluded_device_name(&name_lower) {
                    continue;
                }
                for i in 0..4u32 {
                    if let Some((w, h, fps, dev_name)) = probe_camera_by_index(i) {
                        let dev_lower = dev_name.to_lowercase();
                        if w >= 1280
                            && h >= 720
                            && (dev_lower.contains(&name_lower) || name_lower.contains(&dev_lower))
                            && !candidates.iter().any(|c| c.id == i)
                        {
                            let device_name = format!("{name} {w}x{h}@{fps:.0}fps");
                            println!("  -> Encontrado: {device_name} (ID: {i})");
                            candidates.push(VideoCandidate {
                                id: i,
                                unique_id: format!("idx:{i}"),
                                name: device_name,
                                width: w,
                                height: h,
                                fps,
                            });
                            break;
                        }
                    }
                }
            }
        }
    }

    // 3) Fallback genérico: cualquier dispositivo con resolución HDMI
    if candidates.is_empty() {
        println!("Buscando por capacidades (dispositivos con resolución HDMI)...");
        for i in 0..5u32 {
            if let Some((w, h, fps, dev_name)) = probe_camera_by_index(i) {
                let dev_lower = dev_name.to_lowercase();
                if w >= 1280
                    && h >= 720
                    && !(w == 1280 && h == 720 && fps < 31.0)
                    && !is_excluded_device_name(&dev_lower)
                {
                    let device_name = format!("Capturadora_Generica_{i} {w}x{h}@{fps:.0}fps");
                    println!("  -> Encontrado: {device_name} (ID: {i})");
                    candidates.push(VideoCandidate {
                        id: i,
                        unique_id: format!("idx:{i}"),
                        name: format!("{device_name} ({dev_name})"),
                        width: w,
                        height: h,
                        fps,
                    });
                }
            }
        }
    }

    candidates.sort_by(|a, b| {
        let pa = a.fps + (a.width as f32 / 1000.0);
        let pb = b.fps + (b.width as f32 / 1000.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });

    candidates
}

pub fn list_devices() {
    println!("Dispositivos HDMI USB Captura SOLAMENTE:");
    println!("{}", "=".repeat(50));

    println!("\nDispositivos de Audio HDMI USB:");
    let candidates = auto_detect_audio();
    if candidates.is_empty() {
        println!("  No se detectaron capturadoras HDMI USB de audio");
    } else {
        for dev in &candidates {
            println!(
                "  [{}] {} - {} canales (capturadora HDMI)",
                dev.id, dev.name, dev.channels
            );
        }
    }

    println!("\nDispositivos de Video HDMI USB:");
    let video_candidates = auto_detect_video();
    if video_candidates.is_empty() {
        println!("  No se detectaron capturadoras HDMI USB de video");
    } else {
        for dev in &video_candidates {
            println!("  [{}] {} (capturadora HDMI)", dev.id, dev.name);
        }
    }

    println!("\nNota: Solo se muestran dispositivos HDMI USB. iPhone/webcams son ignorados.");
    println!("Para probar manualmente dispositivos de audio: scalapp --test-audio");
}

pub fn test_audio_devices() {
    println!("Probando dispositivos de audio para HDMI...");
    println!("{}", "=".repeat(50));

    let devices = enumerate_input_devices().unwrap_or_default();
    for dev in devices {
        if dev.channels < 2 {
            continue;
        }
        println!("\nProbando dispositivo [{}]: {}", dev.id, dev.name);

        let channels = dev.channels.clamp(1, 2);
        match run_audio_probe(&dev, channels) {
            Ok(max_amplitude) => {
                println!("  ✓ Stream creado exitosamente");
                println!("  ✓ Audio detectado - Amplitud máxima: {max_amplitude:.6}");
                if max_amplitude > 0.001 {
                    println!("  🎯 ¡SEÑAL DE AUDIO DETECTADA! (probablemente HDMI)");
                } else {
                    println!("  - Silencio (posible HDMI sin señal)");
                }
            }
            Err(e) => {
                println!("  ✗ No se pudo leer audio");
                println!("  ✗ Error: {e:#}");
            }
        }
    }

    println!("\n{}", "=".repeat(50));
    println!("Nota: Revisa cuál dispositivo muestra señal de audio activa");
    println!("Dispositivos con señales detectadas son buenos candidatos para HDMI");
}

fn run_audio_probe(dev: &AudioDevInfo, channels: u16) -> Result<f32> {
    use cpal::{BufferSize, SampleRate, StreamConfig};
    use cpal::traits::StreamTrait;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let device = resolve_input_device(Some(dev.id))?;
    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(44100),
        buffer_size: BufferSize::Fixed(1024),
    };
    let collected: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = collected.clone();
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mut sink = sink.lock().unwrap();
            if sink.len() < 44100 * 2 * channels as usize {
                sink.extend_from_slice(data);
            }
        },
        |err| eprintln!("probe error: {err}"),
        None,
    )?;
    stream.play()?;
    std::thread::sleep(Duration::from_secs(2));
    drop(stream);

    let data = collected.lock().unwrap();
    if data.is_empty() {
        anyhow::bail!("no data received");
    }
    Ok(data.iter().fold(0.0f32, |m, s| m.max(s.abs())))
}

/// Resuelve un dispositivo de SALIDA por substring de nombre (ej: "BlackHole").
/// Para la salida virtual del router de audio (Discord la toma como mic).
pub fn resolve_output_device_by_name(fragment: &str) -> Result<Device> {
    let host = audio_host();
    let frag = fragment.to_lowercase();
    let mut matches: Vec<(String, Device)> = host
        .output_devices()?
        .filter_map(|d| {
            let name = d.name().unwrap_or_default();
            if name.to_lowercase().contains(&frag) {
                Some((name, d))
            } else {
                None
            }
        })
        .collect();
    match matches.len() {
        0 => bail!("no hay dispositivo de salida que contenga \"{fragment}\""),
        1 => {
            println!("Salida virtual: {}", matches[0].0);
            Ok(matches.remove(0).1)
        }
        n => {
            println!("Dispositivos que coinciden con \"{fragment}\":");
            for (i, (name, _)) in matches.iter().enumerate() {
                println!("  [{i}] {name}");
            }
            bail!("{n} coincidencias — se más específico")
        }
    }
}
