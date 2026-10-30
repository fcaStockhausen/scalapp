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
}

impl VideoHealth {
    fn new() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    fn maybe_report(&mut self, cap_fps: f32, out_fps: f32, loop_fps: f32, skipped: u64, target: u32) {
        if self.last.elapsed().as_secs() >= 15 {
            println!(
                "[video @{}] cap {:.1} | out {:.1} | loop {:.1} | skipped {skipped} (target {target})",
                crate::audio::wall_clock(),
                cap_fps,
                out_fps,
                loop_fps,
            );
            self.last = Instant::now();
        }
    }
}

pub struct RendererApp {
    sync: Arc<SyncController>,
    video: Arc<VideoCapture>,
    texture: egui::TextureHandle,
    last_frame_number: u64,
    /// Buffer RGBA reutilizado frame a frame (sin realloc por frame).
    rgba: Vec<u8>,
    texture_size: [usize; 2],
    show_overlay: bool,
    show_stats: bool,
    volume_pct: f32,
    borderless: bool,
    status_text: String,
    audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
    /// Contador de fps del RENDER (frames pintados).
    painted_fps: FpsMeter,
    /// Repaints por segundo del loop de egui.
    loop_fps: FpsMeter,
    /// Línea de salud cada 15s en consola.
    health: VideoHealth,
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
        let ctx_for_video = cc.egui_ctx.clone();
        video.set_frame_notifier(Some(Box::new(move || ctx_for_video.request_repaint())));
        Self {
            sync,
            video,
            texture,
            last_frame_number: u64::MAX,
            rgba: Vec::new(),
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
        }
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
        let w = frame.width as usize;
        let h = frame.height as usize;
        let expected_src = w * h * 3;
        if frame.data.len() < expected_src {
            return;
        }
        // RGB → RGBA en buffer reutilizado (el alpha queda fijado en 255)
        self.rgba.resize(w * h * 4, 255);
        let src_pixels = frame.data.as_chunks::<3>().0;
        for (dst, src) in self.rgba.as_chunks_mut::<4>().0.iter_mut().zip(src_pixels) {
            dst[0] = src[0];
            dst[1] = src[1];
            dst[2] = src[2];
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &self.rgba);
        self.texture.set(image, egui::TextureOptions::LINEAR);
        self.texture_size = [w, h];
        self.last_frame_number = frame.frame_number;
        self.video.mark_displayed(frame.frame_number);
        self.painted_fps.tick();
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
        let cap_stats = self.video.get_capture_stats();
        self.health.maybe_report(
            cap_stats.fps_actual,
            self.painted_fps.fps,
            self.loop_fps.fps,
            cap_stats.skipped_frames,
            self.video.fps,
        );
        self.update_status();
        // Fase 1: 8ms mientras llega el repaint dirigido por frames (Fase 2)
        ctx.request_repaint_after(Duration::from_millis(8));
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
