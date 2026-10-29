use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::AudioCapture;
use crate::video::VideoCapture;

#[derive(Clone)]
pub struct SyncStatus {
    pub audio_time: f64,
    pub video_time: f64,
    /// Drift A/V relativo al baseline capturado al arrancar (el offset
    /// constante entre relojes de audio y video no es desincronización).
    pub audio_video_diff: f64,
    /// Frames sobrescritos en el slot latest-wins sin mostrarse.
    pub video_frames_dropped: u64,
    pub audio_buffer_samples: usize,
    pub video_capture_fps: f32,
    pub is_synced: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct PerfStats {
    pub total_sync_checks: u64,
    pub video_frames_dropped: u64,
    pub avg_drift_ms: f64,
    pub max_drift_ms: f64,
    pub current_fps: f64,
}

struct StatsInner {
    video_frames_dropped: u64,
    total_sync_checks: u64,
    drift_samples: VecDeque<f64>,
    max_drift_seen: f64,
    /// Offset A/V observado al terminar el warmup; el drift reportado es
    /// la diferencia respecto de este baseline.
    baseline_diff: Option<f64>,
}

impl StatsInner {
    fn record_drift(&mut self, diff: f64) {
        self.drift_samples.push_back(diff.abs());
        if self.drift_samples.len() > 100 {
            self.drift_samples.pop_front();
        }
        if diff.abs() > self.max_drift_seen {
            self.max_drift_seen = diff.abs();
        }
    }
}

/// El SyncController ya no es dueño del frame visible: con latest-wins el
/// renderer lee el slot de VideoCapture directamente. Este controlador
/// queda como monitor de drift A/V (reloj de audio vs timestamps de video).
pub struct SyncController {
    audio: Arc<AudioCapture>,
    video: Option<Arc<VideoCapture>>,
    max_video_drift: f64,
    sync_check_interval: f64,
    stats: Mutex<StatsInner>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SyncController {
    pub fn new(audio: Arc<AudioCapture>, video: Option<Arc<VideoCapture>>) -> Self {
        Self {
            audio,
            video,
            max_video_drift: 0.1,
            sync_check_interval: 0.016,
            stats: Mutex::new(StatsInner {
                video_frames_dropped: 0,
                total_sync_checks: 0,
                drift_samples: VecDeque::new(),
                max_drift_seen: 0.0,
                baseline_diff: None,
            }),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    pub fn start(self: &Arc<Self>) {
        if self.running.load(Ordering::Relaxed) {
            return;
        }
        self.running.store(true, Ordering::Relaxed);
        let this = Arc::clone(self);
        let handle = std::thread::spawn(move || this.sync_loop());
        *self.handle.lock().unwrap() = Some(handle);
        println!("Sync controller started");
    }

    pub fn stop(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        println!("Sync controller stopped");
    }

    fn sync_loop(&self) {
        println!("Sync loop started");
        let warmup_time = 2.0;
        let start_time = Instant::now();

        while self.running.load(Ordering::Relaxed) {
            let audio_time = self.audio.get_audio_time();

            if start_time.elapsed().as_secs_f64() < warmup_time {
                std::thread::sleep(Duration::from_secs_f64(self.sync_check_interval));
                continue;
            }

            if let Some(video) = &self.video {
                if let Some(frame) = video.get_latest_frame() {
                    let raw_diff = audio_time - frame.timestamp;
                    let drift = {
                        let mut stats = self.stats.lock().unwrap();
                        let baseline = *stats.baseline_diff.get_or_insert(raw_diff);
                        raw_diff - baseline
                    };
                    self.stats.lock().unwrap().record_drift(drift);
                }
            }

            self.stats.lock().unwrap().total_sync_checks += 1;
            std::thread::sleep(Duration::from_secs_f64(self.sync_check_interval));
        }
        println!("Sync loop stopped");
    }

    pub fn get_sync_status(&self) -> SyncStatus {
        let audio_time = self.audio.get_audio_time();
        let mut video_time = 0.0;
        let mut audio_video_diff = 0.0;
        let mut has_video = false;
        if let Some(video) = &self.video {
            if let Some(frame) = video.get_latest_frame() {
                video_time = frame.timestamp;
                has_video = true;
                let raw_diff = audio_time - frame.timestamp;
                let baseline = self.stats.lock().unwrap().baseline_diff;
                audio_video_diff = raw_diff - baseline.unwrap_or(raw_diff);
            }
        }
        let (video_capture_fps, video_frames_dropped) = self
            .video
            .as_ref()
            .map(|v| {
                let s = v.get_capture_stats();
                (s.fps_actual, s.skipped_frames)
            })
            .unwrap_or((0.0, 0));
        self.stats.lock().unwrap().video_frames_dropped = video_frames_dropped;
        let audio_buffer_samples = self.audio.get_available_samples();
        let is_synced = !has_video || audio_video_diff.abs() <= self.max_video_drift;
        SyncStatus {
            audio_time,
            video_time,
            audio_video_diff,
            video_frames_dropped,
            audio_buffer_samples,
            video_capture_fps,
            is_synced,
        }
    }

    pub fn get_performance_stats(&self) -> PerfStats {
        let stats = self.stats.lock().unwrap();
        let avg_drift = if stats.drift_samples.is_empty() {
            0.0
        } else {
            stats.drift_samples.iter().sum::<f64>() / stats.drift_samples.len() as f64
        };
        let video_frames_dropped = self
            .video
            .as_ref()
            .map(|v| v.get_capture_stats().skipped_frames)
            .unwrap_or(0);
        PerfStats {
            total_sync_checks: stats.total_sync_checks,
            video_frames_dropped,
            avg_drift_ms: avg_drift * 1000.0,
            max_drift_ms: stats.max_drift_seen * 1000.0,
            current_fps: if self.sync_check_interval > 0.0 {
                1.0 / self.sync_check_interval
            } else {
                0.0
            },
        }
    }

    #[allow(dead_code)]
    pub fn reset_stats(&self) {
        let mut stats = self.stats.lock().unwrap();
        stats.video_frames_dropped = 0;
        stats.total_sync_checks = 0;
        stats.drift_samples.clear();
        stats.max_drift_seen = 0.0;
        stats.baseline_diff = None;
    }

    pub fn set_audio_volume(&self, volume: f32) {
        self.audio.set_volume(volume);
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[allow(dead_code)]
struct MonitorHistory {
    timestamp: f64,
    diff: f64,
    dropped: u64,
}

pub struct AVMonitor {
    sync: Arc<SyncController>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    history: Arc<Mutex<Vec<MonitorHistory>>>,
}

impl AVMonitor {
    pub fn new(sync: Arc<SyncController>) -> Self {
        Self {
            sync,
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
            history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn start_monitoring(&self, interval: f64) {
        if self.running.load(Ordering::Relaxed) {
            return;
        }
        self.running.store(true, Ordering::Relaxed);
        let running = self.running.clone();
        let sync = self.sync.clone();
        let history = self.history.clone();
        let handle = std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                let status = sync.get_sync_status();
                {
                    let mut hist = history.lock().unwrap();
                    hist.push(MonitorHistory {
                        timestamp: now_secs(),
                        diff: status.audio_video_diff,
                        dropped: status.video_frames_dropped,
                    });
                    if hist.len() > 100 {
                        hist.remove(0);
                    }
                }
                println!(
                    "A/V Sync: {:+.1}ms Audio: {} samples Video: {:.0}fps Dropped: {}",
                    status.audio_video_diff * 1000.0,
                    status.audio_buffer_samples,
                    status.video_capture_fps,
                    status.video_frames_dropped
                );
                std::thread::sleep(Duration::from_secs_f64(interval));
            }
        });
        *self.handle.lock().unwrap() = Some(handle);
        println!("AV monitoring started");
    }

    pub fn stop_monitoring(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        println!("AV monitoring stopped");
    }

    pub fn get_report(&self) -> String {
        let hist = self.history.lock().unwrap();
        if hist.is_empty() {
            return "No monitoring data available".into();
        }
        let avg_drift = hist.iter().map(|h| h.diff.abs()).sum::<f64>() / hist.len() as f64;
        let max_drift = hist.iter().map(|h| h.diff.abs()).fold(0.0f64, f64::max);
        let total_dropped = hist.last().map(|h| h.dropped).unwrap_or(0);
        let quality = if avg_drift < 0.05 {
            "Good"
        } else if avg_drift < 0.1 {
            "Fair"
        } else {
            "Poor"
        };
        format!(
            "AV Synchronization Report\n========================\nAverage drift: {:.1}ms\nMaximum drift: {:.1}ms\nTotal frames dropped: {total_dropped}\nMonitoring samples: {}\n\nSync quality: {quality}",
            avg_drift * 1000.0,
            max_drift * 1000.0,
            hist.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_history_keeps_last_100() {
        let mut stats = StatsInner {
            video_frames_dropped: 0,
            total_sync_checks: 0,
            drift_samples: VecDeque::new(),
            max_drift_seen: 0.0,
            baseline_diff: None,
        };
        for i in 0..250 {
            stats.record_drift(0.001 * i as f64);
        }
        assert_eq!(stats.drift_samples.len(), 100);
        let first: f64 = stats.drift_samples[0];
        assert!((first - 0.150).abs() < 1e-9);
        assert!((stats.max_drift_seen - 0.249).abs() < 1e-9);
    }

    #[test]
    fn baseline_gets_locked_on_first_sample() {
        let mut stats = StatsInner {
            video_frames_dropped: 0,
            total_sync_checks: 0,
            drift_samples: VecDeque::new(),
            max_drift_seen: 0.0,
            baseline_diff: None,
        };
        // primer diff crudo con offset de relojes de +1.5s → drift 0
        let raw = 1.5;
        let drift = {
            let baseline = *stats.baseline_diff.get_or_insert(raw);
            raw - baseline
        };
        assert!((drift).abs() < 1e-9);
        // después el offset constante no cuenta como drift
        let raw2 = 1.52;
        let drift2 = raw2 - stats.baseline_diff.unwrap();
        assert!((drift2 - 0.02).abs() < 1e-9);
    }
}
