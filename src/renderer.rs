use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use egui::{Color32, FontId, pos2, vec2, Rect};

use crate::audio::AudioStreams;
use crate::sync::SyncController;

pub struct RendererApp {
    sync: Arc<SyncController>,
    texture: egui::TextureHandle,
    last_frame_number: u64,
    rgba: Vec<u8>,
    texture_size: [usize; 2],
    show_overlay: bool,
    show_stats: bool,
    volume_pct: f32,
    borderless: bool,
    target_fps: f32,
    status_text: String,
    audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
}

impl RendererApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        sync: Arc<SyncController>,
        target_fps: f32,
        audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
    ) -> Self {
        let texture = cc.egui_ctx.load_texture(
            "video",
            egui::ColorImage::new([1, 1], Color32::BLACK),
            egui::TextureOptions::LINEAR,
        );
        Self {
            sync,
            texture,
            last_frame_number: u64::MAX,
            rgba: Vec::new(),
            texture_size: [1, 1],
            show_overlay: false,
            show_stats: false,
            volume_pct: 50.0,
            borderless: false,
            target_fps,
            status_text: "Initializing...".into(),
            audio_streams,
        }
    }

    fn pull_frame(&mut self) {
        if let Some((frame_number, _, width, height)) = self.sync.current_frame_meta() {
            if frame_number != self.last_frame_number {
                if let Some((w, h)) = self.sync.copy_current_frame_into(&mut self.rgba) {
                    let expected = (w as usize) * (h as usize) * 3;
                    if self.rgba.len() == expected {
                        self.rgba.reserve(expected + expected / 3);
                        let rgba: Vec<u8> = self
                            .rgba
                            .as_chunks::<3>()
                            .0
                            .iter()
                            .flat_map(|px| [px[0], px[1], px[2], 255])
                            .collect();
                        let image =
                            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                        self.texture.set(image, egui::TextureOptions::LINEAR);
                        self.texture_size = [w as usize, h as usize];
                        self.last_frame_number = frame_number;
                    }
                }
                let _ = (width, height);
            }
        }
    }

    fn update_status(&mut self) {
        let status = self.sync.get_sync_status();
        let perf = self.sync.get_performance_stats();
        self.status_text = format!(
            "Audio: {:.2}s | Video: {:.2}s | A/V Diff: {:+.1}ms | Sync: {} | Dropped: {} | Avg Drift: {:.1}ms",
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
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
            if let Some((frame_number, timestamp, _, _)) = self.sync.current_frame_meta() {
                lines.push(format!("Frame: {frame_number}"));
                lines.push(format!("Time: {timestamp:.3}s"));
                lines.push(format!("FPS: {:.1}", self.target_fps));
            }
        }
        if self.show_stats {
            let status = self.sync.get_sync_status();
            let perf = self.sync.get_performance_stats();
            lines.push(format!(
                "Resolution: {}x{}",
                self.texture_size[0], self.texture_size[1]
            ));
            lines.push(format!(
                "A/V Diff: {:+.1}ms ({})",
                status.audio_video_diff * 1000.0,
                if status.is_synced { "synced" } else { "drift" }
            ));
            lines.push(format!("Dropped: {}", status.video_frames_dropped));
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
                        self.sync.set_audio_volume(self.volume_pct / 100.0);
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

        egui::TopBottomPanel::bottom("status")
            .show_separator_line(!self.borderless)
            .show(ctx, |ui| {
                ui.label(&self.status_text);
            });

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
        self.update_status();
        ctx.request_repaint_after(Duration::from_millis(15));
    }
}

pub fn run(
    sync: Arc<SyncController>,
    target_fps: f32,
    audio_streams: Option<std::sync::Arc<std::sync::Mutex<AudioStreams>>>,
) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("ScalApp AV Monitor - Latency Tolerant"),
        ..Default::default()
    };
    eframe::run_native(
        "scalapp-av-monitor",
        options,
        Box::new(move |cc| Ok(Box::new(RendererApp::new(cc, sync, target_fps, audio_streams)))),
    )
}
