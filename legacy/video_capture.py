"""
Video Capture Module using OpenCV
Handles UVC video capture with timestamp management and frame buffering
"""

import cv2
import time
import threading
import numpy as np
from collections import deque
from typing import Optional, Tuple
from dataclasses import dataclass


@dataclass
class TimestampedFrame:
    """Frame with timestamp metadata"""
    frame: np.ndarray
    timestamp: float  # monotonic time in seconds
    frame_number: int


class VideoCapture:
    def __init__(
        self,
        device_id: int = 0,
        width: int = 1920,
        height: int = 1080,
        fps: int = 60,
        buffer_size: int = 120  # frames (2 seconds at 60fps)
    ):
        self.device_id = device_id
        self.width = width
        self.height = height
        self.fps = fps
        self.buffer_size = buffer_size
        
        # Frame buffer with timestamps
        self.frame_buffer = deque(maxlen=buffer_size)
        
        # Threading
        self.is_running = False
        self.capture_thread = None
        self.lock = threading.Lock()
        
        # Capture properties
        self.cap = None
        self.frame_count = 0
        self.last_frame_time = None
        self.start_time = None  # Track capture start time
        
        # Performance monitoring
        self.fps_actual = 0
        self.last_fps_time = time.time()
        self.fps_frame_count = 0
    
    def _setup_camera(self):
        """Initialize camera with desired settings"""
        self.cap = cv2.VideoCapture(self.device_id)
        
        if not self.cap.isOpened():
            raise RuntimeError(f"Cannot open camera device {self.device_id}")
        
        # Set camera properties - FORCE 60fps
        self.cap.set(cv2.CAP_PROP_FRAME_WIDTH, self.width)
        self.cap.set(cv2.CAP_PROP_FRAME_HEIGHT, self.height)
        self.cap.set(cv2.CAP_PROP_FPS, self.fps)
        self.cap.set(cv2.CAP_PROP_FOURCC, cv2.VideoWriter_fourcc(*'MJPG'))
        
        # Force 60fps with multiple attempts
        fps_set_attempts = [
            60.0,    # Try 60fps first
            59.94,    # NTSC 60fps
            30.0,     # Fallback to 30fps if needed
        ]
        
        actual_fps = self.fps
        for target_fps in fps_set_attempts:
            self.cap.set(cv2.CAP_PROP_FPS, target_fps)
            actual_fps = self.cap.get(cv2.CAP_PROP_FPS)
            print(f"Setting FPS to {target_fps:.2f}, got {actual_fps:.2f}")
            
            if abs(actual_fps - target_fps) < 2.0:  # Within tolerance
                self.fps = target_fps
                break
        
        # Use buffers to reduce latency
        self.cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        
        # Verify settings
        actual_width = int(self.cap.get(cv2.CAP_PROP_FRAME_WIDTH))
        actual_height = int(self.cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
        actual_fps = self.cap.get(cv2.CAP_PROP_FPS)
        
        print(f"Camera setup: {actual_width}x{actual_height} @ {actual_fps:.2f} FPS (forced to {self.fps:.2f})")
        
        return actual_width, actual_height, actual_fps
    
    def _capture_loop(self):
        """Main capture thread loop"""
        print("Video capture thread started")
        self.start_time = time.time()  # Use regular time for timestamps
        
        while self.is_running:
            try:
                # Read frame from camera
                ret, frame = self.cap.read()
                
                if not ret:
                    print("Failed to read frame from camera")
                    time.sleep(0.01)
                    continue
                
                # Use simple relative timestamp (seconds since start)
                timestamp = time.time() - self.start_time
                
                # Create timestamped frame
                timestamped_frame = TimestampedFrame(
                    frame=frame.copy(),
                    timestamp=timestamp,
                    frame_number=self.frame_count
                )
                
                # Add to buffer
                with self.lock:
                    self.frame_buffer.append(timestamped_frame)
                    self.last_frame_time = timestamp
                
                self.frame_count += 1
                self.fps_frame_count += 1
                
                # Calculate actual FPS every second
                current_time = time.time()
                if current_time - self.last_fps_time >= 1.0:
                    self.fps_actual = self.fps_frame_count / (current_time - self.last_fps_time)
                    self.fps_frame_count = 0
                    self.last_fps_time = current_time
                
            except Exception as e:
                print(f"Error in capture loop: {e}")
                time.sleep(0.01)
        
        print("Video capture thread stopped")
    
    def start(self):
        """Start video capture"""
        if self.is_running:
            return
        
        try:
            # Setup camera
            self._setup_camera()
            
            # Start capture thread
            self.is_running = True
            self.capture_thread = threading.Thread(target=self._capture_loop, daemon=True)
            self.capture_thread.start()
            
            print("Video capture started")
            
        except Exception as e:
            print(f"Failed to start video capture: {e}")
            self.is_running = False
            raise
    
    def stop(self):
        """Stop video capture"""
        if not self.is_running:
            return
        
        self.is_running = False
        
        if self.capture_thread and self.capture_thread.is_alive():
            self.capture_thread.join(timeout=2.0)
        
        if self.cap:
            self.cap.release()
            self.cap = None
        
        print("Video capture stopped")
    
    def get_frame_for_time(self, target_time: float) -> Optional[TimestampedFrame]:
        """
        Get the most recent frame with timestamp <= target_time
        Uses frame hold strategy - repeats last valid frame
        """
        with self.lock:
            if not self.frame_buffer:
                return None
            
            # Find the most recent frame with timestamp <= target_time
            best_frame = None
            
            for frame in reversed(self.frame_buffer):
                if frame.timestamp <= target_time:
                    best_frame = frame
                    break
            
            # If no frame is old enough, return the oldest available
            if best_frame is None:
                best_frame = self.frame_buffer[0]
            
            return best_frame
    
    def get_latest_frame(self) -> Optional[TimestampedFrame]:
        """Get the most recent frame"""
        with self.lock:
            if not self.frame_buffer:
                return None
            return self.frame_buffer[-1]
    
    def get_buffer_stats(self) -> dict:
        """Get buffer statistics"""
        with self.lock:
            return {
                'buffer_size': len(self.frame_buffer),
                'max_buffer_size': self.buffer_size,
                'fps_actual': self.fps_actual,
                'total_frames': self.frame_count,
                'last_frame_time': self.last_frame_time
            }
    
    def list_devices(self):
        """List available video devices"""
        print("Available video devices:")
        
        # Test device IDs 0-9
        for i in range(10):
            cap = cv2.VideoCapture(i)
            if cap.isOpened():
                # Get device info
                width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
                height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
                fps = cap.get(cv2.CAP_PROP_FPS)
                backend = cap.getBackendName()
                
                print(f"  [{i}] {backend} - {width}x{height} @ {fps:.2f} FPS")
                cap.release()
            else:
                break  # Stop at first non-existent device


if __name__ == "__main__":
    # Test video capture
    video = VideoCapture()
    video.list_devices()
    
    try:
        video.start()
        print("Video capture running. Press Ctrl+C to stop...")
        
        while True:
            time.sleep(1)
            stats = video.get_buffer_stats()
            print(f"Buffer: {stats['buffer_size']}/{stats['max_buffer_size']} frames, "
                  f"FPS: {stats['fps_actual']:.1f}, Total: {stats['total_frames']}")
            
    except KeyboardInterrupt:
        video.stop()
        print("Video capture stopped")