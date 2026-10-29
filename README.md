# ScalApp AV Monitor (Rust)

Monitor A/V en **Rust** para **visualizar una capturadora HDMI USB** con **audio + video sincronizados**. Esta es la versión canónica; el port original en Python vive en [`legacy/`](legacy/).

## Por qué Rust

- El pipeline de audio (passthrough ~6-8ms) corre en callbacks nativos de CoreAudio sin pasar por el intérprete de Python en cada bloque.
- Los frames de video se mueven por referencias (`Arc`) sin copias innecesarias; la conversión RGB→textura es la única copia.
- Binario único sin venv, sin dependencias Python.

## Arquitectura (igual que la versión Python)

```
UVC Video Capture ─┐
                    ├─► Timestamped Queues ─► Sync Controller ─► Renderer (egui)
UAC Audio Capture ─┘                     ↑
                                   Audio Clock
```

| Python | Rust | Crate |
|---|---|---|
| sounddevice | cpal (CoreAudio nativo) | `cpal 0.15` |
| OpenCV VideoCapture | nokhwa (AVFoundation nativo) | `nokhwa 0.10` |
| PyQt6 | egui/eframe (GPU) | `eframe 0.29` |
| NumPy buffers | buffers planos `f32`/`u8` | std |

## Build

```bash
cargo build --release
# binario: target/release/scalapp
```

Requiere Rust stable. En macOS usa AVFoundation + CoreAudio nativos, **no necesita OpenCV ni PortAudio instalados**.

## Uso (flags idénticos a la versión Python)

```bash
# Audio-only por defecto, Direct Passthrough V4 (~6-8ms)
scalapp

# Con block size personalizado
scalapp --block-size 128   # ~4ms
scalapp --block-size 512   # ~13ms

# Ultra-low latency (8ms, sin passthrough directo)
scalapp --ultra-low-latency

# Modo completo A/V sincronizado
scalapp --video                            # auto-detecta capturadora
scalapp --video-device 1 --monitor --latency 1.0

# Utilidades
scalapp --list-devices           # lista capturadoras HDMI detectadas
scalapp --test-audio             # prueba dispositivos con detección de señal
```

### Controles modo audio-only (consola)

`v`/`V`: bajar/subir volumen · `0-9`: volumen fijo · `m`: mute · `q`: salir

### Controles modo A/V (ventana)

Checkboxes **Show Overlay**/**Show Stats**, slider de volumen, botón **Borderless Mode**, **Quit**.
Teclas: `F` borderless/fullscreen · `O` overlay · `S` stats · `Esc` salir de fullscreen · `Q` salir.

## Módulos

```
src/
├── main.rs          CLI, orquestación (equivale a main.py)
├── audio.rs         AudioCapture + AudioPlayback (audio_capture.py)
├── passthrough.rs   Passthrough V4 con volumen (ultra_low_latency_passthrough.py)
├── video.rs         VideoCapture con buffer timestamped (video_capture.py)
├── sync.rs          SyncController + AVMonitor (sync_controller.py)
├── renderer.rs      Ventana egui con overlays (video_renderer.py)
├── devices.rs       Auto-detección HDMI + test-audio + list-devices
└── atomicf32.rs     Helper para volumen lock-free entre callbacks
```

## Diferencias respecto al Python

1. **Fix**: `V` (subir volumen) no funcionaba en Python porque el input se pasaba a minúsculas antes de comparar. Aquí `V` sube volumen.
2. **IDs de dispositivos**: Python usa los índices intercalados de PortAudio (input+output juntos); Rust numera inputs y outputs por separado (los IDs mostrados por `--list-devices` son consistentes entre sí).
3. **Captura de video**: nokhwa negocia la resolución más alta disponible (las capturadoras HDMI entregan 1080p/60 MJPG); Python forzaba FOURCC MJPG, que el backend AVFoundation de OpenCV ignoraba en la práctica.
4. El panel flotante con hover del renderer PyQt se reemplaza por la barra de controles + teclas; en modo borderless los controles se ocultan igual.

## Tests

```bash
cargo test    # escala de volumen, ring buffer, frame-hold, drift stats, parser de comandos
cargo clippy  # sin warnings
```

Nota: `cargo build` puede avisar sobre `block v0.1.6` (dependencia transitiva de nokhwa); es upstream y no afecta.
