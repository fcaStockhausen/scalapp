# Python AV Monitor (Latency-Tolerant)

Proyecto en **Python** para **visualizar una capturadora HDMI USB** con **audio + video sincronizados**, aceptando **latencia adicional**, orientado a **streaming casual / monitoreo**, evitando artefactos de blur temporal perceptual.

> Este proyecto **no es tiempo real estricto**. La prioridad es **sincronización A/V estable**, no baja latencia. **Excepción**: El modo `--audio-only` (DEFAULT: Direct Passthrough ~6-8ms) y `--ultra-low-latency` (~10-20ms) están optimizados para **latencia ultra-baja**.

## Características

- ✅ Captura de video UVC (HDMI → USB) con OpenCV
- ✅ Captura de audio UAC con sounddevice  
- ✅ **Audio como reloj maestro** para sincronización
- ✅ Buffering intencional (300-800ms latencia)
- ✅ **Frame hold strategy** - sin interpolación temporal
- ✅ Interfaz PyQt6 con overlays de información
- ✅ Monitoreo de A/V sync con estadísticas
- ✅ Soporte para macOS Apple Silicon

## Arquitectura

```
UVC Video Capture ─┐
                    ├─► Timestamped Queues ─► Sync Controller ─► Renderer
UAC Audio Capture ─┘                     ↑
                                   Audio Clock
```

## Instalación

### Virtual Environment (Recomendado)

```bash
# Crear y activar entorno virtual
python3 -m venv venv
source venv/bin/activate  # macOS/Linux
# o en Windows: venv\Scripts\activate

# Instalar dependencias
pip install -r requirements.txt
```

### Conda Environment (Alternativa)

```bash
# Crear ambiente conda
conda create -n scalapp python=3.11 -y
conda activate scalapp

# Instalar dependencias
pip install -r requirements.txt
```

### Dependencias

- **Python 3.11+**
- **OpenCV** — captura de video
- **sounddevice** — captura y playback de audio
- **NumPy** — buffers numéricos
- **PyQt6** — interfaz gráfica
- **PyAV** — timestamps precisos (opcional)

## Uso

### Básico

```bash
# Usar dispositivos por defecto
python main.py

# Dispositivos específicos
python main.py --audio-device 2 --video-device 1

# Listar dispositivos disponibles
python main.py --list-devices
```

### Modo Audio-Only (Ultra Baja Latencia) - DEFAULT: Direct Passthrough ~6-8ms

```bash
# Audio-only mode con Direct Passthrough por DEFAULT (~6-8ms)
python main.py --audio-only

# Audio-only con Low Latency normal (~20-30ms)
python main.py --audio-only --no-direct-passthrough

# Ultra-low latency mode (8ms) - máximo rendimiento
python main.py --audio-only --ultra-low-latency

# Audio-only con latencia personalizada
python main.py --audio-only --audio-latency 0.008  # 8ms ultra-minimal
python main.py --audio-only --audio-latency 0.01   # 10ms ultra-low
python main.py --audio-only --audio-latency 0.015  # 15ms low
python main.py --audio-only --audio-latency 0.02   # 20ms balanced
python main.py --audio-only --audio-latency 0.03   # 30ms very stable

# Tamaño de bloque personalizado (Direct Passthrough V4)
python main.py --audio-only --block-size 64   # 1.45ms - ultra minimal (puede causar dropouts)
python main.py --audio-only --block-size 128  # 2.90ms - ultra low latency
python main.py --audio-only --block-size 256  # 5.80ms - low (default, como low-latency mode)
python main.py --audio-only --block-size 512  # 11.61ms - balanced
python main.py --audio-only --block-size 1024 # 23.22ms - very stable
```

**Controles en modo audio-only:**
- `v`/`V`: Bajar/Subir volumen
- `0-9`: Volumen fijo (0% a 90%)
- `m`: Mute/Unmute
- `q`: Salir

### Avanzado

```bash
# Buffer de latencia personalizado (0.5s default)
python main.py --latency 1.0

# Habilitar monitoreo con estadísticas
python main.py --monitor

# Configuración de video
python main.py --video-width 1280 --video-height 720 --video-fps 30

# Configuración de audio
python main.py --audio-rate 48000
```

## Principios de Diseño

1. **Audio define el tiempo** - Audio clock es maestro
2. **Video frame hold** - Sin interpolación, repite último frame válido
3. **Buffering explícito** - Absorbe jitter de captura
4. **Prioridad a sincronización** - Estable sobre baja latencia
5. **Claridad visual** - Optimizado para texto legible

## Optimización de Latencia (Modo Audio-Only)

El modo `--audio-only` y `--ultra-low-latency` están optimizados específicamente para **latencia mínima pero estable**:

### Parámetros por Modo

| Modo | Block Size | Latencia Buffer | Latencia Total Esperada | CPU | Estabilidad |
|------|------------|----------------|------------------------|-----|------------|
| **Direct Passthrough V4** ⚡ | 256 (~5.8ms) | 0ms | **~6-8ms** | Muy baja | Muy alta |
| **Ultra-minimal** | 128 (~3ms) | 8ms | ~10-20ms | Alta | Media |
| **Ultra-low** | 128 (~3ms) | 8ms | ~10-20ms | Alta | Media |
| **Low (default)** | 256 (~6ms) | 15ms | ~20-30ms | Media | Alta |
| **Balanced** | 256 (~6ms) | 20ms | ~25-35ms | Media | Alta |
| **Stable** | 512 (~12ms) | 30ms | ~35-45ms | Baja | Muy alta |

### Latencia Total Esperada

#### ⚡ Direct Passthrough V4 Mode (~6-8ms) - DEFAULT en --audio-only - LOW LATENCY CLONE
```bash
python main.py --audio-only
# (equivalente a: python main.py --audio-only --direct-passthrough)

# Con block size personalizado:
python main.py --audio-only --block-size 64   # ~2.5ms
python main.py --audio-only --block-size 128  # ~4ms
python main.py --audio-only --block-size 512  # ~13ms
```
- **Total**: **~6-8ms** (con block_size 256, 70-80% menos que audio-only normal!)
- **Block size**: 256 muestras @ 44.1kHz (~5.8ms) - **¡PERSONALIZABLE!**
- **CPU**: Muy baja (streams separadas, buffer pequeño)
- **Uso ideal**: Monitoreo profesional, instrumentos musicales, karaoke
- **Por qué es default**: Siente igual o más rápido que low-latency mode
- **Block size personalizable** (`--block-size`):
  - `64` samples (~1.45ms) - Ultra minimal (puede causar dropouts)
  - `128` samples (~2.90ms) - Ultra low latency
  - `256` samples (~5.80ms) - Default, muy estable
  - `512` samples (~11.61ms) - Balanced
  - `1024` samples (~23.22ms) - Very stable
- **V4 mejoras sobre V3**:
  - ✓ **Streams separadas** (como low-latency mode)
  - ✓ **Block size 256** (más estable que 128, menos overhead)
  - ✓ **Buffer pequeño de 5 bloques** (como low-latency mode)
  - ✓ **latency='low'** en AMBAS streams (misma config que low-latency)
  - ✓ **Misma arquitectura que low-latency mode** pero sin circular buffer
- **Qué elimina sobre V3 y versiones anteriores**:
  - ✗ Circular buffer de AudioCapture (no necesario)
  - ✗ SyncController threading (no necesario)
  - ✗ Duplex stream overhead (streams separadas más estables)
- **Qué mantiene de low-latency mode**:
  - ✓ Streams separadas con latency='low'
  - ✓ Block size 256 (más estable)
  - ✓ Buffer pequeño de playback (5 bloques)
  - ✓ Control de volumen
  - ✓ Mute/unmute
  - ✓ Controles interactivos
- **Comparación**:
  - V3: Duplex, block 128 - overhead de callback frecuente
  - V4: Separate streams, block 256 - MÁS ESTABLE, se siente más rápido
- **Ver detalles**: `PROCESAMIENTO_ELIMINADO.md`

#### Modo Ultra-Low Latency (8ms) - Máximo rendimiento
```bash
python main.py --audio-only --ultra-low-latency
# o
python main.py --audio-only --audio-latency 0.008 --block-size 128
```
- **Total**: ~10-20ms
- **Block size**: 128 muestras (~2.9ms)
- **CPU**: Alta (callbacks frecuentes)
- **Uso ideal**: Monitoreo profesional, sistemas Apple Silicon M1/M2/M3 con CPU disponible

#### Modo Low Latency (15ms) - Sin Direct Passthrough
```bash
python main.py --audio-only --no-direct-passthrough
# o
python main.py --audio-only --audio-latency 0.015
```
- **Total**: ~20-30ms
- **Block size**: 256 muestras (~5.8ms)
- **CPU**: Media
- **Uso ideal**: Monitoreo general, balance entre latencia y estabilidad
- **Nota**: Configuración por defecto, sin dropouts en M2

#### Modo Balanced (20-30ms)
```bash
python main.py --audio-only --audio-latency 0.02
# o
python main.py --audio-only --audio-latency 0.03
```
- **Total**: ~25-45ms
- **Block size**: 256-512 muestras (~6-12ms)
- **CPU**: Baja-Media
- **Uso ideal**: Sistemas más lentos, CPU limitada

### Optimizaciones Implementadas

1. **Bloques pequeños** (128-256 vs 1024): Callbacks más frecuentes (~3-6ms)
2. **Buffer de playback controlado**: 3-5 bloques máximo (~15-30ms)
3. **Low latency mode**: Configuración `latency='low'` en sounddevice
4. **Latencia de sincronización mínima**: 8-15ms en low/ultra-low mode
5. **Buffer management**: En low-latency mode, mantiene 1-2 bloques para evitar dropouts mientras minimiza latencia
6. **Audio time offset reducido**: Solo 5-8ms de offset en low/ultra-low mode (vs 500ms en modo normal)

### Recomendación para tu Apple Silicon M2
```bash
# ⚡ Opción por DEFAULT en --audio-only (latencia mínima, ~5-8ms)
python main.py --audio-only
# Resultado: ~5-8ms latencia total, muy estable, CPU baja

# Opción audio-only sin direct passthrough (sin dropouts, latencia baja)
python main.py --audio-only --no-direct-passthrough
# Resultado: ~20-30ms latencia total, muy estable

# Opción ultra (mínima latencia, posible dropout si CPU está ocupada)
python main.py --audio-only --ultra-low-latency
# Resultado: ~10-20ms latencia total, alta CPU
```

1. **Audio define el tiempo** - Audio clock es maestro
2. **Video frame hold** - Sin interpolación, repite último frame válido
3. **Buffering explícito** - Absorbe jitter de captura
4. **Prioridad a sincronización** - Estable sobre baja latencia
5. **Claridad visual** - Optimizado para texto legible

## Configuración

### Parámetros de Latencia

- **Latencia aceptable**: 300–800 ms
- **Buffer inicial**: 500 ms (configurable)
- **Drift máximo**: 100 ms antes de drop de frame

### Resolución Soportada

- **Entrada**: Hasta 4K60 (depende de capturadora)
- **Salida**: Escalado automático a ventana
- **FPS**: 60 Hz recomendado, 30 Hz mínimo

## Flujo de Sincronización

### Audio
1. Captura con buffer inicial (ej: 500ms)
2. Clock = posición reproducida
3. Audio nunca se detiene por video

### Video  
1. Timestamp con `time.monotonic_ns()`
2. Frame válido si timestamp ≤ audio_time
3. Drop si video está >100ms atrasado
4. Frame hold si no hay frame más reciente

## Interfaz

### Ventana Principal
- Display de video sincronizado
- Overlays opcionales: frame number, timestamp, FPS
- Panel de control: toggle overlays/stats
- Barra de estado: A/V diff, sync status, frames dropped

### Monitoreo (opcional)
- Drift promedio y máximo
- Frames dropped totales  
- Calidad de sincronización
- Reporte final al cerrar

## Troubleshooting

### Dispositivos no encontrados
```bash
# Listar dispositivos disponibles
python main.py --list-devices
```

### Audio no funciona
- Verificar que la capturadora HDMI aparece como dispositivo de audio
- Probar diferentes `--audio-device` IDs
- Aumentar `--latency` si hay dropouts

### Video lag o tearing
- Reducir resolución/FPS con `--video-*` flags
- Aumentar `--latency` para más buffering
- Verificar cable USB 3.0+ para capturadora

### Sincronización pobre
- Aumentar `--latency` a 1.0s
- Habilitar `--monitor` para diagnóstico
- Verificar que ambos dispositivos usan misma fuente HDMI

## Arquitectura Interna

### Módulos

- `audio_capture.py` - Captura y buffering de audio UAC
- `video_capture.py` - Captura y timestamping de video UVC  
- `sync_controller.py` - Control de sincronización A/V
- `video_renderer.py` - Display PyQt6 con overlays
- `main.py` - Entry point y coordinación

### Threading

- **Capture Thread** - Video UVC (OpenCV)
- **Audio Callback** - Audio UAC (sounddevice)  
- **Sync Thread** - Coordinación A/V
- **Render Thread** - Display PyQt6
- **Monitor Thread** - Estadísticas (opcional)

## Licencia

MIT License - ver archivo LICENSE para detalles.