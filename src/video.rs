use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::Camera;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    /// Diagnóstico de PACING (judder): distribución de los deltas de
    /// llegada en la ventana actual. fps promedio 60 con deltas
    /// desparejos = video que se ve a 30-40 aunque el contador diga 60.
    pub pacing_bursts: u64,
    pub pacing_gaps: u64,
    pub pacing_p50_ms: f32,
}

struct StatsInner {
    frame_count: u64,
    skipped_frames: u64,
    fps_actual: f32,
    fps_frame_count: u64,
    last_fps_time: Instant,
    last_frame_time: Option<f64>,
    // pacing: deltas de llegada (ventana móvil)
    pacing_last: Option<f64>,
    pacing_bursts: u64,
    pacing_gaps: u64,
    pacing_deltas_ms: Vec<f32>,
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
            pacing_last: None,
            pacing_bursts: 0,
            pacing_gaps: 0,
            pacing_deltas_ms: Vec::new(),
        }
    }

    /// Registra el delta entre frames consecutivos. A 60fps sano: ~16.7ms
    /// uniformes. Ráfagas (<8ms) seguidas de huecos (>25ms) = judder
    /// aunque el promedio dé 60.
    fn record_pacing(&mut self, timestamp: f64) {
        if let Some(prev) = self.pacing_last.replace(timestamp) {
            let delta_ms = ((timestamp - prev) * 1000.0) as f32;
            if delta_ms < 8.0 {
                self.pacing_bursts += 1;
            }
            if delta_ms > 25.0 {
                self.pacing_gaps += 1;
            }
            self.pacing_deltas_ms.push(delta_ms);
            // ventana móvil ~15s de deltas
            let max = 15 * 120;
            if self.pacing_deltas_ms.len() > max {
                let drop = self.pacing_deltas_ms.len() - max;
                self.pacing_deltas_ms.drain(..drop);
            }
        }
    }

    fn pacing_p50_ms(&self) -> f32 {
        if self.pacing_deltas_ms.is_empty() {
            return 0.0;
        }
        let mut v = self.pacing_deltas_ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    }
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
    /// Notificador de frame nuevo (Fase 2-lite): el thread de captura lo
    /// llama al almacenar cada frame — el renderer lo usa para disparar
    /// el repaint EXACTO cuando hay algo nuevo que pintar (adiós a
    /// perder frames por ráfagas entre repaints por timer).
    frame_notifier: Arc<Mutex<Option<Box<dyn Fn() + Send>>>>,
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
            latest: Arc::new(Mutex::new(None)),
            frame_seq: Arc::new(AtomicU64::new(0)),
            displayed: Arc::new(AtomicU64::new(0)),
            clock_origin: Arc::new(Mutex::new(None)),
            stats: Arc::new(Mutex::new(StatsInner::new())),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// Registra el callback de "frame nuevo" (lo llama el thread de
    /// captura por frame almacenado). El renderer instala
    /// `ctx.request_repaint()` para pintar exactamente cuando llega.
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
        let uid: Option<String> = match &self.device {
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
                }
                if let Ok(camera) = devices::try_camera_new(
                    self.device.clone(),
                    RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(mode)),
                ) {
                    return Ok(camera);
                }
                println!("pre-validated open failed — falling back to nokhwa negotiation");
            }
        }

        let wanted = CameraFormat::new_from(self.width, self.height, FrameFormat::YUYV, self.fps);
        let exact = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(wanted));

        match devices::try_camera_new(self.device.clone(), exact) {
            Ok(camera) => return Ok(camera),
            Err(e) => println!(
                "Exact format {}x{}@{} YUYV rejected on {:?}: {e}",
                self.width, self.height, self.fps, self.device
            ),
        }

        if self.allow_index_scan {
            if let CameraIndex::Index(primary) = self.device {
                for idx in 0..5u32 {
                    if idx == primary {
                        continue;
                    }
                    match devices::try_camera_new(CameraIndex::Index(idx), exact) {
                        Ok(camera) => {
                            println!("Exact format negotiated at alternate index {idx}");
                            return Ok(camera);
                        }
                        Err(_) => continue,
                    }
                }
                println!("No device delivered the exact format at any index");
            }
        }

        println!("Falling back to closest match on {:?}", self.device);
        if let Ok(camera) = devices::try_camera_new(
            self.device.clone(),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(wanted)),
        ) {
            return Ok(camera);
        }

        println!("No YUYV mode available — trying closest MJPEG");
        let mjpeg = CameraFormat::new_from(self.width, self.height, FrameFormat::MJPEG, self.fps);
        devices::try_camera_new(
            self.device.clone(),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(mjpeg)),
        )
        .map_err(|e| format!("cannot open camera device {:?}: {e}", self.device))
    }

    fn capture_loop(&self, tx: std::sync::mpsc::Sender<Result<(u32, u32, f32), String>>) {
        println!("Video capture thread started");
        let mut camera = match self.open_camera() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot open camera device {:?}: {e}", self.device);
                let _ = tx.send(Err(msg));
                self.running.store(false, Ordering::Relaxed);
                return;
            }
        };
        if let Err(e) = camera.open_stream() {
            let msg = format!("cannot start camera stream: {e}");
            let _ = tx.send(Err(msg));
            self.running.store(false, Ordering::Relaxed);
            return;
        }
        let resolution = camera.resolution();
        let actual_fps = camera.frame_rate() as f32;
        let start_time = Instant::now();
        // dump crudo de YUYV para diagnóstico de color (SCALAPP_RAW_DUMP)
        let raw_dump = std::env::var("SCALAPP_RAW_DUMP").ok();
        let raw_count = Arc::new(Mutex::new(0u32));
        *self.clock_origin.lock().unwrap() = Some(start_time);
        let _ = tx.send(Ok((resolution.width_x, resolution.height_y, actual_fps)));
        println!(
            "Camera setup: {}x{} @ {actual_fps:.2} FPS (requested exact {}x{}@{} YUYV, device: {})",
            resolution.width_x,
            resolution.height_y,
            self.width,
            self.height,
            self.fps,
            camera.info().human_name()
        );

        while self.running.load(Ordering::Relaxed) {
            match camera.frame() {
                Ok(frame_buffer) => {
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
                    match frame_buffer.decode_image::<RgbFormat>() {
                    Ok(image) => {
                        let width = image.width();
                        let height = image.height();
                        let timestamp = start_time.elapsed().as_secs_f64();
                        let frame = TimestampedFrame {
                            data: Arc::new(image.into_raw()),
                            width,
                            height,
                            timestamp,
                            frame_number: self.frame_seq.fetch_add(1, Ordering::Relaxed),
                        };
                        store_frame(&self.latest, &self.displayed, &self.stats, frame);
                        // Fase 2-lite: el arrival del frame dispara el
                        // repaint del renderer (pintar exactamente cuando
                        // hay frame nuevo, no cuando el timer se dé)
                        if let Some(cb) = self.frame_notifier.lock().unwrap().as_ref() {
                            cb();
                        }
                        let mut stats = self.stats.lock().unwrap();
                        stats.last_frame_time = Some(timestamp);
                        stats.record_pacing(timestamp);
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
                    Err(e) => {
                        eprintln!("Frame decode error: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    }
                },
                Err(_) => {
                    println!("Failed to read frame from camera");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        let _ = camera.stop_stream();
        println!("Video capture thread stopped");
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

    pub fn get_capture_stats(&self) -> CaptureStats {
        let stats = self.stats.lock().unwrap();
        CaptureStats {
            fps_actual: stats.fps_actual,
            total_frames: stats.frame_count,
            skipped_frames: stats.skipped_frames,
            last_frame_time: stats.last_frame_time,
            pacing_bursts: stats.pacing_bursts,
            pacing_gaps: stats.pacing_gaps,
            pacing_p50_ms: stats.pacing_p50_ms(),
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
