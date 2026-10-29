#!/usr/bin/env python3
"""
Python AV Monitor (Latency-Tolerant)
Main application entry point

Captures HDMI USB audio/video with synchronized playback.
Audio acts as master clock, video uses frame hold strategy.
Accepts 300-800ms latency for stable streaming.
"""

import sys
import signal
import argparse
import time
from typing import Optional

from audio_capture import AudioCapture
from video_capture import VideoCapture
from sync_controller import SyncController, AVMonitor
from video_renderer import AVRenderer
from ultra_low_latency_passthrough import UltraLowLatencyPassthroughWithVolumeV4


class AVMonitorApp:
    """Main application class"""
    
    def __init__(
        self,
        audio_device: Optional[int] = None,
        video_device: Optional[int] = None,
        audio_sample_rate: int = 44100,
        audio_channels: int = 2,
        video_width: int = 1920,
        video_height: int = 1080,
        video_fps: int = 60,
        latency_buffer: float = 0.5,
        monitor_enabled: bool = False,
        audio_only: bool = False,
        ultra_low_latency: bool = False,
        direct_passthrough: bool = False,
        audio_block_size: Optional[int] = None
    ):
        # Audio-only mode flag
        self.audio_only = audio_only
        self.ultra_low_latency = ultra_low_latency
        self.direct_passthrough = direct_passthrough
        self.audio_block_size = audio_block_size
        
        # Auto-detect HDMI devices if not specified
        if audio_device is None:
            candidates = auto_detect_hmi_device()
            if candidates:
                audio_device = candidates[0][0]
                print(f"Auto-detected HDMI audio device: {candidates[0][1]} (ID: {audio_device})")
        
        # Auto-detect video device only if not audio-only
        if not audio_only and video_device is None:
            candidates = auto_detect_hmiv_device()
            if candidates:
                video_device = candidates[0][0]
                print(f"Auto-detected HDMI video device: {candidates[0][1]} (ID: {video_device})")
            else:
                video_device = 1  # Fallback
                print(f"Using fallback video device: {video_device}")
        elif audio_only:
            video_device = None
            print("Audio-only mode: Video capture disabled")
        
        # Configuration
        self.audio_device = audio_device
        self.video_device = video_device
        self.latency_buffer = latency_buffer
        self.monitor_enabled = monitor_enabled
        
        # Components
        self.audio_capture: Optional[AudioCapture] = None
        self.video_capture: Optional[VideoCapture] = None
        self.sync_controller: Optional[SyncController] = None
        self.av_monitor: Optional[AVMonitor] = None
        self.renderer: Optional[AVRenderer] = None
        self.passthrough: Optional[UltraLowLatencyPassthroughWithVolume] = None
        
        # State
        self.is_running = False
        
        # Initialize components
        self._init_components(
            audio_sample_rate=audio_sample_rate,
            audio_channels=audio_channels,
            video_width=video_width,
            video_height=video_height,
            video_fps=video_fps
        )
        
        # Setup signal handlers for graceful shutdown
        signal.signal(signal.SIGINT, self._signal_handler)
        signal.signal(signal.SIGTERM, self._signal_handler)
    
    def _init_components(
        self,
        audio_sample_rate: int,
        audio_channels: int,
        video_width: int,
        video_height: int,
        video_fps: int
    ):
        """Initialize all application components"""
        print("Initializing AV Monitor components...")
        
        # Determine block size based on mode
        if self.audio_block_size is not None:
            block_size = self.audio_block_size
            print(f"Using custom block size: {block_size}")
        elif self.ultra_low_latency:
            block_size = 128  # Small blocks for ultra-low latency but stable
            print(f"Using ultra-low latency block size: {block_size}")
        elif self.audio_only:
            block_size = 256  # Balanced blocks for stability
            print(f"Using low-latency block size: {block_size}")
        else:
            block_size = 1024  # Normal block size
            print(f"Using normal block size: {block_size}")
        
        try:
            # Audio capture (always needed)
            # Enable low latency mode for audio-only or ultra-low latency modes
            low_latency_mode = self.audio_only or self.ultra_low_latency
            self.audio_capture = AudioCapture(
                sample_rate=audio_sample_rate,
                channels=audio_channels,
                block_size=block_size,
                latency_buffer=self.latency_buffer,
                device=self.audio_device,
                low_latency_mode=low_latency_mode
            )
            print(f"✓ Audio capture initialized: {audio_sample_rate}Hz, {audio_channels} channels")
            
            # Video capture (only if not audio-only)
            if not self.audio_only:
                self.video_capture = VideoCapture(
                    device_id=self.video_device or 0,
                    width=video_width,
                    height=video_height,
                    fps=video_fps
                )
                print(f"✓ Video capture initialized: {video_width}x{video_height} @ {video_fps}fps")
            else:
                self.video_capture = None
                print("✓ Video capture skipped (audio-only mode)")
            
            # Sync controller (always needed)
            if self.audio_only:
                self.sync_controller = SyncController(
                    audio_capture=self.audio_capture,
                    video_capture=None
                )
                print("✓ Sync controller initialized (audio-only)")
            else:
                self.sync_controller = SyncController(
                    audio_capture=self.audio_capture,
                    video_capture=self.video_capture
                )
                print("✓ Sync controller initialized (audio + video)")
            
            # AV Monitor (optional)
            if self.monitor_enabled:
                self.av_monitor = AVMonitor(self.sync_controller)
                print("✓ AV Monitor enabled")
            
            # Renderer (conditional on audio-only mode)
            if self.audio_only:
                # Create audio-only renderer (minimal GUI or console)
                print("✓ Audio-only renderer initialized")
            else:
                # Create full AV renderer
                self.renderer = AVRenderer(self.sync_controller)
                print("✓ Video renderer initialized")
            
        except Exception as e:
            print(f"✗ Failed to initialize components: {e}")
            raise
    
    def start(self):
        """Start the application"""
        if self.is_running:
            return
        
        print("\n" + "="*50)
        print("Python AV Monitor - Latency Tolerant")
        print("="*50)
        latency_mode = "ultra-low (5ms)" if self.ultra_low_latency else ("low-latency (10ms)" if self.audio_only else f"normal ({self.latency_buffer*1000:.0f}ms)")
        print(f"Latency mode: {latency_mode}")
        print(f"Audio device: {self.audio_device if self.audio_device is not None else 'default'}")
        print(f"Video device: {self.video_device if self.video_device is not None else 'default'}")
        print("="*50 + "\n")

        try:
            # Handle direct passthrough mode (ultra-low latency, no processing)
            if self.audio_only and self.direct_passthrough:
                # Determine block size: user-specified or default (256)
                block_size = self.audio_block_size if self.audio_block_size is not None else 256
                block_ms = (block_size / 44100) * 1000

                print("\n" + "="*50)
                print("Direct Passthrough Mode - Low Latency V4")
                print("="*50)
                print("  Input device:", self.audio_device)
                print("  Sample rate: 44100Hz")
                print(f"  Block size: {block_size} samples (~{block_ms:.2f}ms)")
                print("  Volume processing: enabled")
                print("  Stream mode: Separate streams (like low-latency mode)")
                print(f"  Theoretical min latency: ~{block_ms:.2f}ms")
                print(f"  Expected total latency: ~{block_ms + 1:.1f}ms (including OS overhead)")
                print("="*50)

                self.passthrough = UltraLowLatencyPassthroughWithVolumeV4(
                    input_device=self.audio_device,
                    output_device=None,  # Use default output
                    sample_rate=44100,
                    block_size=block_size
                )
                self.passthrough.start()
                print("✓ Direct passthrough V4 started\n")

                self.is_running = True
            else:
                # Normal audio pipeline
                # Start audio capture
                if self.audio_device is not None:
                    print(f"Using audio device: {self.audio_device}")
                self.audio_capture.start()
                print("✓ Audio capture started")
            
            # Start video capture (only if not audio-only)
            if not self.audio_only and self.video_capture:
                self.video_capture.start()
                print("✓ Video capture started")
            else:
                print("✓ Video capture skipped (audio-only mode)")
            
            # Start sync controller
            self.sync_controller.start()
            print("✓ Sync controller started")
            
            # Start monitor if enabled
            if self.av_monitor:
                self.av_monitor.start_monitoring(interval=2.0)
                print("✓ AV monitoring started")
            
            self.is_running = True
            mode_text = "ultra-low latency audio-only mode" if self.ultra_low_latency else ("audio-only mode" if self.audio_only else "audio + video mode")
            print(f"\nAV Monitor is running in {mode_text}. Close the window to stop.\n")
            
            # Start renderer (this blocks until window is closed)
            if self.audio_only:
                # Audio-only mode: interactive volume controls
                if self.direct_passthrough:
                    block_size = self.audio_block_size if self.audio_block_size is not None else 256
                    block_ms = (block_size / 44100) * 1000
                    mode_text = f"Direct Passthrough V4 (~{block_ms + 1:.0f}ms)"
                    audio_obj = self.passthrough
                else:
                    mode_text = "Ultra Low Latency (8ms)" if self.ultra_low_latency else "Low Latency (15ms)"
                    audio_obj = self.audio_capture

                print(f"\n🎵 Audio Monitor Mode - {mode_text}")
                print("="*50)
                print("Controls:")
                print("  v/V: Volume Down/Up")
                print("  0-9: Set volume (0% to 90%)")
                print("  m  : Mute/Unmute")
                print("  q  : Quit")
                print("="*50)

                # Set minimal latency based on mode (only for non-passthrough)
                if not self.direct_passthrough:
                    if self.ultra_low_latency:
                        self.audio_capture.set_latency_buffer(0.008)  # 8ms ultra-minimal
                        print("✓ Audio latency set to 8ms (ultra-low latency mode)")
                    else:
                        self.audio_capture.set_latency_buffer(0.015)  # 15ms low latency
                        print("✓ Audio latency set to 15ms (low latency mode)")

                # Volume control state
                current_volume = 0.3  # Start at 30%
                is_muted = False
                muted_volume = 0.3  # Remember volume before mute
                audio_obj.set_volume(current_volume)
                print(f"✓ Volume set to {current_volume*100:.0f}%")
                
                try:
                    import sys
                    
                    while self.is_running:
                        try:
                            # Show current state before getting new command
                            cmd = input(f"\rVolume: {current_volume*100:.0f}% {'(MUTED)' if is_muted else ''} | Command (v/V/m/q/0-9): ").strip().lower()
                            
                            if cmd == 'q':
                                break
                            elif cmd == 'v':
                                current_volume = max(0.0, current_volume - 0.1)
                                is_muted = False
                            elif cmd == 'V':
                                current_volume = min(1.0, current_volume + 0.1)
                                is_muted = False
                            elif cmd == 'm':
                                if is_muted:
                                    current_volume = muted_volume
                                    is_muted = False
                                else:
                                    muted_volume = current_volume
                                    current_volume = 0.0
                                    is_muted = True
                            elif cmd in '0123456789':
                                current_volume = int(cmd) * 0.1
                                is_muted = False

                            # Apply volume after processing command
                            audio_obj.set_volume(current_volume)
                            
                        except KeyboardInterrupt:
                            break
                        
                    print("\nStopping audio monitor...")
                    result = 0
                    
                except Exception as e:
                    print(f"Input error: {e}")
                    result = 1
            else:
                # Full AV mode: start video renderer
                result = self.renderer.start()
            
        except Exception as e:
            print(f"✗ Failed to start application: {e}")
            self.stop()
            raise
    
    def stop(self):
        """Stop the application gracefully"""
        if not self.is_running:
            return
        
        print("\nStopping AV Monitor...")
        
        try:
            # Stop sync controller first (stops audio/video capture)
            if self.sync_controller:
                self.sync_controller.stop()
                print("✓ Sync controller stopped")
            
            # Stop renderer
            if self.renderer:
                self.renderer.stop()
                print("✓ Renderer stopped")
            
            # Stop monitor
            if self.av_monitor:
                self.av_monitor.stop_monitoring()
                print("✓ AV monitoring stopped")
                
                # Print final report
                print("\n" + self.av_monitor.get_report())
            
                # Print performance stats
                stats = self.sync_controller.get_performance_stats()
                print(f"\nPerformance Statistics:")
                print(f"  Total sync checks: {stats['total_sync_checks']}")
                print(f"  Frames dropped: {stats['video_frames_dropped']}")
                print(f"  Average drift: {stats['avg_drift_ms']:.1f}ms")
                print(f"  Maximum drift: {stats['max_drift_ms']:.1f}ms")
            
            # Stop video capture
            if self.video_capture:
                self.video_capture.stop()
                print("✓ Video capture stopped")

            # Stop audio capture
            if self.audio_capture:
                self.audio_capture.stop()
                print("✓ Audio capture stopped")

            # Stop passthrough (if in direct passthrough mode)
            if self.passthrough:
                self.passthrough.stop()
                print("✓ Direct passthrough stopped")
            
            self.is_running = False
            print("\nAV Monitor stopped successfully.")
            
        except Exception as e:
            print(f"✗ Error during shutdown: {e}")
    
    def _signal_handler(self, signum, frame):
        """Handle shutdown signals"""
        print(f"\nReceived signal {signum}, shutting down...")
        self.stop()
        sys.exit(0)


def auto_detect_hmi_device():
    """Auto-detect HDMI USB capture devices (including generic brands) - Enhanced for HDMI audio"""
    import sounddevice as sd
    
    devices = sd.query_devices()
    hdmia_candidates = []
    
    print("Buscando audio HDMI (incluyendo dispositivos genéricos)...")
    
    for i, dev in enumerate(devices):
        if dev['max_input_channels'] >= 2:
            name = dev['name'].lower()
            
            # Enhanced detection for HDMI audio
            hdmi_keywords = [
                # Specific HDMI capture keywords
                'usb', 'hdmi', 'capture', 'digital', 
                'elgato', 'magewell', 'blackmagic', 'avermedia',
                'hdmic', 'hdmia', 'usb-capture', 'video', 'audio',
                # Generic capture device keywords
                'uac', 'usb audio', 'usb device', 'unspecified',
                'capture', 'recording', 'input', 'line in'
            ]
            
            # Exclusions (iPhone, built-in, webcams, etc.)
            exclusions = [
                'iphone', 'facetime', 'built-in', 'webcam', 'camera',
                'microphone', 'built-in', 'speaker', 'bluetooth',
                'airpods', 'headphones', 'output'
            ]
            
            is_capture_device = any(keyword in name for keyword in hdmi_keywords)
            is_excluded = any(exclude in name for exclude in exclusions)
            
            if is_capture_device and not is_excluded:
                hdmia_candidates.append((i, dev['name'], dev['max_input_channels']))
                print(f"  -> Encontrado: {dev['name']} (ID: {i})")
    
    # Prefer "USBN Digital Audio" if available, then sort by channels
    usbn_candidates = [(i, n, c) for i, n, c in hdmia_candidates if 'usbn' in n.lower()]
    if usbn_candidates:
        hdmia_candidates = usbn_candidates + [c for c in hdmia_candidates if c not in usbn_candidates]
    else:
        hdmia_candidates.sort(key=lambda x: x[2], reverse=True)
    
    return hdmia_candidates

def auto_detect_hmiv_device():
    """Auto-detect HDMI USB video capture devices (including generic brands)"""
    import cv2
    import subprocess
    import json
    
    hdmiv_candidates = []
    
    print("Buscando capturadoras USB HDMI (incluyendo genéricas)...")
    
    # Primero: Buscar por nombre exacto de capturadoras USB
    try:
        result = subprocess.run(['system_profiler', 'SPCameraDataType', '-json'], 
                              capture_output=True, text=True, timeout=10)
        if result.returncode == 0:
            data = json.loads(result.stdout)
            cameras = data.get('SPCameraDataType', [])
            
            for camera in cameras:
                if not camera.get('_items'):  # Skip category headers
                    name = camera.get('_name', 'Unknown')
                    name_lower = name.lower()
                    
                    # Buscar capturadoras (incluyendo genéricas)
                    is_capture_device = any(keyword in name_lower for keyword in [
                        'hdmi', 'capture', 'usb', 'elgato', 'magewell', 
                        'blackmagic', 'avermedia', 'hdmic', 'video',
                        'usb-video', 'composite', 'component'
                    ])
                    
                    # Excluir SOLO dispositivos claramente no-HDMI
                    is_excluded = any(exclude in name_lower for exclude in [
                        'iphone', 'facetime', 'built-in', 'webcam', 'camera',
                        'isight', 'facetime', 'macbook', 'imac'
                    ])
                    
                    if is_capture_device and not is_excluded:
                        # Probar los primeros 4 device IDs
                        for i in range(4):
                            try:
                                cap = cv2.VideoCapture(i)
                                if cap.isOpened():
                                    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
                                    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
                                    fps = cap.get(cv2.CAP_PROP_FPS)
                                    
                                    # Si es capturadora HDMI, debe tener resolución HD mínima
                                    if width >= 1280 and height >= 720:
                                        device_name = f"{name} {width}x{height}@{fps:.0f}fps"
                                        hdmiv_candidates.append((i, device_name, width, height, fps))
                                        cap.release()
                                        break
                                cap.release()
                            except:
                                continue
                                
    except Exception as e:
        print(f"No se pudo obtener información detallada: {e}")
    
    # Si no encontramos por nombre, buscar por capacidades genéricas
    if not hdmiv_candidates:
        print("Buscando por capacidades (dispositivos con resolución HDMI)...")
        for i in range(5):  # 5 devices para encontrar capturadoras genéricas
            try:
                cap = cv2.VideoCapture(i)
                if cap.isOpened():
                    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
                    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
                    fps = cap.get(cv2.CAP_PROP_FPS)
                    
                    # Dispositivos HDMI genéricos suelen tener:
                    # - Resolución HD mínima (720p o más)
                    # - No son webcams típicas (640x480)
                    # - Priorizar 1080p+ y fps altos
                    
                    if width >= 1280 and height >= 720:
                        # Excluir webcams muy comunes (720p a 30fps)
                        if not (width == 1280 and height == 720 and fps < 31):
                            priority = fps + (width/1000)  # FPS + resolución
                            device_name = f"Capturadora_Generica_{i} {width}x{height}@{fps:.0f}fps"
                            hdmiv_candidates.append((i, device_name, width, height, fps, priority))
                    
                    cap.release()
            except Exception:
                continue
    
    # Ordenar por prioridad (más resolución y FPS primero)
    hdmiv_candidates.sort(key=lambda x: x[5] if len(x) > 5 else x[4], reverse=True)
    
    return hdmiv_candidates

def test_audio_devices():
    """Test audio devices to find HDMI audio specifically"""
    import sounddevice as sd
    import time
    import numpy as np
    
    devices = sd.query_devices()
    
    print("Probando dispositivos de audio para HDMI...")
    print("="*50)
    
    for i, dev in enumerate(devices):
        if dev['max_input_channels'] >= 2:
            name = dev['name']
            print(f"\nProbando dispositivo [{i}]: {name}")
            
            try:
                # Try to create a stream with this device
                stream = sd.InputStream(
                    device=i,
                    channels=min(dev['max_input_channels'], 2),
                    samplerate=44100,
                    blocksize=1024
                )
                stream.start()
                print(f"  ✓ Stream creado exitosamente")
                
                # Try to read some data (with timeout)
                start_time = time.time()
                data = None
                
                while time.time() - start_time < 2.0:  # 2 second timeout
                    try:
                        data, overflowed = stream.read(1024)
                        if data is not None and len(data) > 0:
                            break
                    except:
                        time.sleep(0.1)
                        continue
                
                if data is not None and len(data) > 0:
                    # Check if there's actual audio signal
                    max_amplitude = np.max(np.abs(data))
                    print(f"  ✓ Audio detectado - Amplitud máxima: {max_amplitude:.6f}")
                    
                    if max_amplitude > 0.001:  # Some threshold
                        print(f"  🎯 ¡SEÑAL DE AUDIO DETECTADA! (probablemente HDMI)")
                    else:
                        print(f"  - Silencio (posible HDMI sin señal)")
                else:
                    print(f"  ✗ No se pudo leer audio")
                
                stream.stop()
                stream.close()
                        
            except Exception as e:
                print(f"  ✗ Error: {e}")
    
    print("\n" + "="*50)
    print("Nota: Revisa cuál dispositivo muestra señal de audio activa")
    print("Dispositivos con señales detectadas son buenos candidatos para HDMI")


def list_devices():
    """List available HDMI USB capture devices ONLY (no iPhone/webcam activation)"""
    print("Dispositivos HDMI USB Captura SOLAMENTE:")
    print("="*50)
    
    # Audio devices - solo HDMI USB candidates
    print("\nDispositivos de Audio HDMI USB:")
    candidates = auto_detect_hmi_device()
    if candidates:
        for device_id, name, channels in candidates:
            print(f"  [{device_id}] {name} - {channels} canales (capturadora HDMI)")
    else:
        print("  No se detectaron capturadoras HDMI USB de audio")
    
    # Video devices - solo HDMI USB candidates
    print("\nDispositivos de Video HDMI USB:")
    video_candidates = auto_detect_hmiv_device()
    if video_candidates:
        for device_id, name, width, height, fps in video_candidates:
            print(f"  [{device_id}] {name} (capturadora HDMI)")
    else:
        print("  No se detectaron capturadoras HDMI USB de video")
        
    print("\nNota: Solo se muestran dispositivos HDMI USB. iPhone/webcams son ignorados.")
    print("Para probar manualmente dispositivos de audio: python main.py --test-audio")


def main():
    """Main entry point"""
    parser = argparse.ArgumentParser(
        description="Python AV Monitor - Latency Tolerant HDMI USB Capture",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s                          # Use default devices
  %(prog)s --audio-device 2         # Use specific audio device
  %(prog)s --video-device 1         # Use specific video device
  %(prog)s --list-devices          # List available devices
  %(prog)s --monitor --latency 1.0  # Enable monitoring, 1s buffer
        """
    )
    
    parser.add_argument(
        '--audio-device', '-a',
        type=int,
        help='Audio device ID (default: system default)'
    )
    
    parser.add_argument(
        '--video-device', '-v', 
        type=int,
        help='Video device ID (default: 0)'
    )
    
    parser.add_argument(
        '--latency', '-l',
        type=float,
        default=0.5,
        help='Latency buffer in seconds (default: 0.5)'
    )
    
    parser.add_argument(
        '--monitor', '-m',
        action='store_true',
        help='Enable AV monitoring with statistics'
    )
    
    parser.add_argument(
        '--audio-only',
        action='store_true',
        help='Audio-only mode (no video capture/display) - optimized for low latency (15ms)'
    )

    parser.add_argument(
        '--direct-passthrough',
        action='store_true',
        default=None,
        help='Direct passthrough mode - eliminates all processing for ~5-8ms latency (default: on when --audio-only, use --no-direct-passthrough to disable)'
    )

    parser.add_argument(
        '--no-direct-passthrough',
        action='store_true',
        help='Disable direct passthrough mode (use normal low-latency mode instead)'
    )
    
    parser.add_argument(
        '--ultra-low-latency',
        action='store_true',
        help='Enable ultra-low latency mode (8ms) - may cause dropouts on slower systems'
    )
    
    parser.add_argument(
        '--audio-latency',
        type=float,
        default=None,
        help='Audio latency in seconds (default: 0.5, audio-only: 0.015, ultra-low: 0.008). Lower = less latency but more dropouts'
    )
    
    parser.add_argument(
        '--block-size',
        type=int,
        default=None,
        help='Audio block size (default: 1024, audio-only: 256, ultra-low: 128). Smaller = less latency but more CPU'
    )
    
    parser.add_argument(
        '--test-audio',
        action='store_true',
        help='Test audio devices to find HDMI audio with signal detection'
    )
    
    parser.add_argument(
        '--list-devices',
        action='store_true',
        help='List available audio and video devices'
    )
    
    parser.add_argument(
        '--audio-rate',
        type=int,
        default=44100,
        help='Audio sample rate (default: 44100)'
    )
    
    parser.add_argument(
        '--video-width',
        type=int,
        default=1920,
        help='Video width (default: 1920)'
    )
    
    parser.add_argument(
        '--video-height',
        type=int,
        default=1080,
        help='Video height (default: 1080)'
    )
    
    parser.add_argument(
        '--video-fps',
        type=int,
        default=60,
        help='Video FPS (default: 60)'
    )
    
    args = parser.parse_args()
    
    # Handle device listing
    if args.list_devices:
        list_devices()
        return
    
    # Handle audio testing
    if args.test_audio:
        test_audio_devices()
        return
    
    # Handle latency configuration
    if args.audio_latency is not None:
        latency_buffer = args.audio_latency
    elif args.ultra_low_latency:
        latency_buffer = 0.008  # Ultra-minimal latency (8ms)
    elif args.audio_only:
        latency_buffer = 0.015  # Low latency for audio-only mode (15ms)
    else:
        latency_buffer = args.latency
    
    # Validate latency (only for non-audio-only/ultra-low modes)
    if not (args.audio_only or args.ultra_low_latency) and (latency_buffer < 0.1 or latency_buffer > 2.0):
        print("Error: Latency must be between 0.1 and 2.0 seconds")
        sys.exit(1)

    # Handle direct passthrough default (enabled by default with --audio-only)
    if args.direct_passthrough is None:
        # Not explicitly set by user - enable by default in audio-only mode
        direct_passthrough = args.audio_only and not args.no_direct_passthrough
    else:
        direct_passthrough = args.direct_passthrough

    # If ultra-low-latency is set, disable direct passthrough
    if args.ultra_low_latency:
        direct_passthrough = False

    # Validate direct passthrough requires audio_only
    if direct_passthrough and not args.audio_only:
        print("Error: --direct-passthrough requires --audio-only")
        sys.exit(1)
    
    try:
        # Create and start application
        app = AVMonitorApp(
            audio_device=args.audio_device,
            video_device=args.video_device,
            audio_sample_rate=args.audio_rate,
            video_width=args.video_width,
            video_height=args.video_height,
            video_fps=args.video_fps,
            latency_buffer=latency_buffer,
            monitor_enabled=args.monitor,
            audio_only=args.audio_only,
            ultra_low_latency=args.ultra_low_latency,
            direct_passthrough=direct_passthrough,
            audio_block_size=args.block_size
        )
        
        app.start()
        
    except KeyboardInterrupt:
        print("\nApplication interrupted by user")
    except Exception as e:
        print(f"Error: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()