"""
Video Renderer using PyQt6
Handles video display with frame hold strategy and overlay information
"""

import sys
import time
import cv2
import numpy as np
from PyQt6.QtWidgets import (QApplication, QMainWindow, QWidget, QVBoxLayout, 
                            QHBoxLayout, QLabel, QPushButton, QCheckBox)
from PyQt6.QtCore import Qt, QTimer, pyqtSignal, QThread
from PyQt6.QtGui import QImage, QPixmap, QPainter, QFont, QColor

from video_capture import TimestampedFrame
from sync_controller import SyncController


class VideoWidget(QWidget):
    """Custom widget for video display with overlay information"""
    
    def __init__(self):
        super().__init__()
        self.current_frame = None
        self.show_overlay = True
        self.show_stats = True
        self.last_render_time = 0
        self.fps_actual = 0
        self.frame_count = 0
        self.fps_update_time = time.time()
        self.target_fps = 60  # Default, will be updated from video capture
        
        # Set minimum size and background
        self.setMinimumSize(320, 240)
        self.setStyleSheet("background-color: black;")
    
    def set_target_fps(self, fps: float):
        """Set the target FPS from video capture"""
        self.target_fps = fps
    
    def update_frame(self, frame: TimestampedFrame):
        """Update the current frame for rendering"""
        self.current_frame = frame
        self.update()  # Trigger repaint
        
        # Calculate FPS
        self.frame_count += 1
        current_time = time.time()
        if current_time - self.fps_update_time >= 1.0:
            self.fps_actual = self.frame_count / (current_time - self.fps_update_time)
            self.frame_count = 0
            self.fps_update_time = current_time
    
    def set_overlay_visible(self, visible: bool):
        """Set overlay information visibility"""
        self.show_overlay = visible
        self.update()
    
    def set_stats_visible(self, visible: bool):
        """Set statistics overlay visibility"""
        self.show_stats = visible
        self.update()
    
    def paintEvent(self, event):
        """Custom paint event for video and overlays"""
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform)
        
        # Draw video frame
        if self.current_frame is not None:
            # Fix color space conversion
            # OpenCV uses BGR, convert to RGB properly
            if len(self.current_frame.frame.shape) == 3:
                # Check if it's BGR (3 channels)
                frame_rgb = cv2.cvtColor(self.current_frame.frame, cv2.COLOR_BGR2RGB)
            else:
                # Handle YUV or other formats
                frame_rgb = self.current_frame.frame
            
            # Convert to QImage
            height, width = frame_rgb.shape[:2]
            if len(frame_rgb.shape) == 3:
                bytes_per_line = 3 * width
                q_image = QImage(frame_rgb.data, width, height, bytes_per_line, QImage.Format.Format_RGB888)
            else:
                bytes_per_line = width
                q_image = QImage(frame_rgb.data, width, height, bytes_per_line, QImage.Format.Format_Grayscale8)
            
        # OBS-style text sharpening for downscaling
        scale_factor = min(widget_size.width() / width, widget_size.height() / height)
        
        if scale_factor < 0.8:  # Scaling down significantly
            # OBS-style text sharpening - preserve brightness!
            # Create unsharp mask (subtle)
            blurred = cv2.GaussianBlur(frame_rgb, (3, 3), 0.5)
            
            # Mild unsharp mask - preserve overall brightness
            mask = cv2.subtract(frame_rgb, blurred)
            mask = cv2.multiply(mask, 1.5)  # Boost contrast
            
            # Apply sharpening but maintain brightness
            sharpened = cv2.add(frame_rgb, mask)
            
            # Boost overall brightness to compensate
            sharpened = cv2.convertScaleAbs(sharpened, alpha=1.2, beta=10)
            
            # Apply very light bilateral filter just for noise
            frame_rgb = cv2.bilateralFilter(sharpened, 5, 50, 50)
        
        # Convert to QImage
        height, width = frame_rgb.shape[:2]
        bytes_per_line = 3 * width
        q_image = QImage(frame_rgb.data, width, height, bytes_per_line, QImage.Format.Format_RGB888)
        
        # Use smooth transformation for final scaling
        widget_size = self.size()
        scaled_pixmap = QPixmap.fromImage(q_image).scaled(
            widget_size, 
            Qt.AspectRatioMode.KeepAspectRatio, 
            Qt.TransformationMode.SmoothTransformation
        )
        
        # Center the image
        x = (self.width() - scaled_pixmap.width()) // 2
        y = (self.height() - scaled_pixmap.height()) // 2
        painter.drawPixmap(x, y, scaled_pixmap)
        
        # Draw overlays
        if self.show_overlay or self.show_stats:
            self._draw_overlays(painter)
    
    def _draw_overlays(self, painter: QPainter):
        """Draw overlay information on top of video"""
        # Set up font for overlays
        font = QFont("Courier", 10, QFont.Weight.Bold)
        painter.setFont(font)
        
        # Background for better readability
        margin = 10
        line_height = 20
        bg_color = QColor(0, 0, 0, 180)  # Semi-transparent black
        
        overlay_lines = []
        
        if self.show_overlay and self.current_frame:
            # Get FPS from video capture (target FPS)
            target_fps = 60  # We force 60fps
            overlay_lines.extend([
                f"Frame: {self.current_frame.frame_number}",
                f"Time: {self.current_frame.timestamp:.3f}s",
                f"FPS: {target_fps:.1f}"
            ])
        
        if self.show_stats:
            overlay_lines.append(f"Resolution: {self.width()}x{self.height()}")
        
        # Draw background rectangle
        if overlay_lines:
            text_width = max(painter.fontMetrics().horizontalAdvance(line) for line in overlay_lines)
            text_height = len(overlay_lines) * line_height
            painter.fillRect(margin, margin, text_width + 10, text_height + 5, bg_color)
            
            # Draw text
            painter.setPen(QColor(255, 255, 255))  # White text
            y_offset = margin + 15
            for line in overlay_lines:
                painter.drawText(margin + 5, y_offset, line)
                y_offset += line_height


class RenderThread(QThread):
    """Separate thread for rendering to avoid blocking UI"""
    
    frame_ready = pyqtSignal(TimestampedFrame)
    stats_update = pyqtSignal(dict)
    
    def __init__(self, sync_controller: SyncController):
        super().__init__()
        self.sync_controller = sync_controller
        self.is_running = False
        self.target_fps = 60
        self.frame_interval = 1.0 / self.target_fps
    
    def run(self):
        """Main render loop"""
        self.is_running = True
        last_frame_time = 0
        
        while self.is_running:
            try:
                current_time = time.time()
                
                # Check if it's time for a new frame
                if current_time - last_frame_time >= self.frame_interval:
                    # Get current synchronized frame
                    frame = self.sync_controller.get_current_frame()
                    
                    if frame:
                        self.frame_ready.emit(frame)
                        
                        # Send stats update
                        status = self.sync_controller.get_sync_status()
                        perf = self.sync_controller.get_performance_stats()
                        stats = {
                            'audio_time': status.audio_time,
                            'video_time': status.video_time,
                            'av_diff': status.audio_video_diff,
                            'is_synced': status.is_synced,
                            'frames_dropped': status.video_frames_dropped,
                            'avg_drift': perf['avg_drift_ms']
                        }
                        self.stats_update.emit(stats)
                    
                    last_frame_time = current_time
                
                # Sleep to maintain frame rate
                sleep_time = max(0, self.frame_interval - (time.time() - current_time))
                if sleep_time > 0:
                    self.msleep(int(sleep_time * 1000))
                
            except Exception as e:
                print(f"Error in render thread: {e}")
                self.msleep(16)  # ~60fps
    
    def stop(self):
        """Stop the render thread"""
        self.is_running = False
        self.wait()


class MainWindow(QMainWindow):
    """Main application window"""
    
    def __init__(self, sync_controller: SyncController):
        super().__init__()
        self.sync_controller = sync_controller
        self.render_thread = None
        
        self.init_ui()
        self.init_render_thread()
    
    def init_ui(self):
        """Initialize the user interface"""
        self.setWindowTitle("Python AV Monitor - Latency Tolerant")
        self.setGeometry(100, 100, 1280, 720)
        
        # Central widget
        central_widget = QWidget()
        self.setCentralWidget(central_widget)
        
        # Main layout
        main_layout = QVBoxLayout(central_widget)
        
        # Video widget
        self.video_widget = VideoWidget()
        main_layout.addWidget(self.video_widget, 1)
        
        # Control panel
        control_panel = self.create_control_panel()
        main_layout.addWidget(control_panel, 0)
        
        # Status bar
        self.status_label = QLabel("Initializing...")
        main_layout.addWidget(self.status_label, 0)
    
    def create_control_panel(self):
        """Create control panel with buttons and checkboxes"""
        panel = QWidget()
        panel.setMaximumHeight(100)
        layout = QHBoxLayout(panel)
        
        # Overlay controls
        self.overlay_checkbox = QCheckBox("Show Overlay")
        self.overlay_checkbox.setChecked(True)
        self.overlay_checkbox.toggled.connect(self.toggle_overlay)
        layout.addWidget(self.overlay_checkbox)
        
        self.stats_checkbox = QCheckBox("Show Stats")
        self.stats_checkbox.setChecked(True)
        self.stats_checkbox.toggled.connect(self.toggle_stats)
        layout.addWidget(self.stats_checkbox)
        
        # Spacer
        layout.addStretch()
        
        # Quit button
        quit_button = QPushButton("Quit")
        quit_button.clicked.connect(self.close)
        layout.addWidget(quit_button)
        
        return panel
    
    def init_render_thread(self):
        """Initialize the render thread"""
        self.render_thread = RenderThread(self.sync_controller)
        self.render_thread.frame_ready.connect(self.update_video_frame)
        self.render_thread.stats_update.connect(self.update_status)
        self.render_thread.start()
    
    def update_video_frame(self, frame: TimestampedFrame):
        """Update video widget with new frame"""
        self.video_widget.update_frame(frame)
    
    def update_status(self, stats: dict):
        """Update status bar with current statistics"""
        status_text = (
            f"Audio: {stats['audio_time']:.2f}s | "
            f"Video: {stats['video_time']:.2f}s | "
            f"A/V Diff: {stats['av_diff']*1000:+.1f}ms | "
            f"Sync: {'✓' if stats['is_synced'] else '✗'} | "
            f"Dropped: {stats['frames_dropped']} | "
            f"Avg Drift: {stats['avg_drift']:.1f}ms"
        )
        self.status_label.setText(status_text)
    
    def toggle_overlay(self, checked: bool):
        """Toggle overlay visibility"""
        self.video_widget.set_overlay_visible(checked)
    
    def toggle_stats(self, checked: bool):
        """Toggle statistics visibility"""
        self.video_widget.set_stats_visible(checked)
    
    def closeEvent(self, event):
        """Handle window close event"""
        if self.render_thread:
            self.render_thread.stop()
        event.accept()


class AVRenderer:
    """Main renderer class that manages the PyQt application"""
    
    def __init__(self, sync_controller: SyncController):
        self.sync_controller = sync_controller
        self.app = None
        self.window = None
    
    def start(self):
        """Start the renderer application"""
        # Create QApplication if it doesn't exist
        if not QApplication.instance():
            self.app = QApplication(sys.argv)
        else:
            self.app = QApplication.instance()
        
        # Create main window
        self.window = MainWindow(self.sync_controller)
        
        # Show window
        self.window.show()
        
        # Start event loop
        return self.app.exec()
    
    def stop(self):
        """Stop the renderer"""
        if self.window:
            self.window.close()


if __name__ == "__main__":
    # Test renderer with mock sync controller
    from sync_controller import SyncController, AudioCapture, VideoCapture
    
    print("Testing Video Renderer")
    
    # Create components (for testing)
    audio = AudioCapture()
    video = VideoCapture()
    sync = SyncController(audio, video)
    
    # Create renderer
    renderer = AVRenderer(sync)
    
    try:
        # Start renderer (this will block until window is closed)
        result = renderer.start()
        print(f"Renderer exited with code: {result}")
        
    except Exception as e:
        print(f"Error running renderer: {e}")
    finally:
        # Cleanup
        if renderer:
            renderer.stop()