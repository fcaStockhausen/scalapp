use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, FontId, pos2, vec2, Rect};

use crate::audio::AudioStreams;
use crate::sync::SyncController;
use crate::video::VideoCapture;

/// Contador de fps genérico (Fase 1: medir lo que llega a pantalla).
struct FpsMeter {
    count: u64,
    fps: f32,
    last: Instant,
    report: Instant,
}

impl FpsMeter {
    fn new() -> Self {
        Self {
            count: 0,
            fps: 0.0,
            last: Instant::now(),
            report: Instant::now(),
        }
    }

    fn tick(&mut self) {
        self.count += 1;
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.count as f32 / elapsed;
            self.count = 0;
            self.last = now;
        }
    }
}

/// Línea de salud del pipeline cada 15s: cap (entrega del device) vs
/// out (frames nuevos pintados) vs loop (repaints) — la métrica
/// fundamental del direct output, visible en consola.
struct VideoHealth {
    last: Instant,
    /// contadores previos para reportar DELTAS por ventana (los
    /// contadores de origen son acumulativos)
    prev_skip: u64,
    prev_dev_bursts: u64,
    prev_dev_gaps: u64,
    prev_pull_bursts: u64,
    prev_pull_gaps: u64,
    prev_paint_bursts: u64,
    prev_paint_gaps: u64,
}

impl VideoHealth {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            prev_skip: 0,
            prev_dev_bursts: 0,
            prev_dev_gaps: 0,
            prev_pull_bursts: 0,
            prev_pull_gaps: 0,
            prev_paint_bursts: 0,
            prev_paint_gaps: 0,
        }
    }

    fn due(&self) -> bool {
        self.last.elapsed().as_secs() >= 15
    }

    fn maybe_report(
        &mut self,
        cap_fps: f32,
        out_fps: f32,
        loop_fps: f32,
        skipped: u64,
        device: (u64, u64, f32),
        pull: (u64, u64, f32),
        paint: (u64, u64, f32),
        paint_tail: (f32, f32),
        target: u32,
    ) {
        if self.last.elapsed().as_secs() < 15 {
            return;
        }
        let (skip, db, dg, pb_, pg, pab, pag) = (
            skipped - self.prev_skip,
            device.0 - self.prev_dev_bursts,
            device.1 - self.prev_dev_gaps,
            pull.0 - self.prev_pull_bursts,
            pull.1 - self.prev_pull_gaps,
            paint.0 - self.prev_paint_bursts,
            paint.1 - self.prev_paint_gaps,
        );
        self.prev_skip = skipped;
        self.prev_dev_bursts = device.0;
        self.prev_dev_gaps = device.1;
        self.prev_pull_bursts = pull.0;
        self.prev_pull_gaps = pull.1;
        self.prev_paint_bursts = paint.0;
        self.prev_paint_gaps = paint.1;
        println!(
            "[video @{}] cap {:.1} | out {:.1} | loop {:.1} | skip +{skip} | DEV: +{db}ráf +{dg}huec p50 {:.1} | PULL: +{pb_}ráf +{pg}huec p50 {:.1} | pintado: +{pab}ráf +{pag}huec p10 {:.1} p50 {:.1} p90 {:.1}ms (target {target})",
            crate::audio::wall_clock(),
            cap_fps,
            out_fps,
            loop_fps,
            device.2,
            pull.2,
            paint_tail.0,
            paint.2,
            paint_tail.1,
        );
        self.last = Instant::now();
    }
}

pub struct RendererApp {
    sync: Arc<SyncController>,
    video: Arc<VideoCapture>,
    texture: egui::TextureHandle,
    last_frame_number: u64,
    /// Diagnóstico de pacing de PINTADO (judder): deltas entre frames
    /// nuevos pintados.
    last_paint: Option<Instant>,
    paint_bursts: u64,
    paint_gaps: u64,
    paint_deltas_ms: Vec<f32>,
    /// Buffer RGBA reutilizado frame a frame (sin realloc por frame).
    texture_size: [usize; 2],
    show_overlay: bool,
    show_stats: bool,
    volume_pct: f32,
    borderless: bool,
    status_text: String,
    audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
    /// FRAME PACER: presentar cada frame en su deadline propio
    /// (ancla + n·tasa_medida_del_device) en vez de "cuando el loop
    /// llegue". Desacopla la cadencia de presentación del ritmo del
    /// update loop: con el loop a ~11ms inconmensurable con 16.7ms, la
    /// latencia de pintado oscilaba 0-11ms a plena amplitud (= patrón
    /// bimodal 5.7/27.7ms medido en el trace). El pacer fija la FASE.
    pace_anchor: Option<(u64, Instant)>, // (frame_number ancla, instante)
    pace_rate_ms: f32,                   // tasa medida del device (DEV)
    next_deadline: Option<Instant>,
    /// Contador de fps del RENDER (frames pintados).
    painted_fps: FpsMeter,
    /// Repaints por segundo del loop de egui.
    loop_fps: FpsMeter,
    /// Línea de salud cada 15s en consola.
    health: VideoHealth,
    /// Ajuste de color por ojo (la MS2131 + YUYV puede correr levemente
    /// al rojo vs el monitor físico — referencia del usuario):
    /// red_gain multiplica el canal R, saturation mezcla hacia la luma.
    /// 1.0/1.0 = neutro. Persiste en ~/.scalapp_color.
    red_gain: f32,
    saturation: f32,
}

impl RendererApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        sync: Arc<SyncController>,
        video: Arc<VideoCapture>,
        audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
        borderless: bool,
    ) -> Self {
        let texture = cc.egui_ctx.load_texture(
            "video",
            egui::ColorImage::new([1, 1], Color32::BLACK),
            egui::TextureOptions::LINEAR,
        );
        // Fase 2-lite: cada frame que llega dispara el repaint — el
        // render ocurre exactamente cuando hay algo nuevo que pintar
        // (los bursts del device ya no se pierden entre timers).
        //
        // CON 1ms, NO con request_repaint(): egui DUPLICA por diseño
        // todo repaint de delay cero (context.rs: "Each request results
        // in two repaints, just to give some things time to settle" —
        // un parche para tooltips/respuestas retardadas de UI). Con 60
        // llegadas/s eso son 120 pasadas de egui/s = el doble de CPU,
        // GPU y WindowServer presentando contenido idéntico. Con
        // delay>0 no hay duplicación y el callback dispara igual de
        // inmediato (egui resta predicted_dt del delay).
        let ctx_for_video = cc.egui_ctx.clone();
        video.set_frame_notifier(Some(Box::new(move || {
            ctx_for_video
                .request_repaint_after(Duration::from_millis(1));
        })));
        let mut app = Self {
            sync,
            video,
            texture,            last_frame_number: u64::MAX,
            last_paint: None,
            paint_bursts: 0,
            paint_gaps: 0,
            paint_deltas_ms: Vec::new(),
            texture_size: [1, 1],
            show_overlay: false,
            show_stats: false,
            // 67% de la escala UI = 20% físico (mantiene la sonoridad
            // default de siempre dentro del nuevo mapeo 0-100 → 0-30)
            volume_pct: crate::audio::actual_volume_to_user(0.2) * 100.0,
            borderless,
            status_text: "Initializing...".into(),
            audio_streams,
            painted_fps: FpsMeter::new(),
            loop_fps: FpsMeter::new(),
            health: VideoHealth::new(),
            pace_anchor: None,
            pace_rate_ms: 16.7,
            next_deadline: None,
            red_gain: 1.0,
            saturation: 1.0,
        };
        app.load_color_adjust();
        // empujar el ajuste cargado al thread de captura (ahí se aplica)
        app.video
            .set_color_adjust(app.red_gain, app.saturation);
        app
    }

    fn color_adjust_path() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
        p.push(".scalapp_color");
        p
    }

    fn load_color_adjust(&mut self) {
        let Ok(text) = std::fs::read_to_string(Self::color_adjust_path()) else {
            return;
        };
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("red ") {
                if let Ok(f) = v.trim().parse::<f32>() {
                    self.red_gain = f.clamp(0.5, 1.5);
                }
            }
            if let Some(v) = line.strip_prefix("sat ") {
                if let Ok(f) = v.trim().parse::<f32>() {
                    self.saturation = f.clamp(0.0, 1.5);
                }
            }
        }
        println!(
            "✓ Ajuste de color: rojo x{:.2} saturación x{:.2}",
            self.red_gain, self.saturation
        );
    }

    fn save_color_adjust(&self) {
        let _ = std::fs::write(
            Self::color_adjust_path(),
            format!("red {:.3}\nsat {:.3}\n", self.red_gain, self.saturation),
        );
    }

    /// Latest-wins: toma SIEMPRE el último frame del slot de captura.
    fn pull_frame(&mut self) {
        let frame = match self.video.get_latest_frame() {
            Some(f) => f,
            None => return,
        };
        if frame.frame_number == self.last_frame_number {
            return;
        }
        // PACING v3 (lección de 3 iteraciones): las llegadas ya son
        // perfectas (DEV p50 16.7 limpio SIEMPRE) — pintar EN LA LLEGADA
        // da latencia constante ≈ 0 y cadencia = cadencia de la señal.
        // El judder histórico nacía del TIMER de 8ms (grilla
        // inconmensurable con 16.7 compitiendo con las llegadas); el
        // pacer de deadlines agregó una TERCERA grilla que peleaba igual.
        // Sin gate, sin anclas: el arrival notifier despierta el loop,
        // esto pinta ya. El heartbeat queda a 100ms solo para la UI.
        self.paint_now(frame.width, frame.height, &frame.data);
        self.last_frame_number = frame.frame_number;
        self.video.mark_displayed(frame.frame_number);
        self.painted_fps.tick();
        self.video
            .trace_log("paint", &format!("frame={}", frame.frame_number));
        let paint_now = Instant::now();
        if let Some(prev) = self.last_paint.replace(paint_now) {
            let delta = paint_now.duration_since(prev).as_secs_f32() * 1000.0;
            if delta < 8.0 {
                self.paint_bursts += 1;
            }
            if delta > 25.0 {
                self.paint_gaps += 1;
            }
            self.paint_deltas_ms.push(delta);
            if self.paint_deltas_ms.len() > 15 * 120 {
                let drop = self.paint_deltas_ms.len() - 15 * 120;
                self.paint_deltas_ms.drain(..drop);
            }
        }
    }

    /// Subida a textura: los frames ya llegan RGBA del thread de captura
    /// (decode YUYV→RGBA en una pasada allá) — aquí solo se copian a la
    /// textura de egui. El loop de UI quedó liviano.
    fn paint_now(&mut self, w: u32, h: u32, data: &[u8]) {
        let w = w as usize;
        let h = h as usize;
        let expected = w * h * 4;
        if data.len() < expected {
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &data[..expected]);
        self.texture.set(image, egui::TextureOptions::LINEAR);
        self.texture_size = [w, h];
    }

    fn update_status(&mut self) {
        let status = self.sync.get_sync_status();
        let perf = self.sync.get_performance_stats();
        self.status_text = format!(
            "Audio: {:.2}s | Video: {:.2}s | A/V Diff: {:+.1}ms | Sync: {} | Skipped: {} | Avg Drift: {:.1}ms",
            status.audio_time,
            status.video_time,
            status.audio_video_diff * 1000.0,
            if status.is_synced { "✓" } else { "✗" },
            status.video_frames_dropped,
            perf.avg_drift_ms
        );
    }

    fn set_borderless(&mut self, ctx: &egui::Context, on: bool) {
        if on != self.borderless {
            self.borderless = on;
            // borderless = ventana SIN decoraciones (sin barra de título),
            // NO fullscreen nativo: la ventana conserva tamaño y posición
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!on));
        }
    }

    fn draw_video(&self, ui: &mut egui::Ui) {
        let available = ui.available_rect_before_wrap();
        let [tw, th] = self.texture_size;
        if tw <= 1 || th <= 1 {
            return;
        }
        let scale = (available.width() / tw as f32).min(available.height() / th as f32);
        let size = vec2(tw as f32 * scale, th as f32 * scale);
        let rect = Rect::from_center_size(available.center(), size);
        ui.painter().image(
            self.texture.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    fn draw_overlay(&self, painter: &egui::Painter) {
        let mut lines: Vec<String> = Vec::new();
        if self.show_overlay {
            if let Some(frame) = self.video.get_latest_frame() {
                lines.push(format!("Frame: {}", frame.frame_number));
                lines.push(format!("Time: {:.3}s", frame.timestamp));
                let cap_stats = self.video.get_capture_stats();
                lines.push(format!(
                    "FPS: out {:.1} / cap {:.1} / target {}",
                    self.painted_fps.fps, cap_stats.fps_actual, self.video.fps
                ));
            }
        }
        if self.show_stats {
            let status = self.sync.get_sync_status();
            let perf = self.sync.get_performance_stats();
            let cap_stats = self.video.get_capture_stats();
            lines.push(format!(
                "Resolution: {}x{}",
                self.texture_size[0], self.texture_size[1]
            ));
            lines.push(format!(
                "FPS: cap {:.1} | out {:.1} | loop {:.1}",
                cap_stats.fps_actual, self.painted_fps.fps, self.loop_fps.fps
            ));
            if let Some(frame) = self.video.get_latest_frame() {
                if let Some(age) = self.video.frame_age_secs(&frame) {
                    lines.push(format!("Frame age: {:.1}ms", age * 1000.0));
                }
            }
            lines.push(format!("Skipped: {}", cap_stats.skipped_frames));
            lines.push(format!(
                "A/V Drift: {:+.1}ms ({})",
                status.audio_video_diff * 1000.0,
                if status.is_synced { "synced" } else { "drift" }
            ));
            lines.push(format!("Avg Drift: {:.1}ms", perf.avg_drift_ms));
        }

        let mut y = 10.0;
        for line in lines {
            let galley = painter.layout_no_wrap(line, FontId::monospace(13.0), Color32::WHITE);
            let size = galley.size() + vec2(8.0, 2.0);
            let rect = Rect::from_min_size(pos2(10.0, y), size);
            painter.rect_filled(rect, 2.0, Color32::from_black_alpha(180));
            painter.galley(pos2(14.0, y + 1.0), galley, Color32::WHITE);
            y += size.y + 1.0;
        }
    }
}

impl eframe::App for RendererApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // parche de negociación: atender pedidos de rebuild del watchdog
        if let Some(streams) = &self.audio_streams {
            streams.lock().unwrap().poll_rebuild();
        }
        self.loop_fps.tick();
        let toggle_f = ctx.input(|i| i.key_pressed(egui::Key::F));
        let toggle_o = ctx.input(|i| i.key_pressed(egui::Key::O));
        let toggle_s = ctx.input(|i| i.key_pressed(egui::Key::S));
        let quit_q = ctx.input(|i| i.key_pressed(egui::Key::Q));
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        if toggle_o {
            self.show_overlay = !self.show_overlay;
        }
        if toggle_s {
            self.show_stats = !self.show_stats;
        }
        if toggle_f {
            self.set_borderless(ctx, !self.borderless);
        }
        if quit_q {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if esc && self.borderless {
            self.set_borderless(ctx, false);
        }

        if !self.borderless {
            egui::TopBottomPanel::top("controls").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.separator();
                    ui.add(egui::Checkbox::new(&mut self.show_overlay, "Show Overlay"));
                    ui.add(egui::Checkbox::new(&mut self.show_stats, "Show Stats"));
                    ui.label("Volume:");
                    if ui
                        .add(egui::Slider::new(
                            &mut self.volume_pct,
                            0.0..=100.0,
                        ))
                        .changed()
                    {
                        // escala de UI 0-100 → rango físico 0-VOLUME_RANGE
                        // (30%): toda la resolución del slider en el
                        // intervalo que realmente se usa
                        let actual =
                            crate::audio::user_volume_to_actual(self.volume_pct / 100.0);
                        self.sync.set_audio_volume(actual);
                    }
                    ui.label(format!(
                        "(real {:.0}%)",
                        crate::audio::user_volume_to_actual(self.volume_pct / 100.0) * 100.0
                    ));
                    ui.separator();
                    if ui
                        .add(egui::Slider::new(&mut self.red_gain, 0.5..=1.5).text("Rojo"))
                        .changed()
                    {
                        self.video.set_color_adjust(self.red_gain, self.saturation);
                        self.save_color_adjust();
                    }
                    if ui
                        .add(egui::Slider::new(&mut self.saturation, 0.0..=1.5).text("Saturación"))
                        .changed()
                    {
                        self.video.set_color_adjust(self.red_gain, self.saturation);
                        self.save_color_adjust();
                    }
                    if ui.button("↺").clicked() {
                        self.red_gain = 1.0;
                        self.saturation = 1.0;
                        self.video.set_color_adjust(1.0, 1.0);
                        self.save_color_adjust();
                    }
                    let borderless_label = if self.borderless {
                        "Windowed Mode"
                    } else {
                        "Borderless Mode"
                    };
                    if ui.button(borderless_label).clicked() {
                        self.set_borderless(ctx, !self.borderless);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            });
        }

        // barra de status (Audio/Video/A-V diff): solo en modo diagnóstico
        // (Show Stats / S) y con ventana decorada — el monitor queda limpio
        if self.show_stats && !self.borderless {
            egui::TopBottomPanel::bottom("status")
                .show_separator_line(true)
                .show(ctx, |ui| {
                    ui.label(&self.status_text);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                self.draw_video(ui);
            });

        if self.show_overlay || self.show_stats {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("overlay"),
            ));
            self.draw_overlay(&painter);
        }

        self.pull_frame();
        if self.health.due() {
            // tomar los deltas SOLO al reportar (antes se drenaban por
            // update y el p50 salía de 1-2 muestras = artefacto)
            let mut paint_deltas = std::mem::take(&mut self.paint_deltas_ms);
            let mut paint_pct = |p: usize| -> f32 {
                if paint_deltas.is_empty() {
                    return 0.0;
                }
                paint_deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                paint_deltas[(paint_deltas.len() - 1) * p / 100]
            };
            let cap_stats = self.video.get_capture_stats();
            self.health.maybe_report(
                cap_stats.fps_actual,
                self.painted_fps.fps,
                self.loop_fps.fps,
                cap_stats.skipped_frames,
                (
                    cap_stats.device_bursts,
                    cap_stats.device_gaps,
                    cap_stats.device_p50_ms,
                ),
                (
                    cap_stats.pull_bursts,
                    cap_stats.pull_gaps,
                    cap_stats.pull_p50_ms,
                ),
                (self.paint_bursts, self.paint_gaps, paint_pct(50)),
                (paint_pct(10), paint_pct(90)),
                self.video.fps,
            );
        }
        self.update_status();
        // Repaint scheduling: si el PACER tiene un deadline pendiente,
        // despertar EXACTO ahí (la presentación va anclada al reloj de la
        // señal); si no, heartbeat de respaldo.
        let hb = std::env::var("SCALAPP_HEARTBEAT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(100);
        // v3: solo heartbeat bajo — el arrival notifier despierta el loop
        // por frame; NINGÚN timer rápido compitiendo con la grilla de
        // llegadas (era la causa del judder: 8ms inconmensurable con 16.7)
        ctx.request_repaint_after(Duration::from_millis(hb));
    }
}

pub fn run(
    sync: Arc<SyncController>,
    video: Arc<VideoCapture>,
    audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
    borderless: bool,
) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_decorations(!borderless)
            .with_title("ScalApp AV Monitor - Direct Output"),
        ..Default::default()
    };
    eframe::run_native(
        "scalapp-av-monitor",
        options,
        Box::new(move |cc| {
            Ok(Box::new(RendererApp::new(
                cc,
                sync,
                video,
                audio_streams,
                borderless,
            )))
        }),
    )
}
