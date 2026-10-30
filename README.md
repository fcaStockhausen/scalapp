# ScalApp AV Monitor (Rust)

Monitor A/V en **Rust** para **visualizar una capturadora HDMI USB** con **audio + video sincronizados**. Esta es la versión canónica; el port original en Python vive en [`legacy/`](legacy/).

## Por qué Rust

- El pipeline de audio (passthrough ~6-8ms) corre en callbacks nativos de CoreAudio sin pasar por el intérprete de Python en cada bloque.
- Los frames de video se mueven por referencias (`Arc`) sin copias innecesarias; la conversión RGB→textura es la única copia.
- Binario único sin venv, sin dependencias Python.

## Arquitectura (igual que la versión Python)

```
UVC Video Capture ─┐
                    ├─► Latest-wins slot ─► Renderer (egui)   SyncController = monitor de drift
UAC Audio Capture ─┘      (single)              ↑                  (reloj de audio vs video)
                                              Audio Clock
```

El video corre con semántica **latest-wins** (Fase 1 del ROADMAP):
single-slot, el renderer pinta siempre el último frame disponible
(latencia de cola = 0), la captura pide el formato exacto
1920x1080@60 al dispositivo y el overlay mide por separado fps de
captura, fps pintados, frames skipped y edad del frame.

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

# Arrancar con ventana borderless — sin barra de título ni bordes
# (NO fullscreen: la ventana conserva tamaño y posición). Volumen default 20%.
scalapp --video --borderless --notch-hum

# Ultra-low latency (8ms, sin passthrough directo)
scalapp --ultra-low-latency

# Modo completo A/V sincronizado
scalapp --video                            # auto-detecta capturadora
scalapp --video-device 1 --monitor --latency 1.0

# Colchón de audio en modo video (la capturadora dropea audio en ráfagas
# con video activo; el colchón adaptativo lo cubre solo, esto fija la base)
scalapp --video --audio-cushion-ms 150

# Filtro anti-hum del chip (tono ~60Hz + armónicos con video streameando).
# Quirúrgico: Q=25 toca B1/B2 a menos de -2dB, el resto del audio intacto.
scalapp --video --notch-hum

# + high-pass opcional para el rumor 28-38Hz (cuesta sub-bajo del contenido:
# con 48Hz, E1/41Hz -9dB, A1/55Hz -4dB) — solo si el sub-bajo no importa
scalapp --video --notch-hum --hpf 48

# Utilidades
scalapp --list-devices           # lista capturadoras HDMI detectadas
scalapp --test-audio             # prueba dispositivos con detección de señal
```

### Controles modo audio-only (consola)

`v`/`V`: bajar/subir volumen · `0-9`: volumen fijo · `m`: mute · `q`: salir

### Controles modo A/V (ventana)

Checkboxes **Show Overlay**/**Show Stats**, slider de volumen, botón **Borderless Mode**, **Quit**.
Teclas: `F` borderless (sin barra de título, no fullscreen) · `O` overlay · `S` stats · `Esc` volver a ventana normal · `Q` salir.

## Módulos

```
src/
├── main.rs          CLI, orquestación (equivale a main.py)
├── audio.rs         AudioCapture + AudioPlayback (audio_capture.py)
├── passthrough.rs   Passthrough V4 con volumen (ultra_low_latency_passthrough.py)
├── video.rs         VideoCapture con slot latest-wins + formato exacto (video_capture.py)
├── sync.rs          SyncController (monitor de drift A/V) + AVMonitor
├── renderer.rs      Ventana egui con overlays y medidores de fps
├── devices.rs       Auto-detección HDMI (por uniqueID) + test-audio + list-devices
└── atomicf32.rs     Helper para volumen lock-free entre callbacks
```

## Diferencias respecto al Python

1. **Fix**: `V` (subir volumen) no funcionaba en Python porque el input se pasaba a minúsculas antes de comparar. Aquí `V` sube volumen.
2. **IDs de dispositivos**: Python usa los índices intercalados de PortAudio (input+output juntos); Rust numera inputs y outputs por separado (los IDs mostrados por `--list-devices` son consistentes entre sí).
3. **Captura de video**: se pide el formato exacto 1920x1080@60 (YUYV — macOS no expone los modos MJPEG de la MS2131) y se abre por uniqueID de AVFoundation porque el orden de índices cambia entre sesiones. Python forzaba FOURCC MJPG, que el backend AVFoundation de OpenCV ignoraba en la práctica.
4. El panel flotante con hover del renderer PyQt se reemplaza por la barra de controles + teclas; en modo borderless los controles se ocultan igual.

## Tests

```bash
cargo test    # escala de volumen, ring buffer, latest-wins slot, drift stats, parser de comandos
cargo clippy  # sin warnings nuevos (quedan 6 de clippy 1.98 sobre código previo: passthrough.rs/main.rs)
```

Nota: `cargo build` puede avisar sobre `block v0.1.6` (dependencia transitiva de nokhwa); es upstream y no afecta.
