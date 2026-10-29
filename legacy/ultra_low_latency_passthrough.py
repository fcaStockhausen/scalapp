"""
Ultra-low latency audio passthrough
Direct input→output with ZERO processing
"""

import sounddevice as sd
import numpy as np
import threading
from collections import deque


class UltraLowLatencyPassthrough:
    """
    Ultra-low latency audio passthrough
    Direct input→output with minimal processing
    No circular buffers, no locks, no volume processing in audio path
    """
    
    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 48000,  # 48kHz for better latency
        channels: int = 2,
        block_size: int = 128,  # 128 samples = 2.67ms @ 48kHz
        volume: float = 1.0,
        enable_volume: bool = False  # Skip volume processing for min latency
    ):
        self.input_device = input_device
        self.output_device = output_device
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.volume = volume
        self.enable_volume = enable_volume
        
        self.is_running = False
        self.stream = None
        
        # Statistics
        self.blocks_processed = 0
        self.total_latency_ms = (block_size / sample_rate) * 1000
    
    def start(self):
        """Start ultra-low latency passthrough"""
        if self.is_running:
            return
        
        print(f"Starting Ultra-Low Latency Passthrough...")
        print(f"  Input device: {self.input_device}")
        print(f"  Output device: {self.output_device}")
        print(f"  Sample rate: {self.sample_rate}Hz")
        print(f"  Block size: {self.block_size} samples (~{self.total_latency_ms:.2f}ms)")
        print(f"  Volume processing: {'enabled' if self.enable_volume else 'DISABLED (min latency)'}")
        print(f"  Theoretical min latency: ~{self.total_latency_ms:.2f}ms")
        
        try:
            # Create duplex stream (input + output in same callback)
            # This is MUCH faster than separate streams
            self.stream = sd.Stream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',
                device=(self.input_device, self.output_device),
                callback=self._duplex_callback
            )
            
            self.stream.start()
            self.is_running = True
            print(f"✓ Ultra-low latency passthrough started")
            print(f"✓ Expected total latency: ~{self.total_latency_ms + 3:.1f}ms (including OS overhead)")
            
        except Exception as e:
            print(f"✗ Failed to start passthrough: {e}")
            raise
    
    def stop(self):
        """Stop passthrough"""
        if not self.is_running:
            return
        
        try:
            if self.stream:
                self.stream.stop()
                self.stream.close()
            
            self.is_running = False
            print(f"✓ Ultra-low latency passthrough stopped")
            print(f"  Blocks processed: {self.blocks_processed}")
            
        except Exception as e:
            print(f"✗ Error stopping passthrough: {e}")
    
    def _duplex_callback(self, indata: np.ndarray, outdata: np.ndarray, 
                        frames: int, time_info: dict, status: sd.CallbackFlags):
        """
        Duplex callback - receives input and produces output in SAME call
        This eliminates all buffering between input and output
        """
        if status:
            print(f"Audio status: {status}")
        
        # Direct passthrough: copy input to output
        # This is the fastest possible - NO processing
        if self.enable_volume and self.volume != 1.0:
            # Volume processing (adds ~0.01-0.02ms but still very fast)
            outdata[:] = indata * self.volume
        else:
            # Direct copy - ZERO processing overhead
            outdata[:] = indata
        
        self.blocks_processed += 1
    
    def set_volume(self, volume: float):
        """Set volume (only effective if enable_volume=True)"""
        self.volume = max(0.0, min(1.0, volume))
        mode = "with volume processing" if self.enable_volume else "ignored (volume disabled)"
        print(f"Volume set to: {volume*100:.0f}% ({mode})")
    
    def get_stats(self) -> dict:
        """Get statistics"""
        return {
            'blocks_processed': self.blocks_processed,
            'block_size': self.block_size,
            'sample_rate': self.sample_rate,
            'theoretical_latency_ms': self.total_latency_ms,
            'estimated_total_latency_ms': self.total_latency_ms + 3,  # +3ms OS overhead
            'volume_processing': self.enable_volume
        }


"""
Ultra-low latency audio passthrough
Direct input→output with ZERO processing
"""

import sounddevice as sd
import numpy as np
import threading
from collections import deque
import ctypes
import ctypes.util


class UltraLowLatencyPassthroughV2:
    """
    Ultra-low latency passthrough V2 - OPTIMIZED for macOS
    Uses SEPARATE input/output streams with minimal buffering
    More predictable than duplex on some hardware
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,  # 44.1kHz better on this hardware
        channels: int = 2,
        block_size: int = 64,  # 64 samples = 1.45ms @ 44.1kHz (ultra minimal)
        volume: float = 0.3,
        enable_volume: bool = True
    ):
        self.input_device = input_device
        self.output_device = output_device
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.volume = volume
        self.enable_volume = enable_volume

        self.is_running = False
        self.input_stream = None
        self.output_stream = None

        # Ultra-minimal buffer (just 1 block ahead)
        self.buffer = None
        self.buffer_lock = threading.Lock()

        # Statistics
        self.blocks_processed = 0
        self.total_latency_ms = (block_size / sample_rate) * 1000

    def start(self):
        """Start ultra-low latency passthrough V2"""
        if self.is_running:
            return

        print(f"Starting Ultra-Low Latency Passthrough V2 (Optimized)...")
        print(f"  Input device: {self.input_device}")
        print(f"  Output device: {self.output_device}")
        print(f"  Sample rate: {self.sample_rate}Hz")
        print(f"  Block size: {self.block_size} samples (~{self.total_latency_ms:.2f}ms)")
        print(f"  Volume processing: {'enabled' if self.enable_volume else 'DISABLED'}")
        print(f"  Theoretical min latency: ~{self.total_latency_ms:.2f}ms")
        print(f"  Stream mode: Separate (more predictable on this hardware)")

        try:
            # Initialize buffer with first block of zeros
            self.buffer = np.zeros((self.block_size, self.channels), dtype=np.float32)

            # Create input stream
            self.input_stream = sd.InputStream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',
                device=self.input_device,
                callback=self._input_callback,
                prime_output_buffers_using_stream_callback=False,  # Don't prime
                dither_off=True  # Disable dither for lower latency
            )

            # Create output stream (separate, not duplex)
            self.output_stream = sd.OutputStream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',
                device=self.output_device,
                callback=self._output_callback,
                prime_output_buffers_using_stream_callback=True,  # Prime output
                dither_off=True  # Disable dither for lower latency
            )

            self.input_stream.start()
            self.output_stream.start()
            self.is_running = True

            print(f"✓ Ultra-low latency passthrough V2 started")
            print(f"✓ Expected total latency: ~{self.total_latency_ms + 2:.1f}ms (including OS overhead)")

        except Exception as e:
            print(f"✗ Failed to start passthrough V2: {e}")
            raise

    def stop(self):
        """Stop passthrough"""
        if not self.is_running:
            return

        try:
            if self.input_stream:
                self.input_stream.stop()
                self.input_stream.close()

            if self.output_stream:
                self.output_stream.stop()
                self.output_stream.close()

            self.is_running = False
            print(f"✓ Ultra-low latency passthrough V2 stopped")
            print(f"  Blocks processed: {self.blocks_processed}")

        except Exception as e:
            print(f"✗ Error stopping passthrough V2: {e}")

    def _input_callback(self, indata: np.ndarray, frames: int,
                       time_info: dict, status: sd.CallbackFlags):
        """Input callback - just stores the latest block"""
        if status:
            print(f"Input status: {status}")

        with self.buffer_lock:
            # Store input data for output (zero-copy if possible)
            if self.enable_volume:
                self.buffer[:] = indata * self.volume
            else:
                self.buffer[:] = indata

    def _output_callback(self, outdata: np.ndarray, frames: int,
                        time_info: dict, status: sd.CallbackFlags):
        """Output callback - sends latest input block"""
        if status:
            print(f"Output status: {status}")

        with self.buffer_lock:
            outdata[:] = self.buffer

        self.blocks_processed += 1

    def set_volume(self, volume: float):
        """Set volume (only effective if enable_volume=True)"""
        self.volume = max(0.0, min(1.0, volume))
        print(f"Volume set to: {volume*100:.0f}%")

    def is_muted(self) -> bool:
        return self.volume == 0.0

    def get_volume_percent(self) -> float:
        return self.volume * 100

    def get_stats(self) -> dict:
        """Get statistics"""
        return {
            'blocks_processed': self.blocks_processed,
            'block_size': self.block_size,
            'sample_rate': self.sample_rate,
            'theoretical_latency_ms': self.total_latency_ms,
            'estimated_total_latency_ms': self.total_latency_ms + 2,
            'volume_processing': self.enable_volume,
            'stream_mode': 'separate'
        }


class UltraLowLatencyPassthroughV3:
    """
    Ultra-low latency passthrough V3 - LOW LATENCY EDITION
    Uses DUPLEX stream with latency='low' (like low-latency mode)
    NO locks, NO separate streams, NO manual sync
    Feels as fast or faster than low-latency mode
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,  # 44.1kHz for this hardware
        channels: int = 2,
        block_size: int = 128,  # 128 samples = 2.9ms @ 44.1kHz (stable but fast)
        volume: float = 0.3,
        enable_volume: bool = True
    ):
        self.input_device = input_device
        self.output_device = output_device
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.volume = volume
        self.enable_volume = enable_volume

        self.is_running = False
        self.stream = None

        # Statistics
        self.blocks_processed = 0
        self.total_latency_ms = (block_size / sample_rate) * 1000

    def start(self):
        """Start ultra-low latency passthrough V3 - Low Latency Edition"""
        if self.is_running:
            return

        print(f"Starting Ultra-Low Latency Passthrough V3 (Low Latency Edition)...")
        print(f"  Input device: {self.input_device}")
        print(f"  Output device: {self.output_device}")
        print(f"  Sample rate: {self.sample_rate}Hz")
        print(f"  Block size: {self.block_size} samples (~{self.total_latency_ms:.2f}ms)")
        print(f"  Volume processing: {'enabled' if self.enable_volume else 'DISABLED'}")
        print(f"  Theoretical min latency: ~{self.total_latency_ms:.2f}ms")
        print(f"  Stream mode: DUPLEX (like low-latency mode)")

        try:
            # Create DUPLEX stream (input + output in SAME callback)
            # This eliminates ALL synchronization overhead
            # Uses latency='low' like the low-latency mode that feels faster
            self.stream = sd.Stream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',  # KEY: same as low-latency mode
                device=(self.input_device, self.output_device),
                callback=self._duplex_callback,
                dither_off=True  # Disable dither for lower latency
            )

            self.stream.start()
            self.is_running = True

            print(f"✓ Ultra-low latency passthrough V3 started")
            print(f"✓ Expected total latency: ~{self.total_latency_ms + 1.5:.1f}ms (including OS overhead)")

        except Exception as e:
            print(f"✗ Failed to start passthrough V3: {e}")
            raise

    def stop(self):
        """Stop passthrough"""
        if not self.is_running:
            return

        try:
            if self.stream:
                self.stream.stop()
                self.stream.close()

            self.is_running = False
            print(f"✓ Ultra-low latency passthrough V3 stopped")
            print(f"  Blocks processed: {self.blocks_processed}")

        except Exception as e:
            print(f"✗ Error stopping passthrough V3: {e}")

    def _duplex_callback(self, indata: np.ndarray, outdata: np.ndarray,
                        frames: int, time_info: dict, status: sd.CallbackFlags):
        """
        Duplex callback - receives input and produces output in SAME call
        NO locks, NO buffers, NO sync - just like low-latency mode
        """
        if status:
            print(f"Audio status: {status}")

        # Direct passthrough: copy input to output
        if self.enable_volume:
            outdata[:] = indata * self.volume
        else:
            outdata[:] = indata

        self.blocks_processed += 1

    def set_volume(self, volume: float):
        """Set volume (only effective if enable_volume=True)"""
        self.volume = max(0.0, min(1.0, volume))
        print(f"Volume set to: {volume*100:.0f}%")

    def is_muted(self) -> bool:
        return self.volume == 0.0

    def get_volume_percent(self) -> float:
        return self.volume * 100

    def get_stats(self) -> dict:
        """Get statistics"""
        return {
            'blocks_processed': self.blocks_processed,
            'block_size': self.block_size,
            'sample_rate': self.sample_rate,
            'theoretical_latency_ms': self.total_latency_ms,
            'estimated_total_latency_ms': self.total_latency_ms + 1.5,
            'volume_processing': self.enable_volume,
            'stream_mode': 'duplex'
        }


class UltraLowLatencyPassthroughV4:
    """
    Ultra-low latency passthrough V4 - LOW LATENCY MODE CLONE
    Uses SEPARATE streams like low-latency mode but eliminates circular buffer
    Block size 256 for stability (feels faster than smaller blocks)
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,
        channels: int = 2,
        block_size: int = 256,  # 256 = 5.8ms @ 44.1kHz (like low-latency mode)
        volume: float = 0.3,
        enable_volume: bool = True
    ):
        self.input_device = input_device
        self.output_device = output_device
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self.volume = volume
        self.enable_volume = enable_volume

        self.is_running = False
        self.input_stream = None
        self.output_stream = None

        # Playback buffer (like low-latency mode - small but stable)
        self.playback_buffer = deque(maxlen=5)  # Keep 5 blocks
        self.buffer_lock = threading.Lock()

        # Statistics
        self.blocks_processed = 0
        self.total_latency_ms = (block_size / sample_rate) * 1000

    def start(self):
        """Start ultra-low latency passthrough V4 - Low Latency Clone"""
        if self.is_running:
            return

        print(f"Starting Ultra-Low Latency Passthrough V4 (Low Latency Clone)...")
        print(f"  Input device: {self.input_device}")
        print(f"  Output device: {self.output_device}")
        print(f"  Sample rate: {self.sample_rate}Hz")
        print(f"  Block size: {self.block_size} samples (~{self.total_latency_ms:.2f}ms)")
        print(f"  Volume processing: {'enabled' if self.enable_volume else 'DISABLED'}")
        print(f"  Theoretical min latency: ~{self.total_latency_ms:.2f}ms")
        print(f"  Stream mode: Separate streams (like low-latency mode)")

        try:
            # Create input stream (like low-latency mode)
            self.input_stream = sd.InputStream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',  # Same as low-latency mode
                device=self.input_device,
                callback=self._input_callback
            )

            # Create output stream (like low-latency mode)
            self.output_stream = sd.OutputStream(
                samplerate=self.sample_rate,
                channels=self.channels,
                blocksize=self.block_size,
                dtype=np.float32,
                latency='low',  # Same as low-latency mode
                device=self.output_device,
                callback=self._output_callback
            )

            self.input_stream.start()
            self.output_stream.start()
            self.is_running = True

            print(f"✓ Ultra-low latency passthrough V4 started")
            print(f"✓ Expected total latency: ~{self.total_latency_ms + 2:.1f}ms (including OS overhead)")

        except Exception as e:
            print(f"✗ Failed to start passthrough V4: {e}")
            raise

    def stop(self):
        """Stop passthrough"""
        if not self.is_running:
            return

        try:
            if self.input_stream:
                self.input_stream.stop()
                self.input_stream.close()

            if self.output_stream:
                self.output_stream.stop()
                self.output_stream.close()

            self.is_running = False
            print(f"✓ Ultra-low latency passthrough V4 stopped")
            print(f"  Blocks processed: {self.blocks_processed}")

        except Exception as e:
            print(f"✗ Error stopping passthrough V4: {e}")

    def _input_callback(self, indata: np.ndarray, frames: int,
                       time_info: dict, status: sd.CallbackFlags):
        """Input callback - adds to playback buffer (like low-latency mode)"""
        if status:
            print(f"Input status: {status}")

        with self.buffer_lock:
            # Apply volume if enabled
            if self.enable_volume:
                data = indata * self.volume
            else:
                data = indata

            # Keep buffer small (like low-latency mode)
            if len(self.playback_buffer) >= 3:
                self.playback_buffer.popleft()

            self.playback_buffer.append(data)

    def _output_callback(self, outdata: np.ndarray, frames: int,
                        time_info: dict, status: sd.CallbackFlags):
        """Output callback - reads from playback buffer (like low-latency mode)"""
        if status:
            print(f"Output status: {status}")

        with self.buffer_lock:
            if self.playback_buffer:
                data = self.playback_buffer.popleft()
                if len(data) == frames:
                    outdata[:] = data
                else:
                    outdata[:len(data)] = data.reshape(-1, self.channels)[:frames]
                    outdata[len(data):] = 0
            else:
                outdata.fill(0)

        self.blocks_processed += 1

    def set_volume(self, volume: float):
        """Set volume (only effective if enable_volume=True)"""
        self.volume = max(0.0, min(1.0, volume))
        print(f"Volume set to: {volume*100:.0f}%")

    def is_muted(self) -> bool:
        return self.volume == 0.0

    def get_volume_percent(self) -> float:
        return self.volume * 100

    def get_stats(self) -> dict:
        """Get statistics"""
        return {
            'blocks_processed': self.blocks_processed,
            'block_size': self.block_size,
            'sample_rate': self.sample_rate,
            'theoretical_latency_ms': self.total_latency_ms,
            'estimated_total_latency_ms': self.total_latency_ms + 2,
            'volume_processing': self.enable_volume,
            'stream_mode': 'separate',
            'buffer_size': len(self.playback_buffer)
        }


class UltraLowLatencyPassthroughWithVolumeV4:
    """
    Ultra-low latency passthrough WITH volume control - V4 Low Latency Clone
    Uses separate streams like low-latency mode, block size 256 (~6-8ms latency)
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,
        channels: int = 2,
        block_size: int = 256,
        initial_volume: float = 0.3
    ):
        self.passthrough = UltraLowLatencyPassthroughV4(
            input_device=input_device,
            output_device=output_device,
            sample_rate=sample_rate,
            channels=channels,
            block_size=block_size,
            volume=initial_volume,
            enable_volume=True
        )
        self.current_volume = initial_volume
        self.is_muted = False
        self.volume_before_mute = initial_volume

    def start(self):
        """Start passthrough"""
        self.passthrough.start()

    def stop(self):
        """Stop passthrough"""
        self.passthrough.stop()

    def set_volume(self, volume: float):
        """Set volume (0.0 to 1.0)"""
        volume = max(0.0, min(1.0, volume))
        self.current_volume = volume
        self.is_muted = False
        self.passthrough.set_volume(volume)

    def volume_up(self, delta: float = 0.1):
        """Increase volume"""
        new_vol = min(1.0, self.current_volume + delta)
        self.set_volume(new_vol)

    def volume_down(self, delta: float = 0.1):
        """Decrease volume"""
        new_vol = max(0.0, self.current_volume - delta)
        self.set_volume(new_vol)

    def toggle_mute(self):
        """Toggle mute"""
        if self.is_muted:
            # Unmute
            self.current_volume = self.volume_before_mute
            self.passthrough.set_volume(self.current_volume)
            self.is_muted = False
            print(f"Unmuted - Volume: {self.current_volume*100:.0f}%")
        else:
            # Mute
            self.volume_before_mute = self.current_volume
            self.passthrough.set_volume(0.0)
            self.is_muted = True
            print(f"Muted")

    def get_volume_percent(self) -> int:
        """Get current volume as percentage"""
        return int(self.current_volume * 100)

    def get_stats(self) -> dict:
        """Get statistics"""
        stats = self.passthrough.get_stats()
        stats.update({
            'current_volume': self.current_volume,
            'is_muted': self.is_muted,
            'volume_before_mute': self.volume_before_mute
        })
        return stats


class UltraLowLatencyPassthroughWithVolumeV3:
    """
    Ultra-low latency passthrough WITH volume control - V3 Low Latency Edition
    Uses duplex stream with latency='low' (~4-6ms latency, feels as fast as low-latency mode)
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,
        channels: int = 2,
        block_size: int = 128,
        initial_volume: float = 0.3
    ):
        self.passthrough = UltraLowLatencyPassthroughV3(
            input_device=input_device,
            output_device=output_device,
            sample_rate=sample_rate,
            channels=channels,
            block_size=block_size,
            volume=initial_volume,
            enable_volume=True
        )
        self.current_volume = initial_volume
        self.is_muted = False
        self.volume_before_mute = initial_volume

    def start(self):
        """Start passthrough"""
        self.passthrough.start()

    def stop(self):
        """Stop passthrough"""
        self.passthrough.stop()

    def set_volume(self, volume: float):
        """Set volume (0.0 to 1.0)"""
        volume = max(0.0, min(1.0, volume))
        self.current_volume = volume
        self.is_muted = False
        self.passthrough.set_volume(volume)

    def volume_up(self, delta: float = 0.1):
        """Increase volume"""
        new_vol = min(1.0, self.current_volume + delta)
        self.set_volume(new_vol)

    def volume_down(self, delta: float = 0.1):
        """Decrease volume"""
        new_vol = max(0.0, self.current_volume - delta)
        self.set_volume(new_vol)

    def toggle_mute(self):
        """Toggle mute"""
        if self.is_muted:
            # Unmute
            self.current_volume = self.volume_before_mute
            self.passthrough.set_volume(self.current_volume)
            self.is_muted = False
            print(f"Unmuted - Volume: {self.current_volume*100:.0f}%")
        else:
            # Mute
            self.volume_before_mute = self.current_volume
            self.passthrough.set_volume(0.0)
            self.is_muted = True
            print(f"Muted")

    def get_volume_percent(self) -> int:
        """Get current volume as percentage"""
        return int(self.current_volume * 100)

    def get_stats(self) -> dict:
        """Get statistics"""
        stats = self.passthrough.get_stats()
        stats.update({
            'current_volume': self.current_volume,
            'is_muted': self.is_muted,
            'volume_before_mute': self.volume_before_mute
        })
        return stats


class UltraLowLatencyPassthroughWithVolumeV2:
    """
    Ultra-low latency passthrough WITH volume control - V2 Optimized
    Uses separate streams with minimal buffering (~3-5ms latency)
    """

    def __init__(
        self,
        input_device: int,
        output_device: int,
        sample_rate: int = 44100,
        channels: int = 2,
        block_size: int = 64,
        initial_volume: float = 0.3
    ):
        self.passthrough = UltraLowLatencyPassthroughV2(
            input_device=input_device,
            output_device=output_device,
            sample_rate=sample_rate,
            channels=channels,
            block_size=block_size,
            volume=initial_volume,
            enable_volume=True
        )
        self.current_volume = initial_volume
        self.is_muted = False
        self.volume_before_mute = initial_volume

    def start(self):
        """Start passthrough"""
        self.passthrough.start()

    def stop(self):
        """Stop passthrough"""
        self.passthrough.stop()

    def set_volume(self, volume: float):
        """Set volume (0.0 to 1.0)"""
        volume = max(0.0, min(1.0, volume))
        self.current_volume = volume
        self.is_muted = False
        self.passthrough.set_volume(volume)

    def volume_up(self, delta: float = 0.1):
        """Increase volume"""
        new_vol = min(1.0, self.current_volume + delta)
        self.set_volume(new_vol)

    def volume_down(self, delta: float = 0.1):
        """Decrease volume"""
        new_vol = max(0.0, self.current_volume - delta)
        self.set_volume(new_vol)

    def toggle_mute(self):
        """Toggle mute"""
        if self.is_muted:
            # Unmute
            self.current_volume = self.volume_before_mute
            self.passthrough.set_volume(self.current_volume)
            self.is_muted = False
            print(f"Unmuted - Volume: {self.current_volume*100:.0f}%")
        else:
            # Mute
            self.volume_before_mute = self.current_volume
            self.passthrough.set_volume(0.0)
            self.is_muted = True
            print(f"Muted")

    def get_volume_percent(self) -> int:
        """Get current volume as percentage"""
        return int(self.current_volume * 100)

    def get_stats(self) -> dict:
        """Get statistics"""
        stats = self.passthrough.get_stats()
        stats.update({
            'current_volume': self.current_volume,
            'is_muted': self.is_muted,
            'volume_before_mute': self.volume_before_mute
        })
        return stats


if __name__ == "__main__":
    import time
    
    # Test ultra-low latency passthrough
    print("Ultra-Low Latency Audio Passthrough Test")
    print("="*50)
    
    # Example: use device 3 for input, default for output
    passthrough = UltraLowLatencyPassthroughWithVolume(
        input_device=3,
        output_device=None,  # Default output
        sample_rate=48000,
        channels=2,
        block_size=128,
        initial_volume=0.3
    )
    
    try:
        passthrough.start()
        
        print("\n🎵 Ultra-Low Latency Mode - Interactive Controls")
        print("="*50)
        print("Controls:")
        print("  v/V: Volume Down/Up")
        print("  0-9: Set volume (0% to 90%)")
        print("  m  : Mute/Unmute")
        print("  q  : Quit")
        print("="*50)
        
        while True:
            cmd = input(f"\rVolume: {passthrough.get_volume_percent()}% {'(MUTED)' if passthrough.is_muted else ''} | Command (v/V/m/q/0-9): ").strip().lower()
            
            if cmd == 'q':
                break
            elif cmd == 'v':
                passthrough.volume_down()
            elif cmd == 'V':
                passthrough.volume_up()
            elif cmd == 'm':
                passthrough.toggle_mute()
            elif cmd in '0123456789':
                passthrough.set_volume(int(cmd) * 0.1)
        
    except KeyboardInterrupt:
        pass
    
    passthrough.stop()
    
    # Print final stats
    stats = passthrough.get_stats()
    print("\n" + "="*50)
    print("Final Statistics:")
    print(f"  Blocks processed: {stats['blocks_processed']}")
    print(f"  Estimated total latency: ~{stats['estimated_total_latency_ms']:.1f}ms")
    print(f"  Volume processing: {'enabled' if stats['volume_processing'] else 'disabled'}")
    print("="*50)
