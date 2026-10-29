# ROADMAP — Cadena de video: "direct output"

Objetivo (Felipe, 29/10): que el video de la ventana corra **como un
direct output de la capturadora** — 60fps reales de punta a punta y la
mínima intervención posible entre el frame que entrega el dispositivo y
el píxel en pantalla.

## Estado actual (audio ✅, video ❌)

Audio quedó resuelto (tasa real 48k + bloques chicos + motor con deriva
compensada ~30-50ms, sin crackle). El video **dice** 60fps pero no se
ve así: el contador `fps_actual` mide la CAPTURA, no lo que llega a
pantalla — los frames mueren más adelante en la cadena y nadie los
cuenta.

## La cadena actual y sus cuellos de botella

```
nokhwa (poll) → decode RgbFormat (CPU) → VecDeque 120 frames → pull_frame → textura egui (CPU→GPU 8.3MB/frame) → pintar
```

1. **nokhwa 0.10 con loop de POLLING** (`camera.frame()` en un while).
   No hay callback: el thread pregunta, duerme 10ms en errores, y el
   frame llega cuando llega. Además pedimos
   `AbsoluteHighestResolution` — que es lo que provocó la negociación
   rara de 2560x1440@30 del autodetect. Hay que pedir el formato
   EXACTO (1920x1080@60 o el nativo de la entrada).
2. **`decode_image::<RgbFormat>()` en CPU por frame** — decodificación
   y conversión de pixel format en el thread de captura, a 60fps de
   1080p es trabajo bruto innecesario.
3. **VecDeque de 120 frames (¡2 segundos!)** — semántica de cola: si el
   renderer se atrasa, la cola absorbe el atraso como LATENCIA, no como
   drop. Para monitoreo queremos lo inverso: **latest-wins**.
4. **`request_repaint_after(15ms)`** — repinta a ~66Hz por timer, no
   sincronizado con la llegada de frames. El frame nuevo puede esperar
   hasta 15ms antes de que alguien lo mire, y el pipeline entero
   acumula jitter.
5. **Subida de textura egui por frame** — 1920×1080×4 = 8.3MB CPU→GPU
   por frame (~500MB/s a 60fps). Funciona, pero es la parte más cara
   del camino; egui además puede re-allocar.

## Plan de mejora (en orden de relación costo/beneficio)

### Fase 1 — Medir y desatorar (barato)
- [ ] Contador de fps del RENDERER (frames pintados/s) junto al de
      captura. Sin esto seguimos adivinando dónde mueren los frames.
- [ ] Cambiar la cola por **single-slot latest-wins** (ArcSwap o
      Mutex<Option<Arc<Frame>>>): el renderer toma SIEMPRE el último;
      latencia de cola = 0 por construcción.
- [ ] Pedir formato exacto al dispositivo (1080p60), no
      AbsoluteHighestResolution.
- [ ] `request_repaint_after(8ms)` como parche mientras llega Fase 2.

### Fase 2 — Push en vez de poll
- [ ] Reemplazar nokhwa por **AVFoundation con delegate**
      (AVCaptureVideoDataOutput entrega CMSampleBuffer por callback:
      push, sin polling, sin decode CPU — llega BGRA listo).
      Opciones: nokhwa moderno si ya lo soporta limpio, o binding
      directo objc2-avfoundation.
- [ ] El callback del frame **dispara el repaint** (ctx.request_repaint
      via waker): renderizamos exactamente cuando hay frame nuevo.

### Fase 3 — Zero-copy (el "direct output" de verdad)
- [ ] CVPixelBuffer → Metal texture via CVOpenGLTextureCache /
      texture cache: pintar el buffer del dispositivo DIRECTO como
      textura GPU, cero copias CPU.
- [ ] Render por wgpu/metal puro (egui queda para overlay) o viewport
      egui con textura externa.

## Notas del terreno
- La unidad (MS2131) es marginal: renegociaciones de formato la
  estresan. Menos cambios de modo = mejor (pedir UN formato y no
  tocarlo más es también política de salud del hardware).
- El autodetect de video (devices.rs) enumera 2560x1440@30 como
  "primera" — cuidado al elegir formato por defecto.
- audio.rs/passthrough.rs quedaron como referencia de motor probado
  (deriva compensada, watchdog, colchón adaptativo) — el video puede
  copiar el patrón de latest-wins del consumer.
