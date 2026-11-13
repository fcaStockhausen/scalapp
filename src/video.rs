use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::Camera;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::devices;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct TimestampedFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub timestamp: f64,
    pub frame_number: u64,
}

/// Métricas de la captura (Fase 1 del ROADMAP: medir dónde mueren los frames).
#[derive(Clone)]
#[allow(dead_code)]
pub struct CaptureStats {
    pub fps_actual: f32,
    pub total_frames: u64,
    /// Frames sobrescritos en el slot latest-wins sin haber sido mostrados.
    /// Es el "dropped" sano del monitoreo: el renderer va tarde y gana el
    /// frame más nuevo.
    pub skipped_frames: u64,
    pub last_frame_time: Option<f64>,
    /// Pacing del DEVICE (capture_timestamp de AVFoundation = reloj de la
    /// señal): la llegada REAL. fps promedio 60 con deltas desparejos
    /// = judder aunque el contador diga 60.
    pub device_bursts: u64,
    pub device_gaps: u64,
    pub device_p50_ms: f32,
    /// Pacing del PULL-LOOP (nuestro ritmo de consumo): si difiere del
    /// device, la ráfaga la genera AVFoundation/nokhwa al entregarnos
    /// los buffers — no la señal.
    pub pull_bursts: u64,
    pub pull_gaps: u64,
    pub pull_p50_ms: f32,
}

/// Pacing medido en un reloj cualquiera (deltas entre eventos).
/// La clave del diagnóstico de judder: el pacing DEL DEVICE (su
/// capture_timestamp, reloj de la señal) puede diferir del pacing del
/// pull-loop (nuestro ritmo de consumo) — medir solo el último es
/// medir "los hertz de salida de la aplicación", no los de entrada.
struct PacingStats {
    last: Option<f64>,
    bursts: u64,
    gaps: u64,
    deltas: Vec<f32>,
    /// Intervalo esperado entre frames (ms): los umbrales de
    /// ráfaga/hueco escalan con él (60fps → 8/25ms; con --30 el
    /// intervalo normal es 33.3ms y un hueco "real" es >50ms).
    expected_ms: f32,
}

impl PacingStats {
    fn new() -> Self {
        Self {
            last: None,
            bursts: 0,
            gaps: 0,
            deltas: Vec::new(),
            expected_ms: 16.7,
        }
    }

    fn set_expected_fps(&mut self, fps: f32) {
        if fps > 0.0 {
            self.expected_ms = 1000.0 / fps;
        }
    }

    fn record(&mut self, t: f64) {
        if let Some(prev) = self.last.replace(t) {
            let delta_ms = ((t - prev) * 1000.0) as f32;
            // umbrales relativos: <0.5× y >1.5× el intervalo esperado
            // (con 16.7ms reproducen los históricos 8/25ms)
            if delta_ms < self.expected_ms * 0.5 {
                self.bursts += 1;
            }
            if delta_ms > self.expected_ms * 1.5 {
                self.gaps += 1;
            }
            self.deltas.push(delta_ms);
            let max = 15 * 120;
            if self.deltas.len() > max {
                let drop = self.deltas.len() - max;
                self.deltas.drain(..drop);
            }
        }
    }

    fn p50_ms(&self) -> f32 {
        if self.deltas.is_empty() {
            return 0.0;
        }
        let mut v = self.deltas.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    }

    /// Período medio de la ventana: promedio de los deltas (la
    /// cuantización de 1ms del DEV se promedia sobre ~900 muestras →
    /// error <0.05ms). Tasa REAL de la señal: 59.94 vs 60.00.
    /// Promedio de tasa EXCLUYENDO outliers (deltas fuera de 5..40ms):
    /// un hiccup del device de 500ms arrastraría el promedio puro y el
    /// pacer derivaría (con el guard de re-anclaje como segunda red).
    fn window_rate_ms(&self) -> f32 {
        if self.deltas.len() < 10 {
            return 16.7;
        }
        let mut sum = 0.0f32;
        let mut n = 0usize;
        for d in &self.deltas {
            if (5.0..40.0).contains(d) {
                sum += d;
                n += 1;
            }
        }
        if n < 10 {
            return 16.7;
        }
        sum / n as f32
    }
}

/// Trace de eventos con timestamp (SCALAPP_TRACE=<path>): stores del
/// capturador (con reloj device Y pull) y paints del renderer — la
/// línea de tiempo completa para análisis post-morten de pacing.
pub struct Trace {
    file: Option<std::fs::File>,
    t0: Instant,
}

impl Trace {
    fn from_env() -> Self {
        let file = std::env::var("SCALAPP_TRACE").ok().map(|path| {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap_or_else(|e| panic!("SCALAPP_TRACE {path}: {e}"));
            println!("[trace] eventos a {path}");
            f
        });
        Self {
            file,
            t0: Instant::now(),
        }
    }

    #[inline]
    fn log(&mut self, event: &str, detail: &str) {
        if let Some(f) = &mut self.file {
            use std::io::Write;
            let t = self.t0.elapsed().as_secs_f64() * 1000.0;
            let _ = writeln!(f, "{t:.3}\t{event}\t{detail}");
        }
    }
}

struct StatsInner {
    frame_count: u64,
    skipped_frames: u64,
    fps_actual: f32,
    fps_frame_count: u64,
    last_fps_time: Instant,
    last_frame_time: Option<f64>,
    // pacing del DEVICE (capture_timestamp de AVFoundation = reloj de la
    // señal) vs pacing del PULL-LOOP (nuestro consumo)
    device_pacing: PacingStats,
    pull_pacing: PacingStats,
}

impl StatsInner {
    fn new() -> Self {
        Self {
            frame_count: 0,
            skipped_frames: 0,
            fps_actual: 0.0,
            fps_frame_count: 0,
            last_fps_time: Instant::now(),
            last_frame_time: None,
            device_pacing: PacingStats::new(),
            pull_pacing: PacingStats::new(),
        }
    }
}

/// Transforma un buffer RGB24 in-place: gain de R + mezcla hacia luma
/// (saturación). Corre en el thread de captura — fuera del loop de UI.
#[allow(dead_code)]
fn apply_color_adjust(buf: &mut [u8], rg: f32, sat: f32) {
    for px in buf.as_chunks_mut::<3>().0.iter_mut() {
        let r = px[0] as f32 * rg;
        let g = px[1] as f32;
        let b = px[2] as f32;
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        px[0] = (luma + (r - luma) * sat).clamp(0.0, 255.0) as u8;
        px[1] = (luma + (g - luma) * sat).clamp(0.0, 255.0) as u8;
        px[2] = (luma + (b - luma) * sat).clamp(0.0, 255.0) as u8;
    }
}

/// Decode YUYV → RGBA en UNA sola pasada, con el ajuste de color inline.
/// Reemplaza la cadena de 3 pasadas (decode nokhwa → color adjust →
/// RGB→RGBA en el renderer).
///
/// PERF: escrituras indexadas sobre buffer pre-sizeado — la primera
/// versión usaba push byte-a-byte (33M pushes con check de capacidad
/// por frame ≈ 16ms = un core entero a 60fps; hallado con `sample`:
/// el capture thread al 100% dentro del decoder).
///
/// Coeficientes BT.601 limited-rate EXACTOS de nokhwa (yuyv444_to_rgb,
/// punto fijo) para preservar la calibración de color del usuario.
///
/// AJUSTE DE COLOR EN DOMINIO YUV: desaturar hacia luma en RGB
/// (luma = 0.299R+0.587G+0.114B — mismos pesos que Y de BT.601) es
/// matemáticamente equivalente a escalar (U,V) hacia 128 → 2 float
/// por PAR de píxeles en vez de ~12 por píxel. El gain de rojo se
/// aplica sobre la R decodificada. Diferencias solo por redondeo ±1
/// LSB en colores saturados a tope.
fn decode_yuyv_to_rgba(yuyv: &[u8], red_gain: f32, saturation: f32) -> Vec<u8> {
    let neutral = (red_gain - 1.0).abs() <= 1e-3 && (saturation - 1.0).abs() <= 1e-3;
    let scale_chroma = !neutral && (saturation - 1.0).abs() > 1e-3;
    let scale_red = !neutral && (red_gain - 1.0).abs() > 1e-3;
    let n_px = yuyv.len() / 4 * 2;
    let mut out = vec![0u8; n_px * 4];
    let mut o = 0usize;
    for chunk in yuyv.chunks_exact(4) {
        let mut d = i32::from(chunk[1]) - 128; // U - 128
        let mut e = i32::from(chunk[3]) - 128; // V - 128
        if scale_chroma {
            d = ((d as f32) * saturation).round() as i32;
            e = ((e as f32) * saturation).round() as i32;
        }
        for &y in [&chunk[0], &chunk[2]] {
            let c298 = (i32::from(y) - 16) * 298;
            let r = ((c298 + 409 * e + 128) >> 8).clamp(0, 255);
            let g = ((c298 - 100 * d - 208 * e + 128) >> 8).clamp(0, 255);
            let b = ((c298 + 516 * d + 128) >> 8).clamp(0, 255);
            out[o] = if scale_red { (r as f32 * red_gain) as u8 } else { r as u8 };
            out[o + 1] = g as u8;
            out[o + 2] = b as u8;
            out[o + 3] = 255;
            o += 4;
        }
    }
    out
}

/// Cuántos frames del device saltar por cada uno presentado (modo
/// económico --30): step = round(fps_device / cap). cap 0 (sin cap) o
/// cap ≥ fps del device → 1 (no diezmamos).
fn decimate_step(cap: u32, device_fps: f32) -> u64 {
    if cap == 0 || device_fps <= cap as f32 {
        return 1;
    }
    ((device_fps / cap as f32) + 0.5).floor() as u64
}

/// Reloj de pared en ms (para vigilancia de stall del supervisor entre
/// sesiones; no se usa para pacing).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Almacena `frame` en el slot latest-wins. Si el frame reemplazado nunca
/// fue mostrado por el renderer (`displayed` quedó atrás), cuenta como skip.
fn store_frame(
    latest: &Mutex<Option<Arc<TimestampedFrame>>>,
    displayed: &AtomicU64,
    stats: &Mutex<StatsInner>,
    frame: TimestampedFrame,
) {
    let replaced_unseen = {
        let mut slot = latest.lock().unwrap();
        let unseen = slot
            .as_ref()
            .filter(|f| f.frame_number > displayed.load(Ordering::Relaxed))
            .is_some();
        *slot = Some(Arc::new(frame));
        unseen
    };
    if replaced_unseen {
        stats.lock().unwrap().skipped_frames += 1;
    }
}

pub struct VideoCapture {
    /// Identidad del dispositivo: `String(uniqueID)` desde el autodetect
    /// (determinista) o `Index(n)` cuando el usuario lo fijó a mano.
    device: CameraIndex,
    width: u32,
    height: u32,
    pub fps: u32,
    /// Solo aplica a `Index(n)`: el orden de dispositivos de AVFoundation
    /// es inestable entre sesiones, así que si el índice no entrega el
    /// formato exacto se escaldean otros. Un dispositivo sin el modo
    /// pedido falla el open en vez de mostrar la cámara equivocada.
    allow_index_scan: bool,
    /// Ajuste de color aplicado EN EL THREAD DE CAPTURA (post-decode),
    /// no en el loop de render: 2M píxeles de float por frame no deben
    /// pisar el presupuesto del hilo de UI (judder). Calibrado por el
    /// usuario contra el monitor físico; persiste en ~/.scalapp_color.
    color_adjust: Arc<Mutex<(f32, f32)>>,
    /// Trace de eventos (SCALAPP_TRACE) — stores acá, paints desde el
    /// renderer via trace_log().
    trace: Arc<Mutex<Trace>>,
    /// Notificador de frame nuevo (Fase 2-lite): el thread de captura lo
    /// llama al almacenar cada frame — el renderer lo usa para disparar
    /// el repaint EXACTO cuando hay algo nuevo que pintar (adiós a
    /// perder frames por ráfagas entre repaints por timer).
    frame_notifier: Arc<Mutex<Option<Box<dyn Fn() + Send>>>>,
    /// Escapa la veda de webcams (--force-webcam) para casos exóticos
    /// (capturadora con nombre que dispara la heurística).
    pub allow_webcam: bool,
    /// Single-slot latest-wins: el consumidor toma SIEMPRE el último frame,
    /// la latencia de cola es 0 por construcción.
    latest: Arc<Mutex<Option<Arc<TimestampedFrame>>>>,
    frame_seq: Arc<AtomicU64>,
    /// Último frame_number mostrado en pantalla (lo marca el renderer).
    displayed: Arc<AtomicU64>,
    /// Instant de arranque del reloj de timestamps; permite medir la edad
    /// de un frame desde fuera del thread de captura.
    clock_origin: Arc<Mutex<Option<Instant>>>,
    stats: Arc<Mutex<StatsInner>>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Señal de video caída (unplug USB): el supervisor la activa al
    /// perder el stream y la desactiva al reconectar — el renderer la
    /// usa para el cartel "SIN SEÑAL" sobre el último frame.
    signal_lost: Arc<AtomicBool>,
    /// Nombre human del device de la última sesión exitosa — permite
    /// re-encontrarlo tras replug aunque cambie el uniqueID.
    device_name: Arc<Mutex<Option<String>>>,
    /// uniqueID "rebotado": tras un replug el device puede reaparecer
    /// con OTRO uniqueID (el prefijo de locationID depende del puerto —
    /// visto en terreno 0x2000…→0x1340…; el sufijo de serial sí es
    /// estable). El supervisor guarda acá el uid fresco; open_camera
    /// lo prefiere por sobre el original.
    rebound_uid: Arc<Mutex<Option<String>>>,
    /// Cap de PRESENTACIÓN (modo económico --30): 0 = sin cap. El
    /// device streamea a su mejor tasa (pedirle menos fps no es seguro:
    /// por el bug del binding solo los modos "última-por-dimensión"
    /// son pedibles, y p.ej. 1080p es @60) — el puller saltea frames
    /// ANTES de decodificar: decode/textura/presentación a la mitad.
    fps_cap: Arc<AtomicU32>,
}

impl VideoCapture {
    pub fn new(device: CameraIndex, width: u32, height: u32, fps: u32) -> Self {
        Self {
            device,
            width,
            height,
            fps,
            allow_index_scan: true,
            frame_notifier: Arc::new(Mutex::new(None)),
            color_adjust: Arc::new(Mutex::new((1.0, 1.0))),
            trace: Arc::new(Mutex::new(Trace::from_env())),
            allow_webcam: false,
            latest: Arc::new(Mutex::new(None)),
            frame_seq: Arc::new(AtomicU64::new(0)),
            displayed: Arc::new(AtomicU64::new(0)),
            clock_origin: Arc::new(Mutex::new(None)),
            stats: Arc::new(Mutex::new(StatsInner::new())),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
            signal_lost: Arc::new(AtomicBool::new(false)),
            device_name: Arc::new(Mutex::new(None)),
            rebound_uid: Arc::new(Mutex::new(None)),
            fps_cap: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Registra el callback de "frame nuevo" (lo llama el thread de
    /// captura por frame almacenado). El renderer instala
    /// `ctx.request_repaint()` para pintar exactamente cuando llega.
    /// Ajuste de color (gain de R, saturación) aplicado en el thread de
    /// captura — lo alimenta el renderer desde los sliders.
    pub fn set_color_adjust(&self, red_gain: f32, saturation: f32) {
        *self.color_adjust.lock().unwrap() = (red_gain, saturation);
    }

    /// Tasa de frames medida en el RELOJ DEL DEVICE (capture_timestamp
    /// de AVFoundation): (último - primero) / muestras de la ventana.
    /// Es el período REAL de la señal (59.94 vs 60.00 etc.) — el pacer
    /// del renderer se ancla a esto, no a nuestro consumo.
    pub fn device_rate_ms(&self) -> f32 {
        let stats = self.stats.lock().unwrap();
        stats.device_pacing.window_rate_ms()
    }

    pub fn set_frame_notifier(&self, cb: Option<Box<dyn Fn() + Send>>) {
        *self.frame_notifier.lock().unwrap() = cb;
    }

    /// Desactiva el escaneo de índices cuando el usuario eligió el
    /// dispositivo explícitamente (--video-device N).
    pub fn set_allow_index_scan(&mut self, allow: bool) {
        self.allow_index_scan = allow;
    }

    pub fn start(self: &Arc<Self>) -> Result<(u32, u32, f32), String> {
        if self.running.load(Ordering::Relaxed) {
            return Ok((self.width, self.height, self.fps as f32));
        }
        self.running.store(true, Ordering::Relaxed);

        let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, u32, f32), String>>();
        let this = Arc::clone(self);
        let handle = std::thread::spawn(move || this.capture_loop(tx));

        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => {
                *self.handle.lock().unwrap() = Some(handle);
                println!("Video capture started");
                result
            }
            Err(_) => {
                self.running.store(false, Ordering::Relaxed);
                let _ = handle.join();
                Err("video capture thread did not initialize".into())
            }
        }
    }

    /// Abre la cámara pidiendo un modo que EXISTE en el device.
    ///
    /// 1. Pre-validación por AVFoundation (sin HID): leer la lista real de
    ///    modos por uniqueID y elegir el mejor (exacto si hay; si no —
    ///    USB2 — el más cercano). Pedir solo modos reales evita el bug del
    ///    binding: set_all lanza NSException con modos fantasma y deja el
    ///    device lockeado dentro del proceso.
    /// 2. Cadena legacy blindada (Index explícito o sin lista):
    ///    Exact → escaneo → Closest YUYV → Closest MJPEG, todo bajo
    ///    catch_unwind.
    fn open_camera(&self) -> Result<Camera, String> {
        // Reconnect: si el device reapareció con OTRO uniqueID (cambio
        // de puerto/ubicación USB), usar el uid fresco del supervisor.
        let device: CameraIndex = self
            .rebound_uid
            .lock()
            .unwrap()
            .clone()
            .map(CameraIndex::String)
            .unwrap_or_else(|| self.device.clone());
        // VEDA DE WEBCAMS: clasificar el dispositivo objetivo ANTES de
        // abrirlo. Ningún camino de selección puede terminar en una
        // webcam/virtual (el bug histórico: fallback a ciegas → FaceTime
        // a 30fps mostrada como si fuera la capturadora).
        let uid: Option<String> = match &device {
            CameraIndex::String(s) => Some(s.clone()),
            CameraIndex::Index(i) => nokhwa_bindings_macos::query_avfoundation()
                .ok()
                .and_then(|v| v.into_iter().nth(*i as usize))
                .map(|info| info.misc().to_string()),
        };
        if let (Some(uid), false) = (&uid, self.allow_webcam) {
            if let Some(entry) = devices::video_device_map()
                .into_iter()
                .find(|e| &e.uid == uid)
            {
                match entry.class {
                    devices::DeviceClass::Webcam | devices::DeviceClass::Virtual => {
                        return Err(format!(
                            "'{}' está clasificada como {:?} — veda anti-webcam \
                             (usá el mapa del arranque para elegir, o --force-webcam)",
                            entry.name, entry.class
                        ));
                    }
                    _ => {}
                }
            }
        }
        let uid: Option<String> = match &device {
            CameraIndex::String(s) => Some(s.clone()),
            CameraIndex::Index(i) => nokhwa_bindings_macos::query_avfoundation()
                .ok()
                .and_then(|v| v.into_iter().nth(*i as usize))
                .map(|info| info.misc().to_string()),
        };
        if let Some(uid) = uid {
            if let Some(mode) =
                devices::best_mode_for_device(&uid, (self.width, self.height, self.fps))
            {
                if mode.resolution().width_x == self.width
                    && mode.resolution().height_y == self.height
                    && mode.frame_rate() == self.fps
                {
                    println!(
                        "Pre-validated exact {}x{}@{} {:?}",
                        self.width,
                        self.height,
                        self.fps,
                        mode.format()
                    );
                } else {
                    println!(
                        "⚠ exact {}x{}@{} unavailable — best real mode: {}x{}@{} {:?}",
                        self.width,
                        self.height,
                        self.fps,
                        mode.resolution().width_x,
                        mode.resolution().height_y,
                        mode.frame_rate(),
                        mode.format()
                    );
                    // PORQUÉ del fallback (episodio 7/11): si el device
                    // se linkeó a USB2, su lista de modos directamente
                    // no tiene 1080p60 — avisar causa y remedio
                    if let Some(entry) = devices::video_device_map()
                        .into_iter()
                        .find(|e| e.uid == uid)
                    {
                        if entry.name.to_lowercase().contains("usb2") {
                            println!(
                                "⚠ '{}' = linkeada a USB2: {}x{}@{} necesita puerto/cable USB3",
                                entry.name, self.width, self.height, self.fps
                            );
                        }
                    }
                    let ar = f64::from(mode.resolution().width_x) / f64::from(mode.resolution().height_y);
                    let want_ar = f64::from(self.width) / f64::from(self.height);
                    if (ar - want_ar).abs() > 0.02 {
                        println!(
                            "⚠ sin modos {:.2}:1 disponibles — usando {:.2}:1 (la imagen se verá con barras o estirada)",
                            want_ar, ar
                        );
                    }
                }
                if let Ok(camera) = devices::try_camera_new_retry(
                    device.clone(),
                    RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(mode)),
                    3,
                ) {
                    return Ok(camera);
                }
                println!("pre-validated open failed — falling back to nokhwa negotiation");
            }
        }

        let wanted = CameraFormat::new_from(self.width, self.height, FrameFormat::YUYV, self.fps);
        let exact = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(wanted));

        match devices::try_camera_new(device.clone(), exact) {
            Ok(camera) => return Ok(camera),
            Err(e) => println!(
                "Exact format {}x{}@{} YUYV rejected on {:?}: {e}",
                self.width, self.height, self.fps, device
            ),
        }

        if self.allow_index_scan {
            if let CameraIndex::Index(primary) = device {
                for idx in 0..5u32 {
                    if idx == primary {
                        continue;
                    }
                    match devices::try_camera_new_retry(CameraIndex::Index(idx), exact, 0) {
                        Ok(camera) => {
                            // NUNCA caer a una webcam (clasificación del
                            // mapa: Virtual/Webcam vetadas)
                            let name = camera.info().human_name().to_lowercase();
                            if matches!(
                                devices::classify_device_name(&name),
                                devices::DeviceClass::Webcam | devices::DeviceClass::Virtual
                            ) {
                                continue;
                            }
                            println!("Exact format negotiated at alternate index {idx} ({})", camera.info().human_name());
                            return Ok(camera);
                        }
                        Err(_) => continue,
                    }
                }
                println!("No device delivered the exact format at any index");
            }
        }

        println!("Falling back to closest match on {:?}", device);
        if let Ok(camera) = devices::try_camera_new(
            device.clone(),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(wanted)),
        ) {
            return Ok(camera);
        }

        println!("No YUYV mode available — trying closest MJPEG");
        let mjpeg = CameraFormat::new_from(self.width, self.height, FrameFormat::MJPEG, self.fps);
        devices::try_camera_new(
            device.clone(),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(mjpeg)),
        )
        .map_err(|e| format!("cannot open camera device {:?}: {e}", device))
    }

    /// Sesión de captura: abre la cámara y streamea hasta que la señal
    /// muera o cierre la app. Corre en el thread PULLER, que POSEE la
    /// cámara. `activity` se actualiza con cada frame OK (el supervisor
    /// lo vigila por tiempo); `stop` corta entre frames al cerrar.
    ///
    /// nokhwa/AVFoundation NO reporta el unplug: `frame()` es un
    /// `flume recv()` SIN timeout — cuando el USB se cae, esta función
    /// queda colgada ADENTRO de nokhwa para siempre (ni siquiera
    /// devuelve Err). El supervisor lo detecta por inactividad y lo
    /// reemplaza; este thread queda detached (leak documentado, acotado
    /// a uno por caída — nokhwa no expone forma de interrumpirlo).
    fn puller_session(
        &self,
        init_tx: std::sync::mpsc::Sender<Result<(u32, u32, f32), String>>,
        activity: &AtomicU64,
        stop: &AtomicBool,
    ) {
        println!("Video capture thread started");
        let mut camera = match self.open_camera() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot open camera device {:?}: {e}", self.device);
                let _ = init_tx.send(Err(msg));
                return;
            }
        };
        if let Err(e) = camera.open_stream() {
            let msg = format!("cannot start camera stream: {e}");
            let _ = init_tx.send(Err(msg));
            return;
        }
        let resolution = camera.resolution();
        let actual_fps = camera.frame_rate() as f32;
        let start_time = Instant::now();
        // dump crudo de YUYV para diagnóstico de color (SCALAPP_RAW_DUMP)
        let raw_dump = std::env::var("SCALAPP_RAW_DUMP").ok();
        let raw_count = Arc::new(Mutex::new(0u32));
        *self.clock_origin.lock().unwrap() = Some(start_time);
        // actividad desde el arranque: el supervisor no debe declarar
        // stall entre el init y el primer frame
        activity.store(now_ms(), Ordering::Relaxed);
        let _ = init_tx.send(Ok((resolution.width_x, resolution.height_y, actual_fps)));
        // nombre para el re-encuentro tras replug (el uniqueID puede
        // cambiar de puerto a puerto; el nombre no)
        *self.device_name.lock().unwrap() = Some(camera.info().human_name().to_string());
        // umbrales de pacing relativos al intervalo EFECTIVO de
        // presentación (con --30, 33.3ms es lo normal — no un hueco)
        {
            let cap = self.fps_cap.load(Ordering::Relaxed) as f32;
            let eff = if cap > 0.0 && cap < actual_fps {
                cap
            } else {
                actual_fps
            };
            let mut stats = self.stats.lock().unwrap();
            stats.device_pacing.set_expected_fps(eff);
            stats.pull_pacing.set_expected_fps(eff);
        }
        println!(
            "Camera setup: {}x{} @ {actual_fps:.2} FPS (requested exact {}x{}@{} YUYV, device: {})",
            resolution.width_x,
            resolution.height_y,
            self.width,
            self.height,
            self.fps,
            camera.info().human_name()
        );

        // Err sostenido ≈1.5s = sesión muerta que SÍ reporta (p.ej. el
        // device lo tomó otro proceso): cortar y dejar que el
        // supervisor decida. El unplug silencioso (hang de recv) lo
        // caza el supervisor por stall.
        let mut errors = 0u32;
        // diezmado del modo económico: contador de frames del DEVICE
        let mut device_seq: u64 = 0;
        while self.running.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
            match camera.frame() {
                Ok(frame_buffer) => {
                    activity.store(now_ms(), Ordering::Relaxed);
                    errors = 0;
                    // --30: saltear ANTES de decodificar. La actividad
                    // ya se marcó (el device está vivo aunque saltemos).
                    device_seq += 1;
                    let cap = self.fps_cap.load(Ordering::Relaxed);
                    let step = decimate_step(cap, actual_fps);
                    if step > 1 && device_seq % step != 0 {
                        continue;
                    }
                    // dump crudo para diagnóstico de color (SCALAPP_RAW_DUMP):
                    // guarda N frames YUYV sin decodificar
                    if let Some(path) = &raw_dump {
                        let mut d = raw_count.lock().unwrap();
                        if *d < 5 {
                            let p = format!("{}_{:02}.yuyv", path, *d);
                            let _ = std::fs::write(&p, frame_buffer.buffer());
                            println!("[raw-dump] {p} ({} bytes)", frame_buffer.buffer().len());
                            *d += 1;
                        }
                    }
                    // decode YUYV→RGBA en UNA pasada (decode + color + alpha):
                    // reemplaza las 3 pasadas anteriores (nokhwa decode →
                    // color adjust → RGB→RGBA del renderer)
                    let (rg, sat) = *self.color_adjust.lock().unwrap();
                    let resolution = frame_buffer.resolution();
                    let rgba = decode_yuyv_to_rgba(frame_buffer.buffer(), rg, sat);
                    {
                        let width = resolution.width_x;
                        let height = resolution.height_y;
                        let pull_ts = start_time.elapsed().as_secs_f64();
                        // reloj del DEVICE (capture_timestamp de AVFoundation):
                        // la llegada REAL de la señal — no nuestro ritmo de
                        // consumo. Si difiere del pull, la ráfaga nace en la
                        // entrega de buffers, no en la fuente.
                        let dev_ts = frame_buffer
                            .capture_timestamp()
                            .map(|d| d.as_secs_f64());
                        let frame = TimestampedFrame {
                            data: Arc::new(rgba),
                            width,
                            height,
                            timestamp: pull_ts,
                            frame_number: self.frame_seq.fetch_add(1, Ordering::Relaxed),
                        };
                        let frame_no = frame.frame_number;
                        store_frame(&self.latest, &self.displayed, &self.stats, frame);
                        self.trace.lock().unwrap().log(
                            "store",
                            &format!(
                                "frame={frame_no} dev={:.3} pull={pull_ts:.3}",
                                dev_ts.unwrap_or(0.0)
                            ),
                        );
                        // Fase 2-lite: el arrival del frame dispara el
                        // repaint del renderer (pintar exactamente cuando
                        // hay frame nuevo, no cuando el timer se dé)
                        if let Some(cb) = self.frame_notifier.lock().unwrap().as_ref() {
                            cb();
                        }
                        let mut stats = self.stats.lock().unwrap();
                        stats.last_frame_time = Some(pull_ts);
                        if let Some(d) = dev_ts {
                            stats.device_pacing.record(d);
                        }
                        stats.pull_pacing.record(pull_ts);
                        stats.fps_frame_count += 1;
                        let now = Instant::now();
                        let elapsed = now.duration_since(stats.last_fps_time).as_secs_f64();
                        if elapsed >= 1.0 {
                            stats.fps_actual = stats.fps_frame_count as f32 / elapsed as f32;
                            stats.fps_frame_count = 0;
                            stats.last_fps_time = now;
                        }
                        stats.frame_count = self.frame_seq.load(Ordering::Relaxed);
                    }
                },
                Err(_) => {
                    errors += 1;
                    if errors == 1 || errors % 100 == 0 {
                        println!("Failed to read frame from camera ({errors})");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    if errors >= 150 {
                        break;
                    }
                }
            }
        }
        let _ = camera.stop_stream();
    }

    /// SUPERVISOR de captura (antes era el capture_loop monolítico).
    ///
    /// Por sesión: spawnea un PULLER que posee la cámara y vigila su
    /// actividad. Caída de señal (stall de frames o errores sostenidos)
    /// → descarta la sesión, cartel "SIN SEÑAL", espera a que el device
    /// reaparezca en AVFoundation (poll del uniqueID, sin abrir) y
    /// REABRE con toda la cadena de validación de siempre (veda
    /// anti-webcam, modo pre-validado, retries ante Lock Rejected).
    /// El arranque inicial fallido mantiene el comportamiento
    /// histórico: error limpio y la app termina.
    fn capture_loop(
        self: &Arc<Self>,
        tx: std::sync::mpsc::Sender<Result<(u32, u32, f32), String>>,
    ) {
        println!("Video capture supervisor started");
        let stall_ms: u64 = std::env::var("SCALAPP_VIDEO_STALL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        let mut first_session = true;

        while self.running.load(Ordering::Relaxed) {
            // ── sesión nueva: puller con su propia cámara ──
            let activity = Arc::new(AtomicU64::new(0));
            let stop_flag = Arc::new(AtomicBool::new(false));
            let (init_tx, init_rx) =
                std::sync::mpsc::channel::<Result<(u32, u32, f32), String>>();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            {
                let vc = Arc::clone(self);
                let act = Arc::clone(&activity);
                let stp = Arc::clone(&stop_flag);
                let _ = std::thread::Builder::new()
                    .name("video-puller".into())
                    .spawn(move || {
                        vc.puller_session(init_tx, &act, &stp);
                        // done_tx se dropea acá: despierta al supervisor
                        drop(done_tx);
                    });
            }

            // ── arranque de la sesión (open con retries incluidos) ──
            let mut session_alive = true;
            match init_rx.recv_timeout(Duration::from_secs(20)) {
                Ok(Ok((w, h, fps))) => {
                    if first_session {
                        let _ = tx.send(Ok((w, h, fps)));
                        first_session = false;
                    } else {
                        println!("[camera] ✓ stream de video RESTAURADO ({w}x{h}@{fps:.0})");
                        self.signal_lost.store(false, Ordering::Relaxed);
                    }
                }
                Ok(Err(msg)) => {
                    if first_session {
                        let _ = tx.send(Err(msg));
                        self.running.store(false, Ordering::Relaxed);
                        return;
                    }
                    println!("[camera] reconexión falló ({msg}) — reintentando…");
                    session_alive = false;
                }
                Err(_) => {
                    if first_session {
                        let _ = tx.send(Err("video camera init timeout".into()));
                        self.running.store(false, Ordering::Relaxed);
                        return;
                    }
                    println!("[camera] init timeout en reconexión — reintentando…");
                    session_alive = false;
                }
            }

            // ── vigilancia: fin del puller o stall de frames ──
            let mut cause = "";
            if session_alive {
                loop {
                    if !self.running.load(Ordering::Relaxed) {
                        // cierre de la app: cortar el puller y salir todo
                        stop_flag.store(true, Ordering::Relaxed);
                        let _ = done_rx.recv_timeout(Duration::from_secs(2));
                        break;
                    }
                    if done_rx.try_recv().is_ok() {
                        cause = "errores de frame sostenidos";
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    let last = activity.load(Ordering::Relaxed);
                    if last != 0 && now_ms().saturating_sub(last) > stall_ms {
                        stop_flag.store(true, Ordering::Relaxed);
                        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                            // colgado DENTRO de nokhwa (recv sin timeout):
                            // thread abandonado — leak documentado arriba.
                            println!(
                                "[camera] puller colgado en recv() de nokhwa — thread abandonado"
                            );
                        }
                        cause = "sin frames (¿USB desconectado?)";
                        break;
                    }
                }
                if !self.running.load(Ordering::Relaxed) {
                    break; // cierre limpio de la app
                }
            } else {
                // init de reconexión falló (p.ej. otro proceso tomó el
                // device apenas reapareció) — NO es cierre de app:
                // seguir reintentando en el ciclo de recuperación.
                cause = "fallo de reconexión";
            }

            // ── señal perdida: cartel en pantalla + esperar el replug ──
            self.signal_lost.store(true, Ordering::Relaxed);
            *self.latest.lock().unwrap() = None;
            self.stats.lock().unwrap().fps_actual = 0.0;
            println!("[camera] ✗ señal de video perdida ({cause}) — esperando reconexión…");

            // Esperar a que el device reaparezca (matching tolerante a
            // cambio de uniqueID por puerto — ver wait_device_back).
            self.wait_device_back();
            // settle: la MS2131 re-enumera formatos al volver al bus;
            // no martillar el open (salud del hardware marginal).
            std::thread::sleep(Duration::from_millis(1500));
        }
        println!("Video capture thread stopped");
    }

    /// Espera a que la capturadora reaparezca en AVFoundation tras un
    /// replug, SIN abrirla (solo verla en el mapa — barato).
    ///
    /// Matching tolerante a que CAMBIE el uniqueID (el prefijo de
    /// locationID depende del puerto — visto 0x2000…→0x1340… en
    /// terreno; el sufijo de serial es el estable): mismo uid, mismo
    /// sufijo de 8, o mismo nombre en clase Capture. Si reaparece con
    /// otro uid, lo guarda en `rebound_uid` para que open_camera use el
    /// fresco (y lo limpia si volvió al uid original).
    fn wait_device_back(&self) {
        let orig_uid = match &self.device {
            CameraIndex::String(s) => Some(s.clone()),
            _ => None,
        };
        let want_name = self.device_name.lock().unwrap().clone();
        let suffix: Option<String> = orig_uid
            .as_ref()
            .filter(|u| u.len() >= 8)
            .map(|u| u.chars().rev().take(8).collect());
        while self.running.load(Ordering::Relaxed) {
            let hit = devices::video_device_map().into_iter().find(|e| {
                if matches!(
                    e.class,
                    devices::DeviceClass::Webcam | devices::DeviceClass::Virtual
                ) {
                    return false; // veda anti-webcam también acá
                }
                if let Some(uid) = &orig_uid {
                    if &e.uid == uid {
                        return true;
                    }
                }
                if let Some(sfx) = &suffix {
                    let tail: String = e.uid.chars().rev().take(8).collect();
                    if sfx == &tail {
                        return true;
                    }
                }
                // por nombre solo en clase Capture (Unknown + nombre
                // genérico podría matchear cualquier cosa)
                if e.class == devices::DeviceClass::Capture {
                    if let Some(name) = &want_name {
                        if e.name.starts_with(name.as_str()) || name.starts_with(&e.name) {
                            return true;
                        }
                    }
                }
                false
            });
            match hit {
                Some(entry) => {
                    if let Some(orig) = &orig_uid {
                        if entry.uid == *orig {
                            *self.rebound_uid.lock().unwrap() = None;
                        } else {
                            println!(
                                "[camera] device reapareció con OTRO uniqueID ({} → {}) — usando el nuevo",
                                orig, entry.uid
                            );
                            *self.rebound_uid.lock().unwrap() = Some(entry.uid.clone());
                        }
                    }
                    return;
                }
                None => std::thread::sleep(Duration::from_millis(1000)),
            }
        }
    }

    pub fn stop(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        println!("Video capture stopped");
    }

    /// ¿Está el stream de video caído (esperando reconexión del USB)?
    pub fn is_signal_lost(&self) -> bool {
        self.signal_lost.load(Ordering::Relaxed)
    }

    /// Cap de presentación en fps (0 = sin cap, modo --30 = 30).
    pub fn set_fps_cap(&self, cap: u32) {
        self.fps_cap.store(cap, Ordering::Relaxed);
        if cap > 0 {
            println!(
                "✓ Presentación a {cap}fps (--30): device a su mejor tasa, \
                 se saltean frames antes de decodificar (menos CPU/GPU)"
            );
        }
    }

    /// Fps EFECTIVO de presentación (cap si hay, sino el del modo) —
    /// para targets de diagnóstico y overlays.
    pub fn display_fps(&self) -> u32 {
        let cap = self.fps_cap.load(Ordering::Relaxed);
        if cap > 0 && cap < self.fps {
            cap
        } else {
            self.fps
        }
    }

    /// El frame más reciente disponible (copia barata: clon de Arc).
    pub fn get_latest_frame(&self) -> Option<Arc<TimestampedFrame>> {
        self.latest.lock().unwrap().clone()
    }

    /// Marca que el renderer pintó `frame_number` (para el conteo de skips).
    pub fn mark_displayed(&self, frame_number: u64) {
        self.displayed.fetch_max(frame_number, Ordering::Relaxed);
    }

    /// Edad del frame medida con el reloj del thread de captura:
    /// cuánto tiempo pasó desde que el frame se timestampó hasta ahora.
    pub fn frame_age_secs(&self, frame: &TimestampedFrame) -> Option<f64> {
        let origin = *self.clock_origin.lock().unwrap();
        origin.map(|o| (o.elapsed().as_secs_f64() - frame.timestamp).max(0.0))
    }

    /// Traza un evento del renderer (p.ej. paint) al archivo de trace.
    pub fn trace_log(&self, event: &str, detail: &str) {
        self.trace.lock().unwrap().log(event, detail);
    }

    pub fn get_capture_stats(&self) -> CaptureStats {
        let stats = self.stats.lock().unwrap();
        CaptureStats {
            fps_actual: stats.fps_actual,
            total_frames: stats.frame_count,
            skipped_frames: stats.skipped_frames,
            last_frame_time: stats.last_frame_time,
            device_bursts: stats.device_pacing.bursts,
            device_gaps: stats.device_pacing.gaps,
            device_p50_ms: stats.device_pacing.p50_ms(),
            pull_bursts: stats.pull_pacing.bursts,
            pull_gaps: stats.pull_pacing.gaps,
            pull_p50_ms: stats.pull_pacing.p50_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u64) -> TimestampedFrame {
        TimestampedFrame {
            data: Arc::new(vec![0u8; 4]),
            width: 2,
            height: 2,
            timestamp: n as f64 / 60.0,
            frame_number: n,
        }
    }

    #[test]
    fn yuyv_to_rgba_matches_nokhwa_byte_a_byte() {
        // YUYV sintético variado (Y, U, Y, V)
        let mut yuyv = Vec::new();
        for i in 0..128u8 {
            yuyv.extend_from_slice(&[i, 30 + i % 200, 255 - i, 50 + i % 190]);
        }
        let ours = decode_yuyv_to_rgba(&yuyv, 1.0, 1.0);
        let theirs = nokhwa_core::types::yuyv422_to_rgb(&yuyv, false).unwrap();
        for (px, (o, t)) in ours
            .as_chunks::<4>()
            .0
            .iter()
            .zip(theirs.as_chunks::<3>().0)
            .enumerate()
        {
            assert_eq!(o[0], t[0], "R en px {px}");
            assert_eq!(o[1], t[1], "G en px {px}");
            assert_eq!(o[2], t[2], "B en px {px}");
            assert_eq!(o[3], 255, "alpha en px {px}");
        }
    }

    #[test]
    fn yuyv_to_rgba_saturation_zero_is_grayscale() {
        let mut yuyv = Vec::new();
        for i in 0..64u8 {
            yuyv.extend_from_slice(&[i, 30 + i % 200, 255 - i, 50 + i % 190]);
        }
        let gray = decode_yuyv_to_rgba(&yuyv, 1.0, 0.0);
        for px in gray.as_chunks::<4>().0 {
            assert_eq!(px[0], px[1], "gris: R=G");
            assert_eq!(px[1], px[2], "gris: G=B");
        }
    }

    #[test]
    fn yuyv_saturation_yuv_domain_matches_rgb_reference() {
        // El ajuste en dominio YUV coincide con la referencia float de
        // desaturación RGB (±1.5 LSB) EN LA ZONA SIN CLIPPING del decode
        // neutro. En colores saturados a tope (canal clampeado a 0/255)
        // la referencia opera sobre valores clampeados y el dominio YUV
        // no — diferencia conocida y aceptada (es praxis estándar: chroma
        // gain); la calibración del usuario es por ojo.
        let mut yuyv = Vec::new();
        for i in 0..=255u8 {
            yuyv.extend_from_slice(&[i, 60 + i % 140, 255 - i, 70 + i % 120]);
        }
        let sat = 0.75f32;
        let ours = decode_yuyv_to_rgba(&yuyv, 1.0, sat);
        let neutral = decode_yuyv_to_rgba(&yuyv, 1.0, 1.0);
        let mut comparados = 0usize;
        for (px_new, px_ref) in ours
            .as_chunks::<4>()
            .0
            .iter()
            .zip(neutral.as_chunks::<4>().0)
        {
            // solo zona sin clipping
            if px_ref.iter().take(3).any(|&c| c < 2 || c > 253) {
                continue;
            }
            comparados += 1;
            let luma = 0.299 * f32::from(px_ref[0]) + 0.587 * f32::from(px_ref[1])
                + 0.114 * f32::from(px_ref[2]);
            for ch in 0..3 {
                let expected =
                    (luma + (f32::from(px_ref[ch]) - luma) * sat).clamp(0.0, 255.0);
                let diff = (f32::from(px_new[ch]) - expected).abs();
                // tolerancia: acumulación de punto fijo del decode
                // entero (truncación por canal ≤1 LSB) + redondeo del
                // escalamiento de croma
                assert!(
                    diff <= 2.5,
                    "canal {ch}: nuevo {} vs esperado {expected}",
                    px_new[ch]
                );
            }
        }
        assert!(comparados > 100, "muy pocos píxeles comparables: {comparados}");
    }

    #[test]
    fn decimate_step_modo_economico() {
        use super::decimate_step;
        // sin cap o cap ≥ fps del device → no diezmamos
        assert_eq!(decimate_step(0, 60.0), 1);
        assert_eq!(decimate_step(60, 60.0), 1);
        assert_eq!(decimate_step(120, 60.0), 1);
        // --30 sobre 60 → 1 de cada 2
        assert_eq!(decimate_step(30, 60.0), 2);
        // cap 30 sobre 50 → 2 (redondeo)
        assert_eq!(decimate_step(30, 50.0), 2);
        // cap 24 sobre 60 → 2.5 → 3 (redondeo a nearest)
        assert_eq!(decimate_step(24, 60.0), 3);
    }

    #[test]
    fn latest_wins_overwrites_and_counts_skips() {
        let latest = Mutex::new(None);
        let displayed = AtomicU64::new(0);
        let stats = Mutex::new(StatsInner::new());

        // frames 0..=5 con nadie mirando: 1..4 mueren en el slot
        for n in 0..=5u64 {
            store_frame(&latest, &displayed, &stats, frame(n));
        }
        assert_eq!(latest.lock().unwrap().as_ref().unwrap().frame_number, 5);
        assert_eq!(stats.lock().unwrap().skipped_frames, 4);

        // el renderer muestra el 5; los siguientes sobreviven hasta que
        // llegue uno más nuevo sin ser mostrados
        displayed.store(5, Ordering::Relaxed);
        store_frame(&latest, &displayed, &stats, frame(6));
        assert_eq!(stats.lock().unwrap().skipped_frames, 4);
        store_frame(&latest, &displayed, &stats, frame(7));
        assert_eq!(stats.lock().unwrap().skipped_frames, 5);
    }
}
