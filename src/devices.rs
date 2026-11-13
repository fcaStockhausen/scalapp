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
/// un crash de la app. Con reintentos ante "Lock Rejected": otro proceso
/// (Discord) puede tener el device un instante.
pub fn try_camera_new(index: CameraIndex, requested: RequestedFormat) -> Result<Camera, String> {
    try_camera_new_retry(index, requested, 0)
}

/// Ídem con hasta `retries` reintentos (1.5s entre cada uno) cuando el
/// device está tomado ("Lock Rejected" / "Already in use").
pub fn try_camera_new_retry(
    index: CameraIndex,
    requested: RequestedFormat,
    retries: u32,
) -> Result<Camera, String> {
    let mut attempt = 0;
    loop {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Camera::new(index.clone(), requested)
        }));
        std::panic::set_hook(prev_hook);
        match result {
            Ok(Ok(camera)) => return Ok(camera),
            Ok(Err(e)) => {
                let msg = e.to_string();
                let busy = msg.contains("Lock Rejected") || msg.contains("Already in use");
                if busy && attempt < retries {
                    attempt += 1;
                    println!("[camera] device tomado (¿Discord?) — reintento {attempt}/{retries} en 1.5s");
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    continue;
                }
                return Err(msg);
            }
            Err(_) => {
                return Err("excepción NSException del binding al setear el formato".into())
            }
        }
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
    let camera = match try_camera_new_retry(
        CameraIndex::String(unique_id.to_string()),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(mode)),
        3,
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
/// ¿Existe el combo EXACTO (w,h,fps) YUYV en la lista REAL del device?
/// Mira TODOS los duplicados — no solo el "último por dimensión".
/// Con el binding vendoreado y parcheado (set_all matchea el par
/// formato+rango atómicamente) pedir cualquiera de los duplicados es
/// seguro: así pide fps nativas OBS (activeVideoMinFrameDuration).
pub fn exact_mode_in_raw_list(uid: &str, want: (u32, u32, u32)) -> Option<CameraFormat> {
    let device =
        nokhwa_bindings_macos::AVCaptureDevice::new(&CameraIndex::String(uid.to_string())).ok()?;
    let formats = device.supported_formats().ok()?;
    formats.into_iter().find(|f| {
        f.format() == FrameFormat::YUYV
            && f.resolution().width_x == want.0
            && f.resolution().height_y == want.1
            && f.frame_rate() == want.2
    })
}

/// Dump completo de la lista de modos UVC de un device: TODAS las
/// entradas crudas (duplicados incluidos — evidencia del bug del
/// binding) + el resumen "última por dimensión" que usa
/// best_mode_for_device. No abre el device: solo lee la lista de
/// formatos de AVFoundation (seguro aunque otro proceso lo tenga).
/// `target`: uniqueID, índice, o None → caché/primera capturadora.
pub fn dump_video_modes(target: Option<&str>) {
    let index: CameraIndex = match target {
        Some(t) => match t.parse::<u32>() {
            Ok(i) => CameraIndex::Index(i),
            Err(_) => CameraIndex::String(t.to_string()),
        },
        None => {
            let pick = cached_device_uid().or_else(|| {
                video_device_map()
                    .into_iter()
                    .find(|e| e.class == DeviceClass::Capture)
                    .map(|e| e.uid)
            });
            match pick {
                Some(uid) => CameraIndex::String(uid),
                None => {
                    println!("--dump-modes: no hay capturadora conocida (pasá uid o índice)");
                    return;
                }
            }
        }
    };
    println!("Dump de modos UVC para {:?}:", index);
    let device = match nokhwa_bindings_macos::AVCaptureDevice::new(&index) {
        Ok(d) => d,
        Err(e) => {
            println!("no se pudo enumerar: {e}");
            return;
        }
    };
    println!("Device: {}", device.info().human_name());
    let formats = match device.supported_formats() {
        Ok(f) => f,
        Err(e) => {
            println!("supported_formats falló: {e}");
            return;
        }
    };
    println!("— entradas crudas: {}", formats.len());
    for (i, f) in formats.iter().enumerate() {
        println!(
            "  [{i:02}] {}x{} @{} {:?}",
            f.resolution().width_x,
            f.resolution().height_y,
            f.frame_rate(),
            f.format()
        );
    }
    // resumen "última por dimensión" (el blindaje del bug de duplicados
    // asimétricos del binding — solo estos modos son seguros de pedir)
    let mut last_by_dims: Vec<CameraFormat> = Vec::new();
    for f in &formats {
        match last_by_dims
            .iter_mut()
            .find(|e| e.resolution() == f.resolution())
        {
            Some(slot) => *slot = *f,
            None => last_by_dims.push(*f),
        }
    }
    println!("— última-entrada-por-dimensión (pedibles): {}", last_by_dims.len());
    for f in &last_by_dims {
        println!(
            "  {}x{} @{} {:?}",
            f.resolution().width_x,
            f.resolution().height_y,
            f.frame_rate(),
            f.format()
        );
    }
}

pub fn best_mode_for_device(uid: &str, want: (u32, u32, u32)) -> Option<CameraFormat> {    let device =
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
    Some(pick_fallback(&last_by_dims, want))
}

/// Elección de fallback cuando el modo exacto no existe en la lista
/// real del device (lección del episodio USB2: la lista sin 1080p60
/// elegía 800x600 — un 4:3 que deforma una fuente 16:9).
///
/// Prioridad (la fuente se ve mejor en el MISMO aspect — el chip
/// escala la entrada al modo negociado):
///   1) mismo aspect que `want` y mismo fps → mayor área (sin pasarse
///      del área pedida; si todo se pasa, la mayor)
///   2) mismo fps → mayor área bajo el mismo criterio
///   3) mismo aspect, cualquier fps → mayor área
///   4) histórico: el más cercano en (diferencia de fps, |área−want|)
fn pick_fallback(cands: &[CameraFormat], want: (u32, u32, u32)) -> CameraFormat {
    let want_area = i64::from(want.0) * i64::from(want.1);
    let area = |f: &CameraFormat| i64::from(f.resolution().width_x) * i64::from(f.resolution().height_y);
    let want_ar = f64::from(want.0) / f64::from(want.1);
    let same_ar = |f: &CameraFormat| {
        let ar = f64::from(f.resolution().width_x) / f64::from(f.resolution().height_y);
        (ar - want_ar).abs() < 0.02
    };
    // la mayor área sin pasarse de want; si TODAS se pasan, la mayor
    let pick_largest = |pool: Vec<&CameraFormat>| -> Option<CameraFormat> {
        let mut best_le: Option<&CameraFormat> = None;
        let mut best_any: Option<&CameraFormat> = None;
        for c in pool {
            if area(c) <= want_area && best_le.map_or(true, |b| area(c) > area(b)) {
                best_le = Some(c);
            }
            if best_any.map_or(true, |b| area(c) > area(b)) {
                best_any = Some(c);
            }
        }
        best_le.or(best_any).copied()
    };

    let at_fps: Vec<&CameraFormat> = cands.iter().filter(|f| f.frame_rate() == want.2).collect();
    if let Some(best) = pick_largest(at_fps.iter().copied().filter(|f| same_ar(f)).collect()) {
        return best;
    }
    if let Some(best) = pick_largest(at_fps) {
        return best;
    }
    if let Some(best) = pick_largest(cands.iter().filter(|f| same_ar(f)).collect()) {
        return best;
    }
    let mut sorted: Vec<CameraFormat> = cands.to_vec();
    sorted.sort_by_key(|f| {
        let fps_diff = (f.frame_rate() as i64 - i64::from(want.2)).abs();
        (fps_diff, (area(f) - want_area).abs())
    });
    sorted[0]
}

/// Clasificación de dispositivos de video: la heurística anti-webcam.
/// Nunca abrir nada que no sea clase Capture (o Unknown como fallback
/// de capturadoras con nombre exótico) — Virtual/Webcam vetados en
/// TODOS los caminos de selección.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceClass {
    /// Capturadora HDMI (o candidata por nombre)
    Capture,
    /// Webcam (FaceTime, integradas, iPhone) — VETADA
    Webcam,
    /// Cámara virtual (OBS, droidcam...) — VETADA
    Virtual,
    /// Nombre no reconocido — se prueba como fallback con filtro ≥720p
    Unknown,
}

pub fn classify_device_name(name_lower: &str) -> DeviceClass {
    // normalizar acentos (cámara/camara)
    let n: String = name_lower
        .chars()
        .map(|c| match c {
            'á' => 'a', 'é' => 'e', 'í' => 'i', 'ó' => 'o', 'ú' => 'u',
            other => other,
        })
        .collect();
    // el orden importa: virtual antes que webcam ("OBS Virtual Camera"),
    // webcam antes que capture ("USB Camera" = webcam, exclusión gana)
    let virtual_kw = ["obs virtual", "virtual", "droidcam", "epoccam", "iriun", "snap camera"];
    let webcam_kw = ["facetime", "isight", "built-in", "builtin", "webcam", "camera",
                     "camara", "iphone", "ipad", "macbook", "imac", "continuity"];
    let capture_kw = ["hdmi", "capture", "usb", "elgato", "magewell", "blackmagic",
                      "avermedia", "hdmic", "video", "usb-video", "composite", "component"];
    if virtual_kw.iter().any(|k| n.contains(k)) {
        return DeviceClass::Virtual;
    }
    if webcam_kw.iter().any(|k| n.contains(k)) {
        return DeviceClass::Webcam;
    }
    if capture_kw.iter().any(|k| n.contains(k)) {
        return DeviceClass::Capture;
    }
    DeviceClass::Unknown
}

/// Entrada del mapa de dispositivos de video (una enumeración, todo
/// clasificado — la base de TODA decisión de apertura).
pub struct VideoDeviceEntry {
    pub index: u32,
    pub uid: String,
    pub name: String,
    pub class: DeviceClass,
}

/// Enumera y clasifica todos los dispositivos de video visibles.
pub fn video_device_map() -> Vec<VideoDeviceEntry> {
    query_devices_full()
        .into_iter()
        .map(|(index, name, unique_id)| VideoDeviceEntry {
            index,
            uid: unique_id,
            class: classify_device_name(&name.to_lowercase()),
            name,
        })
        .collect()
}

/// Caché del último capturadora usada con éxito (~/.scalapp_device):
/// cuando el discovery de AVFoundation se cuelga (visto: device visible
/// en ioreg pero ausente del query tras caídas de bus), la apertura
/// DIRECTA por uniqueID sigue funcionando — el arranque cae al caché.
pub fn cached_device_uid() -> Option<String> {
    std::fs::read_to_string(device_cache_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn cache_device_uid(uid: &str) {
    let _ = std::fs::write(device_cache_path(), uid);
}

fn device_cache_path() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    p.push(".scalapp_device");
    p
}

/// Imprime el mapa con la clasificación — visible siempre en el arranque
/// para que la decisión de selección sea auditable.
pub fn print_video_device_map() {
    println!("Mapa de dispositivos de video:");
    for e in video_device_map() {
        let tag = match e.class {
            DeviceClass::Capture => "capturadora",
            DeviceClass::Webcam => "WEBCAM (vetada)",
            DeviceClass::Virtual => "virtual (vetada)",
            DeviceClass::Unknown => "desconocida (fallback)",
        };
        println!("  [{}] {} — {}", e.index, e.name, tag);
    }
}

pub fn auto_detect_video() -> Vec<VideoCandidate> {
    println!("Buscando capturadoras USB HDMI...");
    let map = video_device_map();
    let mut candidates: Vec<VideoCandidate> = Vec::new();

    // 1) Solo clase Capture (heurística anti-webcam: Virtual/Webcam ni
    //    se prueban — nunca más se les enciende la luz por accidente)
    for e in map.iter().filter(|e| e.class == DeviceClass::Capture) {
        if let Some((w, h, fps, _)) = probe_camera_by_id(&e.uid) {
            if w > 0 && h > 0 {
                let device_name = format!("{} {w}x{h}@{fps:.0}fps", e.name);
                println!("  -> Encontrado: {device_name} (ID: {})", e.index);
                candidates.push(VideoCandidate {
                    id: e.index,
                    unique_id: e.uid.clone(),
                    name: device_name,
                    width: w,
                    height: h,
                    fps,
                });
            }
        }
    }

    // 2) Fallback: dispositivos de clase Unknown (capturadoras con
    //    nombre exótico), con filtro de resolución HDMI
    if candidates.is_empty() {
        println!("Sin capturadoras por nombre — probando dispositivos desconocidos...");
        for e in map.iter().filter(|e| e.class == DeviceClass::Unknown) {
            if let Some((w, h, fps, _)) = probe_camera_by_id(&e.uid) {
                if w >= 1280 && h >= 720 && !(w == 1280 && h == 720 && fps < 31.0) {
                    let device_name = format!("Capturadora_Generica_{} {w}x{h}@{fps:.0}fps", e.index);
                    println!("  -> Encontrado: {device_name} (ID: {})", e.index);
                    candidates.push(VideoCandidate {
                        id: e.index,
                        unique_id: e.uid.clone(),
                        name: device_name,
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
    print_video_device_map();

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

#[cfg(test)]
mod tests {
    use super::pick_fallback;
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa_core::types::CameraFormat;

    fn fmt(w: u32, h: u32, fps: u32) -> CameraFormat {
        CameraFormat::new_from(w, h, nokhwa_core::types::FrameFormat::YUYV, fps)
    }

    #[test]
    fn fallback_prefiere_mismo_aspect_a_area() {
        // episodio USB2: sin 1080p60, la lista tenía 4:3 hasta 800x600 y
        // 16:9 menores — el 16:9 debe ganar aunque el 4:3 sea más grande
        let cands = [fmt(800, 600, 60), fmt(1280, 720, 60), fmt(1024, 768, 60)];
        let pick = pick_fallback(&cands, (1920, 1080, 60));
        assert_eq!((pick.resolution().width_x, pick.resolution().height_y), (1280, 720));
    }

    #[test]
    fn fallback_sin_mismo_aspect_toma_mayor_area_al_fps() {
        let cands = [fmt(640, 480, 60), fmt(800, 600, 60), fmt(1024, 768, 60)];
        let pick = pick_fallback(&cands, (1920, 1080, 60));
        assert_eq!((pick.resolution().width_x, pick.resolution().height_y), (1024, 768));
    }

    #[test]
    fn fallback_fps_gana_a_aspect() {
        // decisión de diseño: FLUIDEZ primero (prioridad del proyecto:
        // 60fps antes que calidad). Sin 1080p60, un 4:3@60 le gana a un
        // 16:9@30 — y open_camera avisa del aspecto distinto.
        let cands = [fmt(800, 600, 60), fmt(1920, 1080, 30)];
        let pick = pick_fallback(&cands, (1920, 1080, 60));
        assert_eq!((pick.resolution().width_x, pick.resolution().height_y), (800, 600));
        assert_eq!(pick.frame_rate(), 60);
    }

    #[test]
    fn fallback_no_se_pasa_del_area_pedida() {
        // si hay un modo mayor que el pedido, prefiere el mayor que NO
        // se pasa (menos bandwidth para la misma imagen escalada)
        let cands = [fmt(2560, 1440, 60), fmt(1280, 720, 60)];
        let pick = pick_fallback(&cands, (1920, 1080, 60));
        assert_eq!((pick.resolution().width_x, pick.resolution().height_y), (1280, 720));
    }

    #[test]
    fn fallback_todo_gigante_toma_el_mayor() {
        let cands = [fmt(2560, 1440, 30), fmt(1920, 1080, 30)];
        let pick = pick_fallback(&cands, (1280, 720, 60));
        assert_eq!((pick.resolution().width_x, pick.resolution().height_y), (2560, 1440));
    }

    #[test]
    fn fallback_historico_cuando_no_hay_coincidencia() {
        // sin mismo aspect ni fps: el histórico (fps más cercano)
        let cands = [fmt(800, 600, 30), fmt(640, 480, 50)];
        let pick = pick_fallback(&cands, (1920, 1080, 60));
        assert_eq!(pick.frame_rate(), 50);
    }
}
