use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
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

#[allow(dead_code)]
pub struct BufferStats {
    pub buffer_size: usize,
    pub max_buffer_size: usize,
    pub fps_actual: f32,
    pub total_frames: u64,
    pub last_frame_time: Option<f64>,
}

struct CaptureStats {
    frame_count: u64,
    fps_actual: f32,
    fps_frame_count: u64,
    last_fps_time: Instant,
    last_frame_time: Option<f64>,
}

pub fn select_frame_for_time(buffer: &[TimestampedFrame], target_time: f64) -> Option<TimestampedFrame> {
    if buffer.is_empty() {
        return None;
    }
    buffer
        .iter()
        .rev()
        .find(|f| f.timestamp <= target_time)
        .or_else(|| buffer.first())
        .cloned()
}

pub struct VideoCapture {
    device_index: u32,
    width: u32,
    height: u32,
    pub fps: u32,
    buffer_size: usize,
    buffer: Arc<Mutex<VecDeque<TimestampedFrame>>>,
    stats: Arc<Mutex<CaptureStats>>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl VideoCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn new(device_index: u32, width: u32, height: u32, fps: u32) -> Self {
        Self {
            device_index,
            width,
            height,
            fps,
            buffer_size: 120,
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            stats: Arc::new(Mutex::new(CaptureStats {
                frame_count: 0,
                fps_actual: 0.0,
                fps_frame_count: 0,
                last_fps_time: Instant::now(),
                last_frame_time: None,
            })),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
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

    fn capture_loop(&self, tx: std::sync::mpsc::Sender<Result<(u32, u32, f32), String>>) {
        println!("Video capture thread started");
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
        let mut camera = match Camera::new(CameraIndex::Index(self.device_index), requested) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot open camera device {}: {e}", self.device_index);
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
        let _ = tx.send(Ok((resolution.width_x, resolution.height_y, actual_fps)));
        println!(
            "Camera setup: {}x{} @ {actual_fps:.2} FPS",
            resolution.width_x, resolution.height_y
        );

        let start_time = Instant::now();
        while self.running.load(Ordering::Relaxed) {
            match camera.frame() {
                Ok(frame_buffer) => match frame_buffer.decode_image::<RgbFormat>() {
                    Ok(image) => {
                        let width = image.width();
                        let height = image.height();
                        let data = Arc::new(image.into_raw());
                        let timestamp = start_time.elapsed().as_secs_f64();
                        {
                            let mut buffer = self.buffer.lock().unwrap();
                            if buffer.len() >= self.buffer_size {
                                buffer.pop_front();
                            }
                            buffer.push_back(TimestampedFrame {
                                data,
                                width,
                                height,
                                timestamp,
                                frame_number: self.stats.lock().unwrap().frame_count,
                            });
                        }
                        let mut stats = self.stats.lock().unwrap();
                        stats.frame_count += 1;
                        stats.fps_frame_count += 1;
                        stats.last_frame_time = Some(timestamp);
                        let now = Instant::now();
                        let elapsed = now.duration_since(stats.last_fps_time).as_secs_f64();
                        if elapsed >= 1.0 {
                            stats.fps_actual = stats.fps_frame_count as f32 / elapsed as f32;
                            stats.fps_frame_count = 0;
                            stats.last_fps_time = now;
                        }
                    }
                    Err(e) => {
                        eprintln!("Frame decode error: {e}");
                        std::thread::sleep(Duration::from_millis(10));
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

    pub fn get_frame_for_time(&self, target_time: f64) -> Option<TimestampedFrame> {
        let mut buffer = self.buffer.lock().unwrap();
        select_frame_for_time(buffer.make_contiguous(), target_time)
    }

    #[allow(dead_code)]
    pub fn get_latest_frame(&self) -> Option<TimestampedFrame> {
        let buffer = self.buffer.lock().unwrap();
        buffer.back().cloned()
    }

    pub fn get_buffer_stats(&self) -> BufferStats {
        let buffer = self.buffer.lock().unwrap();
        let stats = self.stats.lock().unwrap();
        BufferStats {
            buffer_size: buffer.len(),
            max_buffer_size: self.buffer_size,
            fps_actual: stats.fps_actual,
            total_frames: stats.frame_count,
            last_frame_time: stats.last_frame_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts: f64, n: u64) -> TimestampedFrame {
        TimestampedFrame {
            data: Arc::new(vec![0u8; 4]),
            width: 2,
            height: 2,
            timestamp: ts,
            frame_number: n,
        }
    }

    #[test]
    fn frame_hold_picks_most_recent_not_in_future() {
        let buffer = vec![frame(1.0, 0), frame(2.0, 1), frame(3.0, 2)];
        assert_eq!(select_frame_for_time(&buffer, 2.5).unwrap().frame_number, 1);
        assert_eq!(select_frame_for_time(&buffer, 9.0).unwrap().frame_number, 2);
        assert_eq!(select_frame_for_time(&buffer, 0.5).unwrap().frame_number, 0);
        assert!(select_frame_for_time(&[], 1.0).is_none());
    }
}
