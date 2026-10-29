"""
Audio Capture Module using sounddevice
Handles UAC audio capture with buffering and timestamp management
"""

import sounddevice as sd
import numpy as np
import threading
import time
from collections import deque
from typing import Optional, Callable


class AudioPlayback:
    """Audio playback for monitoring captured audio - Low latency mode"""
    
    def __init__(self, sample_rate: int = 44100, channels: int = 2, 
                 block_size: int = 256, device: Optional[int] = None, low_latency: bool = True):
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.device = device  # None = default output device
        self.low_latency = low_latency
        
        self.is_running = False
        self.lock = threading.Lock()
        
        # Buffer for playback — audited 24/10: el diseño anterior (tope 3-5
        # bloques = 17-29ms) cortaba porque los relojes de captura y salida
        # derivan SIEMPRE (cristales distintos) y al acumular ~17ms el tope
        # borraba bloques = corte audible. Nuevo diseño: colchón de ~120ms
        # + limpieza controlada solo si se dispara GROSAMENTE (>30 bloques).
        buffer_length = 20 if low_latency else 100
        self.playback_buffer = deque(maxlen=buffer_length + 10)
        
        # Volume control (default 20% - more reasonable starting point)
        self.volume = 0.2
        
    def start(self):
        """Start audio playback"""
        if self.is_running:
            return
            
        try:
            # Configure low latency parameters
            stream_params = {
                'samplerate': self.sample_rate,
                'channels': self.channels,
                'blocksize': self.block_size,
                'callback': self._output_callback
            }
            
            if self.device is not None:
                stream_params['device'] = self.device
            
            # Use low latency mode for minimal buffering
            if self.low_latency:
                stream_params['latency'] = 'low'
            
            self.stream = sd.OutputStream(**stream_params)
            self.stream.start()
            self.is_running = True
            latency_mode = "low-latency" if self.low_latency else "normal"
            print(f"Audio playback started ({latency_mode}): {self.sample_rate}Hz, {self.channels} channels, blocksize={self.block_size}")
        except Exception as e:
            print(f"Failed to start audio playback: {e}")
            raise
    
    def stop(self):
        """Stop audio playback"""
        if not self.is_running:
            return
            
        try:
            self.stream.stop()
            self.stream.close()
            self.is_running = False
            print("Audio playback stopped")
        except Exception as e:
            print(f"Error stopping audio playback: {e}")
    
    def _output_callback(self, outdata: np.ndarray, frames: int, 
                        time_info: dict, status: sd.CallbackFlags):
        """Sounddevice callback for output audio data"""
        if status:
            print(f"Audio output status: {status}")
        
        with self.lock:
            if self.playback_buffer:
                # Get oldest audio data for playback (FIFO preserva el flujo)
                data = self.playback_buffer.popleft()
                if len(data) == frames:
                    outdata[:] = data
                else:
                    # Handle size mismatch
                    outdata[:len(data)] = data.reshape(-1, self.channels)[:frames]
                    outdata[len(data):] = 0
            else:
                # Buffer vacio (arranque o hipo >120ms): silencio. Con el
                # colchon de 20 bloques esto solo pasa al inicio.
                outdata.fill(0)
    
    def add_audio_data(self, data: np.ndarray):
        """Add audio data to playback buffer — deriva de relojes tolerada.

        Auditado 24/10: ya NO se borra a los 3 bloques. Con colchón de 20 y
        limpieza solo >30 (catch-up tras un hipo del callback), los dos
        relojes derivan dentro del colchón sin cortes.
        """
        with self.lock:
            # Catch-up controlado: solo si se acumuló GROSERA cantidad
            # (un callback de salida se quedó dormido >170ms) saltamos al
            # presente para no acumular latencia infinita.
            while len(self.playback_buffer) > 30:
                self.playback_buffer.popleft()

            # Apply volume control
            if hasattr(self, 'volume_multiplier'):
                adjusted_data = data * self.volume_multiplier
            else:
                adjusted_data = data

            self.playback_buffer.append(adjusted_data)
    
    def set_volume(self, volume: float):
        """Set volume level (0.0 to 1.0)"""
        volume = max(0.0, min(1.0, volume))  # Clamp to valid range
        self.volume_multiplier = volume
        print(f"Volume set to: {volume*100:.0f}%")


class AudioCapture:
    def __init__(
        self,
        sample_rate: int = 44100,
        channels: int = 2,
        block_size: int = 256,  # Balanced blocks for stability
        buffer_size: float = 2.0,  # seconds
        latency_buffer: float = 0.5,  # initial buffer in seconds
        device: Optional[int] = None,
        low_latency_mode: bool = False  # Enable low latency optimizations
    ):
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.buffer_size = buffer_size
        self.latency_buffer = latency_buffer
        self.device = device
        self.low_latency_mode = low_latency_mode
        
        # Circular buffer for audio data
        self.buffer_size_samples = int(buffer_size * sample_rate)
        self.audio_buffer = np.zeros((self.buffer_size_samples, channels), dtype=np.float32)
        self.buffer_write_pos = 0
        self.buffer_read_pos = 0
        
        # Playback timing
        self.playback_start_time = None
        self.total_samples_captured = 0
        self.total_samples_played = 0
        
        # Threading
        self.is_running = False
        self.lock = threading.Lock()
        
        # Callback for new data
        self.data_callback: Optional[Callable] = None
        
        # Audio playback for monitoring
        self.playback: Optional[AudioPlayback] = None
        self.enable_playback = True  # Enable monitoring by default
        
        # Volume control
        self.volume = 0.5  # Default 50% volume
    
    def _audio_callback(self, indata: np.ndarray, frames: int, 
                        time_info: dict, status: sd.CallbackFlags):
        """Sounddevice callback for incoming audio data"""
        if status:
            print(f"Audio callback status: {status}")
        
        with self.lock:
            # Write to circular buffer
            end_pos = self.buffer_write_pos + frames
            
            if end_pos <= self.buffer_size_samples:
                # Fits in one chunk
                self.audio_buffer[self.buffer_write_pos:end_pos] = indata
            else:
                # Wraps around
                first_chunk = self.buffer_size_samples - self.buffer_write_pos
                self.audio_buffer[self.buffer_write_pos:] = indata[:first_chunk]
                self.audio_buffer[:end_pos - self.buffer_size_samples] = indata[first_chunk:]
            
            self.buffer_write_pos = end_pos % self.buffer_size_samples
            self.total_samples_captured += frames
            
            # Add to playback buffer for monitoring
            if self.enable_playback and self.playback:
                self.playback.add_audio_data(indata)
    
    def start(self):
        """Start audio capture"""
        if self.is_running:
            return
        
        try:
            stream_params = {
                'samplerate': self.sample_rate,
                'channels': self.channels,
                'blocksize': self.block_size,
                'callback': self._audio_callback
            }
            
            if self.device is not None:
                stream_params['device'] = self.device
                device_info = sd.query_devices(self.device)
                print(f"Using audio device: {device_info['name']}")
            
            # Low latency mode: minimize input buffering
            if self.low_latency_mode:
                stream_params['latency'] = 'low'
            
            self.stream = sd.InputStream(**stream_params)
            self.stream.start()
            self.playback_start_time = time.time()
            self.is_running = True
            latency_mode = "low-latency" if self.low_latency_mode else "normal"
            print(f"Audio capture started ({latency_mode}): {self.sample_rate}Hz, {self.channels} channels, blocksize={self.block_size}")
            
            # Start playback for monitoring
            if self.enable_playback:
                try:
                    self.playback = AudioPlayback(
                        sample_rate=self.sample_rate,
                        channels=self.channels,
                        block_size=self.block_size,
                        device=None,  # Default output device
                        low_latency=self.low_latency_mode
                    )
                    self.playback.set_volume(self.volume)
                    self.playback.start()
                    print("✓ Audio playback/monitoring enabled")
                except Exception as e:
                    print(f"⚠ Could not start audio playback: {e}")
                    print("  Video will work but you won't hear audio")
                    self.enable_playback = False
                    
        except Exception as e:
            print(f"Failed to start audio capture: {e}")
            raise
    
    def stop(self):
        """Stop audio capture"""
        if not self.is_running:
            return
        
        try:
            self.stream.stop()
            self.stream.close()
            
            # Stop playback
            if self.playback:
                self.playback.stop()
                
            self.is_running = False
            print("Audio capture stopped")
        except Exception as e:
            print(f"Error stopping audio capture: {e}")
    
    def get_audio_time(self) -> float:
        """Get current audio playback time in seconds (acts as master clock)
        For ultra-low latency mode, reduce or eliminate the latency offset
        """
        if self.playback_start_time is None:
            return 0.0
        
        # Return relative time since start (simpler for sync)
        elapsed = time.time() - self.playback_start_time
        
        # In ultra-low latency mode, minimize or skip the latency buffer offset
        if self.low_latency_mode:
            # Use just 5ms offset for ultra-low latency
            buffered_time = max(0, elapsed - 0.005)
        else:
            # Normal buffering
            buffered_time = max(0, elapsed - self.latency_buffer)
        
        return buffered_time
    
    def get_available_samples(self) -> int:
        """Get number of samples available for reading"""
        with self.lock:
            if self.buffer_write_pos >= self.buffer_read_pos:
                return self.buffer_write_pos - self.buffer_read_pos
            else:
                return self.buffer_size_samples - self.buffer_read_pos + self.buffer_write_pos
    
    def set_volume(self, volume: float):
        """Set volume for audio monitoring (0.0 to 1.0) with corrected scaling"""
        volume = max(0.0, min(1.0, volume))  # Clamp to valid range
        
        # Apply logarithmic scaling for better low-volume control
        # Fixed scaling that reaches 100%
        if volume <= 0.0:
            scaled_volume = 0.0
        elif volume <= 0.8:
            # Low to mid volumes (0-80%): logarithmic control
            # Map 0-0.8 to 0-0.64 (80% of input)
            scaled_volume = (volume ** 1.2) * 1.25
        else:
            # High volumes (80-100%): linear response to full range
            # Map 0.8-1.0 to 0.64-1.0
            scaled_volume = 0.64 + ((volume - 0.8) * 1.8)
        
        # Ensure we can reach 100%
        if volume >= 0.95:
            scaled_volume = 1.0
        elif volume >= 0.85:
            scaled_volume = min(1.0, 0.85 + (volume - 0.85) * 1.0)
        
        # Store the original slider value
        self.volume = volume
        if self.playback:
            self.playback.set_volume(scaled_volume)
        print(f"Volume: {volume*100:.0f}% (actual: {scaled_volume*100:.0f}%)")
    
    def set_latency_buffer(self, latency: float):
        """Set latency buffer size in seconds (for audio-only mode optimization)
        Recommended values:
        - Ultra-low latency: 0.01-0.02 (10-20ms)
        - Low latency: 0.03-0.05 (30-50ms)
        - Normal: 0.1-0.5 (100-500ms)
        """
        self.latency_buffer = max(0.01, min(2.0, latency))  # Clamp to reasonable range
        latency_ms = self.latency_buffer * 1000
        if self.latency_buffer <= 0.03:
            mode = "ultra-low"
        elif self.latency_buffer <= 0.05:
            mode = "low"
        else:
            mode = "normal"
        print(f"Audio latency set to: {latency_ms:.0f}ms ({mode}-latency mode)")
    
    def read_samples(self, num_samples: int) -> Optional[np.ndarray]:
        """Read samples from buffer for playback"""
        with self.lock:
            available = self.get_available_samples()
            if available < num_samples:
                return None
            
            # Read from circular buffer
            end_pos = self.buffer_read_pos + num_samples
            data = np.zeros((num_samples, self.channels), dtype=np.float32)
            
            if end_pos <= self.buffer_size_samples:
                # Fits in one chunk
                data[:] = self.audio_buffer[self.buffer_read_pos:end_pos]
            else:
                # Wraps around
                first_chunk = self.buffer_size_samples - self.buffer_read_pos
                data[:first_chunk] = self.audio_buffer[self.buffer_read_pos:]
                data[first_chunk:] = self.audio_buffer[:end_pos - self.buffer_size_samples]
            
            self.buffer_read_pos = end_pos % self.buffer_size_samples
            self.total_samples_played += num_samples
            
            return data
    
    def list_devices(self):
        """List available audio devices"""
        print("Available audio devices:")
        devices = sd.query_devices()
        for i, dev in enumerate(devices):
            if dev['max_input_channels'] > 0:
                print(f"  [{i}] {dev['name']} - {dev['max_input_channels']} input channels")


if __name__ == "__main__":
    # Test audio capture
    audio = AudioCapture()
    audio.list_devices()
    
    try:
        audio.start()
        print("Audio capture running. Press Ctrl+C to stop...")
        while True:
            time.sleep(1)
            audio_time = audio.get_audio_time()
            available = audio.get_available_samples()
            print(f"Audio time: {audio_time:.3f}s, Available samples: {available}")
    except KeyboardInterrupt:
        audio.stop()
        print("Audio capture stopped")