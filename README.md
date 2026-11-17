# scalapp — Monitor A/V de baja latencia para capturadoras MS2131 (macOS)

Monitor de audio+video en Rust para la capturadora HDMI **MacroSilicon
MS2131** (1080p60 YUYV por USB3), pensado para usar la Mac **como si
fuera el monitor**: fluidez de 60fps reales, latencia baja, audio limpio
y bajo consumo.

> **Se usa junto a [`firmware_capturadora`](../firmware_capturadora)** —
> el repo hermano de ingeniería inversa del chip (firmware 8051,
> registros, parches de flash). Los hallazgos de ese repo
> (registro de diezmado, hum de 60Hz, mapa XDATA del bloque de audio)
> se convirtieron en los fixes y mitigaciones que corren acá. Ver
> [Cómo se usan juntos](#cómo-se-usan-juntos).

## Features

- **Direct output**: latest-wins de un slot, pinta en la llegada del
  frame (notifier → repaint), sin timers compitiendo. 60fps exactos.
- **Decoder propio YUYV→RGBA en una pasada** (coeficientes BT.601
  exactos de nokhwa, test byte-a-byte), ajuste de color en dominio
  YUV. CPU 92%→19% medido en el camino.
- **fps NATIVAS al device** (`--30`, `--video-fps N`): arreglo del
  binding nokhwa vendoreado (doble bug: desempate de duplicados +
  readback que mentía la fps máxima) — ver `vendor/`.
- **Notch anti-hum ADAPTATIVO**: el chip inyecta tonos en el audio con
  video activo (par batiendo ~1Hz); scalapp los mide en vivo (Goertzel)
  y re-afina los notches quirúrgicos en caliente (Q=100, el bajo del
  juego sobrevive).
- **Recuperación de unplug/replug**: supervisor + puller (nokhwa se
  cuelga para siempre al caer el USB), re-conexión por uniqueID
  tolerante a cambio de puerto, cartel "SIN SEÑAL".
- **Anti-webcam**: mapa clasificado de dispositivos; webcams/virtuales
  jamás se abren.
- `--dump-modes` (lista UVC completa), `--borderless`, `--list-devices`.

## Uso

```bash
cargo build --release
./target/release/scalapp --video              # A/V completo 1080p60
./target/release/scalapp --video --30         # 30fps nativos (económico)
./target/release/scalapp --video --dump-modes # diagnóstico de modos
```

Teclas: `F` borderless · `P` pixel-perfect (1 texel = 1 px físico) ·
`M` mipmaps · `S`/`O` overlays · `Q` salir.

Diagnóstico de audio: `SCALAPP_REC=/tmp/in.raw` + `python3
tools/audio_diag.py` (espectro fino, detector de drops, AM del hum).

## Cómo se usan juntos

| firmware_capturadora (el chip) | scalapp (el host) |
|---|---|
| RE del firmware 8051: diezmado 120→60 (f606), hot-switch, fix120 | pide el modo exacto por uniqueID; supervisor reconecta |
| Hum del acople video→audio: par 59.5/60.5Hz batiendo | notch adaptativo que lo mide y lo mata en vivo |
| Mapa XDATA del bloque de audio (f100-f12a) | diagnóstico y validación de mitigaciones |
| Protocolo HID de fábrica + flash (backup/parche, checksum) | `--dump-modes`, pruebas de capacidades |

El flujo de trabajo: el firmware repo **mide y atribuye** en el
silicio; scalapp **convierte el hallazgo en producto**. Ejemplo
completo: `fw_re/NOTA_AUDIO_UPSTREAM.md` (thump ~1Hz — diagnóstico,
atribución al dominio UVC, experimento all-in de 96k, veredicto).

## Tests

24 tests: decoder byte-a-byte vs nokhwa, gris exacto, equivalencia
YUV/RGB, selección de modo consciente de aspecto/fps, detector del par
de hum, notch móvil sin dañar Bb1/B1, decimation, ring.

## Estado

Ver `ROADMAP.md` para la bitácora completa (fases, hallazgos de
terreno, lecciones).
