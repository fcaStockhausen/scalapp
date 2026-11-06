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

### Fase 1 — Medir y desatorar (barato) — ✅ HECHA (29/10)
- [x] Contador de fps del RENDERER (frames pintados/s) junto al de
      captura. Sin esto seguimos adivinando dónde mueren los frames.
- [x] Cambiar la cola por **single-slot latest-wins**
      (`Mutex<Option<Arc<Frame>>>`): el renderer toma SIEMPRE el último;
      latencia de cola = 0 por construcción. Con contador de frames
      *skipped* (sobrescritos sin mostrarse) y *frame age* (ms entre
      timestamp de captura y ahora) en el panel de stats.
- [x] Pedir formato exacto al dispositivo (1080p60), no
      AbsoluteHighestResolution.
- [x] `request_repaint_after(8ms)` como parche mientras llega Fase 2.

**Resultado medido (MS2131):** captura 1920x1080@**60fps reales**
(antes negociaba 30), drift A/V +3..13ms, skipped estable en ~10
(solo warmup) → el renderer consume los 60fps. Conversión RGB→RGBA en
buffer reutilizado (adiós alloc de 8.3MB/frame).

**Hallazgos de terreno (Fase 1):**
- La MS2131 expone 1920x1080@60 como **YUYV**: macOS NO lista sus
  modos MJPEG. El pedido exacto va con fourcc YUYV (nokhwa's `set_all`
  casa solo resolución+fps, pero `Closest` filtra por fourcc y muere
  con MJPEG).
- El orden de dispositivos de AVFoundation **cambia entre sesiones de
  discovery dentro del mismo proceso**: `query()` e `Camera::new(Index)`
  pueden ver órdenes distintos. Solución: abrir por **uniqueID**
  (`CameraIndex::String`), deterministic. El índice quedó solo para
  `--video-device N` explícito (con escaneo de respaldo).
- Un proceso que queda vivo con la capturadora abierta deja el
  dispositivo en "Lock Rejected" para todos los demás (nokhwa no libera
  limpio en todos los paths). Síntoma: `set_all` falla con
  `lockForConfiguration`. Matar el proceso libera.
- **Bug del binding nokhwa en modos duplicados (29/10, USB2)**: el
  `set_all` deja como formato activo la ÚLTIMA entrada con las
  dimensiones pedidas, pero el rango de fps puede venir de un
  duplicado anterior → par inconsistente → NSException que mata el
  proceso. En USB2 la MS2131 lista los modos con duplicados
  asimétricos (uno completo, uno limitado), así que 1080p@50/30/20
  "existen" en la lista pero explotan al pedirlos. **Regla segura:
  pedir solo entradas que sean la última de sus dimensiones** —
  verificado 11/11 en USB2. scalapp ahora pre-valida la lista real
  por AVFoundation (binding directo, sin HID) y elige modo seguro;
  además todos los opens van bajo catch_unwind. En USB2 el mejor modo
  seguro es 800x600@60 (o 1080p@10 con --video-fps 10).

### Fase 2 — Push en vez de poll (medio hecha: 29/10)
- [ ] Reemplazar nokhwa por **AVFoundation con delegate**
      (AVCaptureVideoDataOutput entrega CMSampleBuffer por callback:
      push, sin polling, sin decode CPU — llega BGRA listo).
      Opciones: nokhwa moderno si ya lo soporta limpio, o binding
      directo objc2-avfoundation.
- [x] El callback del frame **dispara el repaint** (ctx.request_repaint
      via notifier del thread de captura — 29/10): el render ocurre
      exactamente cuando hay frame nuevo. Implementado sin reescribir
      nokhwa: el thread de captura llama un notifier al almacenar en
      el slot latest-wins. Resultado medido: out 60.0-60.7 estable
      (antes caía a ~40 cuando el loop de repaint decaía y perdía
      ráfagas). El repaint por timer (8ms) queda como heartbeat.

### Fase 3 — Zero-copy (el "direct output" de verdad)
- [ ] CVPixelBuffer → Metal texture via CVOpenGLTextureCache /
      texture cache: pintar el buffer del dispositivo DIRECTO como
      textura GPU, cero copias CPU.
- [ ] Render por wgpu/metal puro (egui queda para overlay) o viewport
      egui con textura externa.

## Notas del terreno
- La unidad (MS2131) es marginal: renegociaciones de formato la
  estresan. Menos cambios de modo = mejor (pedir UN formato y no
  tocarlo más es también política de salud del hardware). El probe del
  autodetect ahora pide el mismo modo exacto que usará la captura, así
  el open de la captura no renegocia nada.
- ~~El autodetect de video (devices.rs) enumera 2560x1440@30 como
  "primera"~~ Resuelto en Fase 1: autodetect por query() + uniqueID,
  pidiendo 1080p60 exacto (reporta "USB3 Video 1920x1080@60fps").
- **La MS2131 acopla su reloj de video al audio** (29/10, medido con
  FFT de la captura cruda): con video streameando aparece un tono PURO
  a 60.06 Hz (≈ refresco vertical 59.94) + armónicos 120/180 +
  subarmónico ~30 Hz + rumor 28-38 Hz, +7 dB sobre el nivel audio-only.
  NO es hum de red (Chile = 50 Hz). Nace dentro del chip (la captura
  es digital de punta a punta). Mitigación software: `--notch-hum`
  (notches 60.06/120.12/180.18 a Q=25 — quirúrgico, B1/B2 a -1.9dB) +
  `--hpf <hz>` opcional para el rumor 28-38Hz (cuesta sub-bajo del
  contenido: E1 -9dB/A1 -4dB con 48Hz). Validado grabando la salida:
  tono fuera del espectro. Los Hz exactos pueden
  variar con la fuente: si cambia el refresh (50 Hz PAL etc.), hay que
  re-medir (SCALAPP_REC + SCALAPP_REC_OUT para captura/salida cruda).
- **La MS2131 dropea audio isócrono mientras streamea video** (30/10):
  ráfagas de 100-210ms cada ~1min (~3ms/s promedio), independiente de
  resolución/fps (mismo ritmo a 1080p60 y 720p60); sin video activo,
  0 gaps en 3min. Es firmware — no prevenible host-side, solo
  amortiguable. Fix: colchón adaptativo en el playback (50ms base,
  sube a gap+50ms al observar un gap, tope 350ms) + clamp de deriva
  asimétrico (-1% en déficit profundo para recuperar entre ráfagas,
  +0.2% en exceso; cerca del target el control es lineal ~-0.07%,
  imperceptible). Resultado: 1 blip por sesión (el primer gap), el
  resto absorbido. El audio queda a ~160-170ms de latencia → si el
  lip-sync importa, retrasar el video para emparejar es un ítem
  futuro (modo "latency tolerant" moderno).
- audio.rs/passthrough.rs quedaron como referencia de motor probado
  (deriva compensada, watchdog, colchón adaptativo) — el video puede
  copiar el patrón de latest-wins del consumer.
- **egui DUPLICA por diseño todo `request_repaint()` de delay cero**
  (6/11, hallado en fuente egui-0.29.1 context.rs:176: "Each request
  results in two repaints, just to give some things time to settle" —
  parche para tooltips/respuestas retardadas de UI). Nuestro arrival
  notifier llamaba `request_repaint()` por frame → 60 llegadas/s se
  volvían 120 pasadas/s de egui (loop 120.0 exacto) = doble CPU/GPU/
  WindowServer presentando contenido idéntico. Fix: el notifier usa
  `request_repaint_after(1ms)` — delay>0 no duplica, y el callback
  dispara igual de inmediato (egui resta `predicted_dt` del delay).
  Esperado post-fix: loop ~60-61, consumo a la mitad en la parte de
  egui/WindowServer. NOTA: el patrón arrival→texture.set+request_repaint
  es el canónico (egui-video hace exactamente lo mismo, línea 407 de su
  lib.rs) — el desperdicio es de egui, no del patrón.
  Correlación empírica que este fix explica: las ventanas con loop 120.0
  tenían pintado limpio (16.4/16.7/17.1); las de loop ~80-88 (vsync del
  display sin slots para 120 pedidos/s) pintado bimodal 5.5-7.9/25-28ms
  = judder. Pendiente de validación foreground: si con el loop a ~60 el
  pintado queda limpio en TODAS las ventanas, el judder muere acá.
