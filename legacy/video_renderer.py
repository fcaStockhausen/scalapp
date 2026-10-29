"""
Video Renderer using PyQt6
Handles video display with basic scaling and floating transparent controls
"""

import sys
import time
import cv2
import numpy as np
from PyQt6.QtWidgets import (QApplication, QMainWindow, QWidget, QVBoxLayout, 
                            QHBoxLayout, QLabel, QPushButton, QCheckBox, 
                            QSlider, QFrame)
from PyQt6.QtCore import Qt, QTimer, pyqtSignal, QThread, QPropertyAnimation, QRect, QEasingCurve
from PyQt6.QtGui import QImage, QPixmap, QPainter, QFont, QColor, QCursor

from video_capture import TimestampedFrame
from sync_controller import SyncController

# Debug logging system
DEBUG = True

def debug_log(message):
    """Debug logging function"""
    if DEBUG:
        timestamp = time.strftime("%H:%M:%S")
        print(f"[{timestamp}] {message}")
        sys.stdout.flush()


class FloatingControlPanel(QWidget):
    """Floating transparent control panel that appears on hover"""
    
    volume_changed = pyqtSignal(int)
    
    def __init__(self):
        super().__init__()
        self.init_ui()
        self.setup_animation()
        
        # Timer for auto-hide (not single_shot)
        self.hide_timer = QTimer()
        self.hide_timer.timeout.connect(self.fade_out)
        
    def init_ui(self):
        """Initialize floating control panel"""
        self.setWindowFlags(
            Qt.WindowType.Tool | 
            Qt.WindowType.FramelessWindowHint | 
            Qt.WindowType.WindowStaysOnTopHint
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.setFixedSize(250, 80)
        
        layout = QVBoxLayout(self)
        layout.setContentsMargins(15, 15, 15, 15)
        
        # Semi-transparent background
        self.setStyleSheet("""
            QWidget {
                background-color: rgba(30, 30, 30, 220);
                border-radius: 12px;
                border: 1px solid rgba(255, 255, 255, 80);
            }
            QLabel {
                color: white;
                font-size: 12px;
                font-weight: 600;
                padding: 2px;
            }
            QSlider::groove:horizontal {
                height: 6px;
                background: rgba(255, 255, 255, 100);
                border-radius: 3px;
            }
            QSlider::handle:horizontal {
                background: #4CAF50;
                border: 2px solid white;
                width: 14px;
                height: 14px;
                margin: -6px 0;
                border-radius: 7px;
            }
            QSlider::handle:hover {
                background: #66BB6A;
            }
            QPushButton {
                background-color: rgba(244, 67, 54, 180);
                color: white;
                border: none;
                padding: 8px 16px;
                border-radius: 8px;
                font-weight: bold;
                font-size: 12px;
                box-shadow: 0 2px 4px rgba(0,0,0,0.2);
            }
            QPushButton:hover {
                background-color: rgba(211, 47, 47, 200);
            }
            QPushButton:pressed {
                background-color: rgba(198, 40, 40, 220);
            }
        """)
        
        # Volume control
        volume_layout = QHBoxLayout()
        
        volume_label = QLabel("Volume:")
        volume_layout.addWidget(volume_label)
        
        self.volume_slider = QSlider(Qt.Orientation.Horizontal)
        self.volume_slider.setMinimum(0)
        self.volume_slider.setMaximum(100)
        self.volume_slider.setValue(20)
        self.volume_slider.setFixedWidth(120)
        self.volume_slider.valueChanged.connect(self.volume_changed.emit)
        volume_layout.addWidget(self.volume_slider)
        
        self.volume_percent_label = QLabel("30%")
        volume_layout.addWidget(self.volume_percent_label)
        
        layout.addLayout(volume_layout)
        
        # Exit button
        self.exit_button = QPushButton("✕ Exit")
        self.exit_button.clicked.connect(lambda: self.parent().toggle_borderless(False))
        self.exit_button.setStyleSheet("""
            QPushButton {
                background-color: rgba(244, 67, 54, 200);
                color: white;
                border: none;
                padding: 8px 16px;
                border-radius: 8px;
                font-weight: bold;
                font-size: 12px;
            }
            QPushButton:hover {
                background-color: rgba(211, 47, 47, 220);
            }
            QPushButton:pressed {
                background-color: rgba(198, 40, 40, 255);
            }
        """)
        layout.addWidget(self.exit_button, 0, Qt.AlignmentFlag.AlignCenter)
        
    def setup_animation(self):
        """Setup fade animations"""
        self.fade_animation = QPropertyAnimation(self, b"windowOpacity")
        self.fade_animation.setDuration(300)
        self.fade_animation.setEasingCurve(QEasingCurve.Type.InOutQuad)
        
    def set_volume_display(self, percent):
        """Update volume percentage display"""
        self.volume_percent_label.setText(f"{percent}%")
    
    def enterEvent(self, event):
        """Show panel on hover"""
        self.show()
        self.fade_animation.setStartValue(0.0)
        self.fade_animation.setEndValue(1.0)
        self.fade_animation.start()
        super().enterEvent(event)
        
    def show_at_position(self, x, y):
        """Show floating panel at specific position"""
        self.move(x, y)
        self.setWindowOpacity(0.0)
        self.show()
        
        # Auto-hide after 3 seconds of no mouse movement
        self.hide_timer.stop()
        self.hide_timer.start(3000)
        
    def reset_hide_timer(self):
        """Reset the auto-hide timer"""
        if self.hide_timer.isActive():
            self.hide_timer.stop()
        self.hide_timer.start(3000)
    
    def fade_out(self):
        """Fade out panel"""
        self.fade_animation.setStartValue(1.0)
        self.fade_animation.setEndValue(0.0)
        self.fade_animation.start()
        
    def mouseMoveEvent(self, event):
        """Reset hide timer on mouse movement"""
        self.reset_hide_timer()
        super().mouseMoveEvent(event)


class VideoWidget(QWidget):
    """Custom widget for video display with overlay information"""
    
    def __init__(self):
        super().__init__()
        self.current_frame = None
        self.show_overlay = False  # Consistent with checkboxes
        self.show_stats = False    # Consistent with checkboxes
        self.last_render_time = 0
        self.fps_actual = 0
        self.frame_count = 0
        self.fps_update_time = time.time()
        self.target_fps = 60  # Default, will be updated from video capture
        
        # Floating control panel
        self.floating_panel = None
        self.show_controls = False  # Controls are hidden by default
        self.last_click_time = None  # For auto-hide timing
        
        # Mouse click detection
        self.setMouseTracking(False)  # Only track clicks, not movement
        self.mouse_timer = QTimer()
        self.mouse_timer.timeout.connect(self.check_hide_controls)
        self.mouse_timer.start(1000)  # Check every second
        
        # Set minimum size and background
        self.setMinimumSize(320, 240)
        self.setStyleSheet("background-color: black;")
        
    def setup_floating_panel(self, main_window):
        """Setup floating control panel"""
        self.floating_panel = FloatingControlPanel()
        self.floating_panel.setParent(main_window)
        self.floating_panel.volume_changed.connect(main_window.on_volume_changed)
        
    def mousePressEvent(self, event):
        """Show controls on click"""
        self.show_controls = True
        if self.floating_panel:
            global_pos = self.mapToGlobal(self.rect().topRight())
            panel_x = global_pos.x() - 260
            panel_y = global_pos.y() + 10
            self.floating_panel.show_at_position(panel_x, panel_y)
        super().mousePressEvent(event)
    
    def check_hide_controls(self):
        """Check if controls should be hidden"""
        if self.show_controls and self.floating_panel:
            current_time = time.time()
            # Show controls for 3 seconds after click
            if hasattr(self, 'last_click_time'):
                if current_time - self.last_click_time > 3.0:
                    self.hide_controls_panel()
                # else: Still within 3 seconds, keep showing
            else:
                self.last_click_time = current_time
    
    def hide_controls_panel(self):
        """Hide the floating controls panel"""
        if self.floating_panel:
            self.floating_panel.fade_out()
            self.show_controls = False
    
    def set_target_fps(self, fps: float):
        """Set target FPS from video capture"""
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
        debug_log(f"set_overlay_visible called with visible={visible}")
        self.show_overlay = visible
        self.update()
        debug_log(f"show_overlay variable set to {visible}")
    
    def set_stats_visible(self, visible: bool):
        """Set statistics overlay visibility"""
        debug_log(f"set_stats_visible called with visible={visible}")
        self.show_stats = visible
        self.update()
        debug_log(f"show_stats variable set to {visible}")
    
    def paintEvent(self, event):
        """Custom paint event for video and overlays"""
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform)
        
        # Draw video frame
        if self.current_frame is not None:
            # Convert OpenCV BGR to RGB
            frame_rgb = cv2.cvtColor(self.current_frame.frame, cv2.COLOR_BGR2RGB)
            
            # Convert to QImage
            height, width, channel = frame_rgb.shape
            bytes_per_line = 3 * width
            q_image = QImage(frame_rgb.data, width, height, bytes_per_line, QImage.Format.Format_RGB888)
            
            # Scale to fit widget while maintaining aspect ratio
            scaled_pixmap = QPixmap.fromImage(q_image).scaled(
                self.size(), 
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
    
    def _draw_overlays(self, painter):
        """Draw overlay information on top of video"""
        # Set up font for overlays
        font = QFont("Arial", 10, QFont.Weight.Bold)
        painter.setFont(font)
        
        # Background for better readability
        margin = 10
        line_height = 20
        bg_color = QColor(0, 0, 0, 180)  # Semi-transparent black
        
        overlay_lines = []
        
        if self.show_overlay and self.current_frame:
            # Get FPS from video capture (target FPS)
            overlay_lines.extend([
                f"Frame: {self.current_frame.frame_number}",
                f"Time: {self.current_frame.timestamp:.3f}s",
                f"FPS: {self.target_fps:.1f}"
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
        main_layout.setContentsMargins(0, 0, 0, 0)  # No margins
        
        # Video widget (takes entire space)
        self.video_widget = VideoWidget()
        self.video_widget.setup_floating_panel(self)  # Setup floating panel
        main_layout.addWidget(self.video_widget, 1)
        
        # Traditional control panel (kept for non-fullscreen mode)
        self.control_panel = self.create_control_panel()
        main_layout.addWidget(self.control_panel, 0)
        
        # Status bar
        self.status_label = QLabel("Initializing...")
        main_layout.addWidget(self.status_label, 0)
        
        # Store original window state
        self.original_geometry = None
        self.original_flags = None
        self.is_borderless = False
    
    def create_control_panel(self):
        """Create control panel with buttons and checkboxes"""
        panel = QWidget()
        panel.setMaximumHeight(100)
        layout = QHBoxLayout(panel)
        
        # Overlay controls
        self.overlay_checkbox = QCheckBox("Show Overlay")
        self.overlay_checkbox.setChecked(False)  # Disabled by default
        self.overlay_checkbox.toggled.connect(self.toggle_overlay)
        debug_log(f"Overlay checkbox: created, checked=False")
        layout.addWidget(self.overlay_checkbox)
        
        self.stats_checkbox = QCheckBox("Show Stats")
        self.stats_checkbox.setChecked(False)  # Disabled by default
        self.stats_checkbox.toggled.connect(self.toggle_stats)
        debug_log(f"Stats checkbox: created, checked=False")
        layout.addWidget(self.stats_checkbox)
        
        # Volume slider (traditional)
        from PyQt6.QtWidgets import QSlider, QLabel
        layout.addWidget(QLabel("Volume:"))
        self.volume_slider = QSlider(Qt.Orientation.Horizontal)
        self.volume_slider.setMinimum(0)
        self.volume_slider.setMaximum(100)
        self.volume_slider.setValue(30)  # Default 30%
        self.volume_slider.setFixedWidth(100)
        self.volume_slider.valueChanged.connect(self.on_volume_changed)
        layout.addWidget(self.volume_slider)
        
        # Borderless mode button (like scrcpy)
        self.borderless_button = QPushButton("Borderless Mode")
        self.borderless_button.clicked.connect(self.toggle_borderless)
        self.borderless_button.setCheckable(True)
        layout.addWidget(self.borderless_button)
        
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
        debug_log(f"toggle_overlay called with checked={checked}")
        self.video_widget.set_overlay_visible(checked)
        debug_log(f"Overlay visibility: set to {checked}")
    
    def toggle_stats(self, checked: bool):
        """Toggle statistics visibility"""
        debug_log(f"toggle_stats called with checked={checked}")
        self.video_widget.set_stats_visible(checked)
        debug_log(f"Stats visibility: set to {checked}")
    
    def on_volume_changed(self, value: int):
        """Handle volume slider change"""
        volume = value / 100.0  # Convert to 0.0-1.0 range
        if hasattr(self, 'sync_controller') and self.sync_controller:
            self.sync_controller.set_audio_volume(volume)
        
        # Update floating panel if it exists
        if self.video_widget and self.video_widget.floating_panel:
            self.video_widget.floating_panel.set_volume_display(value)
    
    def toggle_borderless(self, checked: bool):
        """Toggle borderless window mode (like scrcpy --window-borderless)"""
        debug_log(f"toggle_borderless called with checked={checked}, current is_borderless={self.is_borderless}")
        
        if checked and not self.is_borderless:
            debug_log("Entering borderless mode")
            # Store original state
            self.original_geometry = self.geometry()
            self.original_flags = self.windowFlags()
            
            # Hide traditional controls completely to remove empty space
            self.control_panel.hide()
            self.status_label.hide()
            
            # Set borderless window flags (like scrcpy)
            from PyQt6.QtCore import Qt
            from PyQt6.QtWidgets import QApplication
            
            self.setWindowFlags(
                Qt.WindowType.FramelessWindowHint | 
                Qt.WindowType.WindowStaysOnTopHint
            )
            
            # Restore geometry but keep as normal window (not fullscreen)
            if self.original_geometry:
                self.setGeometry(self.original_geometry)
            
            self.show()
            self.raise_()
            self.activateWindow()
            
            self.is_borderless = True
            self.borderless_button.setText("Windowed Mode")
            self.borderless_button.setChecked(True)  # Keep checkbox state synced
            debug_log("Borderless mode activated, button set to 'Windowed Mode', checked=True")
            
        elif not checked and self.is_borderless:
            debug_log("Exiting borderless mode")
            # Restore original window with decorations
            if self.original_flags:
                self.setWindowFlags(self.original_flags)
            if self.original_geometry:
                self.setGeometry(self.original_geometry)
            
            self.show()
            self.is_borderless = False
            self.borderless_button.setText("Borderless Mode")
            self.borderless_button.setChecked(False)  # Keep checkbox state synced
            debug_log("Windowed mode activated, button set to 'Borderless Mode', checked=False")
            
            # Show traditional controls again
            self.control_panel.show()
            self.status_label.show()
            
            # Hide floating panel
            if self.video_widget and self.video_widget.floating_panel:
                self.video_widget.floating_panel.hide()
        else:
            debug_log(f"toggle_borderless: no action needed (checked={checked}, is_borderless={self.is_borderless})")
    
    def closeEvent(self, event):
        """Handle window close event"""
        if self.render_thread:
            self.render_thread.stop()
        
        # Restore normal window mode before closing
        if self.is_borderless and self.original_flags:
            self.setWindowFlags(self.original_flags)
        
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