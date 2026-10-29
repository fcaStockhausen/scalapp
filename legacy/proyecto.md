# Python AV Monitor (Latency-Tolerant)

Proyecto en **Python** para **visualizar una capturadora HDMI USB** con **audio + video sincronizados**, aceptando **latencia adicional**, orientado a **streaming casual / monitoreo**, evitando artefactos de blur temporal perceptual.

> Este proyecto **no es tiempo real estricto**. La prioridad es **sincronización A/V estable**, no baja latencia.

---

## Objetivo

Construir una aplicación en Python (macOS Apple Silicon) que:

* Capture video UVC (HDMI → USB)
* Capture audio UAC
* Introduzca buffering intencional
* Use **audio como reloj maestro**
* Renderice video sin frame blending perceptual
* Sea suficientemente estable para streaming casual (Discord / Twitch)

---

## Supuestos explícitos

* Latencia aceptable: **300–800 ms**
* FPS de entrada: 60 Hz
* Monitor: 60–120 Hz
* No se requiere fullscreen exclusivo
* macOS M2

---

## Principios de diseño

1. **Audio define el tiempo**
2. Video se muestra por *frame hold* (no interpolación)
3. Buffering explícito para absorber jitter
4. Render simple (no vsync perfecto)
5. Prioridad a claridad de texto

---

## Arquitectura

```
UVC Video Capture ─┐
                    ├─► Timestamped Queues ─► Sync Controller ─► Renderer
UAC Audio Capture ─┘                     ↑
                                   Audio Clock
```

---

## Stack tecnológico

### Lenguaje

* **Python 3.11+**

### Librerías

* **OpenCV** — captura de video
* **sounddevice** — captura y playback de audio
* **NumPy** — buffers
* **PyAV (opcional)** — timestamps más precisos
* **PyQt6 / Tkinter** — ventana de render

---

## Flujo de datos

### Video

1. Captura frame
2. Timestamp (`time.monotonic_ns()`)
3. Push a cola FIFO
4. Renderer consume último frame <= audio_time
5. Frame se mantiene hasta nuevo válido

### Audio

1. Captura bloques PCM
2. Push a buffer circular
3. Playback con retraso fijo (latency buffer)
4. Posición de playback define **audio_time**

---

## Política de sincronización

### Audio

* Playback con buffer inicial (ej: 500 ms)
* Clock = posición reproducida
* No detener audio por video

### Video

* Nunca interpolar
* Repetir último frame válido
* Si video va atrasado → dropear
