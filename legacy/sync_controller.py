"""
Synchronization Controller
Coordinates audio and video using audio as master clock
Implements buffering and frame hold strategy
"""

import time
import threading
from typing import Optional, Tuple
from dataclasses import dataclass

from audio_capture import AudioCapture
from video_capture import VideoCapture, TimestampedFrame


@dataclass
class SyncStatus:
    """Synchronization status information"""
    audio_time: float
    video_time: float
    audio_video_diff: float
    video_frames_dropped: int
    audio_buffer_samples: int
    video_buffer_frames: int
    is_synced: bool


class SyncController:
    def __init__(
        self,
        audio_capture: AudioCapture,
        video_capture: VideoCapture,
        max_video_drift: float = 0.1,  # 100ms max drift
        sync_check_interval: float = 0.016  # ~60fps
    ):
        self.audio_capture = audio_capture
        self.video_capture = video_capture
        self.max_video_drift = max_video_drift
        self.sync_check_interval = sync_check_interval
        
        # Synchronization state
        self.is_running = False
        self.sync_thread = None
        self.lock = threading.Lock()
        
        # Statistics
        self.video_frames_dropped = 0
        self.total_sync_checks = 0
        self.last_sync_time = None
        
        # Current frame for rendering
        self.current_frame: Optional[TimestampedFrame] = None
        self.frame_update_time = 0.0
        
        # Performance tracking
        self.avg_drift = 0.0
        self.max_drift_seen = 0.0
        self.drift_samples = []
    
    def start(self):
        """Start synchronization controller"""
        if self.is_running:
            return
        
        self.is_running = True
        self.sync_thread = threading.Thread(target=self._sync_loop, daemon=True)
        self.sync_thread.start()
        print("Sync controller started")
    
    def stop(self):
        """Stop synchronization controller"""
        if not self.is_running:
            return
        
        self.is_running = False
        
        if self.sync_thread and self.sync_thread.is_alive():
            self.sync_thread.join(timeout=1.0)
        
        print("Sync controller stopped")
    
    def _sync_loop(self):
        """Main synchronization loop"""
        print("Sync loop started")
        
        # Wait for both audio and video to start before sync
        warmup_time = 2.0  # 2 seconds warmup
        start_time = time.time()
        
        while self.is_running:
            try:
                # Get current audio time (master clock)
                audio_time = self.audio_capture.get_audio_time()
                
                # Skip sync during warmup period
                if time.time() - start_time < warmup_time:
                    time.sleep(self.sync_check_interval)
                    continue
                
                # Get appropriate video frame for this audio time
                if self.video_capture is not None:
                    frame = self.video_capture.get_frame_for_time(audio_time)
                else:
                    # Audio-only mode: create dummy frame or None
                    frame = None
                
                if frame:
                    # Calculate video time (frame timestamp)
                    video_time = frame.timestamp
                    audio_video_diff = audio_time - video_time
                    
                    # Update drift tracking
                    self.drift_samples.append(abs(audio_video_diff))
                    if len(self.drift_samples) > 100:  # Keep last 100 samples
                        self.drift_samples.pop(0)
                    
                    # Update max drift seen
                    if abs(audio_video_diff) > self.max_drift_seen:
                        self.max_drift_seen = abs(audio_video_diff)
                    
                    # Check if frame should be dropped (too far behind)
                    should_drop = False
                    if audio_video_diff > self.max_video_drift:
                        # Video is too far behind audio
                        should_drop = True
                        self.video_frames_dropped += 1
                        if self.video_frames_dropped % 100 == 0:  # Only print every 100 drops
                            print(f"Frames dropped: {self.video_frames_dropped} (video behind by {audio_video_diff:.3f}s)")
                    elif audio_video_diff < -self.max_video_drift:
                        # Video is too far ahead (less common)
                        should_drop = True
                        if self.video_frames_dropped % 100 == 0:
                            print(f"Frames dropped: {self.video_frames_dropped} (video ahead by {-audio_video_diff:.3f}s)")
                    
                    if not should_drop:
                        # Update current frame for rendering
                        with self.lock:
                            self.current_frame = frame
                            self.frame_update_time = time.time()
                else:
                    # No video frame available (normal for audio-only)
                    if self.video_capture is not None:
                        # Video exists but no frame yet - normal during startup
                        pass
                    else:
                        # Audio-only mode - no video to sync
                        pass
                
                self.last_sync_time = time.time()
                self.total_sync_checks += 1
                
                # Sleep until next sync check
                time.sleep(self.sync_check_interval)
                
            except Exception as e:
                print(f"Error in sync loop: {e}")
                time.sleep(0.01)
        
        print("Sync loop stopped")
    
    def get_current_frame(self) -> Optional[TimestampedFrame]:
        """Get the current synchronized frame for rendering"""
        with self.lock:
            return self.current_frame
    
    def get_sync_status(self) -> SyncStatus:
        """Get current synchronization status"""
        audio_time = self.audio_capture.get_audio_time()
        
        video_time = 0.0
        audio_video_diff = 0.0
        
        if self.current_frame:
            video_time = self.current_frame.timestamp
            audio_video_diff = audio_time - video_time
        
        # Get buffer stats
        audio_buffer_samples = self.audio_capture.get_available_samples()
        video_buffer_frames = 0
        if self.video_capture is not None:
            video_stats = self.video_capture.get_buffer_stats()
            video_buffer_frames = video_stats['buffer_size']
        
        # Determine if synced (within acceptable drift)
        is_synced = abs(audio_video_diff) <= self.max_video_drift
        
        return SyncStatus(
            audio_time=audio_time,
            video_time=video_time,
            audio_video_diff=audio_video_diff,
            video_frames_dropped=self.video_frames_dropped,
            audio_buffer_samples=audio_buffer_samples,
            video_buffer_frames=video_buffer_frames,
            is_synced=is_synced
        )
    
    def get_performance_stats(self) -> dict:
        """Get performance statistics"""
        avg_drift = 0.0
        if self.drift_samples:
            avg_drift = sum(self.drift_samples) / len(self.drift_samples)
        
        return {
            'total_sync_checks': self.total_sync_checks,
            'video_frames_dropped': self.video_frames_dropped,
            'avg_drift_ms': avg_drift * 1000,
            'max_drift_ms': self.max_drift_seen * 1000,
            'current_fps': 1.0 / self.sync_check_interval if self.sync_check_interval > 0 else 0
        }
    
    def reset_stats(self):
        """Reset statistics"""
        self.video_frames_dropped = 0
        self.total_sync_checks = 0
        self.drift_samples.clear()
        self.max_drift_seen = 0.0
    
    def set_audio_volume(self, volume: float):
        """Set audio volume for monitoring (0.0 to 1.0)"""
        if self.audio_capture:
            self.audio_capture.set_volume(volume)


class AVMonitor:
    """
    Utility class for monitoring A/V synchronization
    Can be used for debugging and performance tuning
    """
    
    def __init__(self, sync_controller: SyncController):
        self.sync_controller = sync_controller
        self.is_monitoring = False
        self.monitor_thread = None
        self.status_history = []
    
    def start_monitoring(self, interval: float = 1.0):
        """Start monitoring sync status"""
        if self.is_monitoring:
            return
        
        self.is_monitoring = True
        self.monitor_thread = threading.Thread(
            target=self._monitor_loop, 
            args=(interval,), 
            daemon=True
        )
        self.monitor_thread.start()
        print("AV monitoring started")
    
    def stop_monitoring(self):
        """Stop monitoring"""
        self.is_monitoring = False
        if self.monitor_thread and self.monitor_thread.is_alive():
            self.monitor_thread.join(timeout=2.0)
        print("AV monitoring stopped")
    
    def _monitor_loop(self, interval: float):
        """Monitoring loop"""
        while self.is_monitoring:
            status = self.sync_controller.get_sync_status()
            perf = self.sync_controller.get_performance_stats()
            
            # Store status history (keep last 100 entries)
            status_entry = {
                'timestamp': time.time(),
                'status': status,
                'performance': perf
            }
            self.status_history.append(status_entry)
            if len(self.status_history) > 100:
                self.status_history.pop(0)
            
            # Print status
            print(f"A/V Sync: {status.audio_video_diff*1000:+.1f}ms "
                  f"Audio: {status.audio_buffer_samples} samples "
                  f"Video: {status.video_buffer_frames} frames "
                  f"Dropped: {status.video_frames_dropped}")
            
            time.sleep(interval)
    
    def get_report(self) -> str:
        """Generate monitoring report"""
        if not self.status_history:
            return "No monitoring data available"
        
        # Calculate averages and statistics
        drifts = [entry['status'].audio_video_diff for entry in self.status_history]
        avg_drift = sum(abs(d) for d in drifts) / len(drifts)
        max_drift = max(abs(d) for d in drifts)
        
        total_dropped = self.status_history[-1]['status'].video_frames_dropped
        
        report = f"""
AV Synchronization Report
========================
Average drift: {avg_drift*1000:.1f}ms
Maximum drift: {max_drift*1000:.1f}ms
Total frames dropped: {total_dropped}
Monitoring samples: {len(self.status_history)}

Sync quality: {'Good' if avg_drift < 0.05 else 'Fair' if avg_drift < 0.1 else 'Poor'}
        """.strip()
        
        return report


if __name__ == "__main__":
    # Test sync controller
    print("Testing Sync Controller (requires audio/video devices)")
    
    # Create capture instances
    audio = AudioCapture()
    video = VideoCapture()
    
    # Create sync controller
    sync = SyncController(audio, video)
    
    # Create monitor
    monitor = AVMonitor(sync)
    
    try:
        # Start everything
        audio.start()
        video.start()
        sync.start()
        monitor.start_monitoring(interval=2.0)
        
        print("Sync test running. Press Ctrl+C to stop...")
        time.sleep(30)  # Run for 30 seconds
        
    except KeyboardInterrupt:
        print("\nStopping sync test...")
        monitor.stop_monitoring()
        sync.stop()
        video.stop()
        audio.stop()
        
        # Print final report
        print(monitor.get_report())