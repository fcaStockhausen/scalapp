# Additional Latency Optimization Strategies

## Current Status
- **Latency actual**: ~20-30ms (audio-only mode)
- **Optimizaciones aplicadas**: Block size pequeño, buffer agresivo, latency='low'

## Estrategias Adicionales (por impacto de latencia)

### 1. Aumentar Sample Rate (48kHz) - ⭐⭐⭐⭐⭐
**Impacto**: Puede reducir ~5-10ms adicional

**Cómo funciona**: Con 48kHz en lugar de 44.1kHz, un block de 128 samples representa menos tiempo:
- 44.1kHz: 128 samples = 2.9ms
- 48kHz: 128 samples = 2.67ms

**Implementación**:
```bash
python main.py --audio-only --audio-rate 48000
```

**Pros**: Fácil de implementar, ya soportado
**Cons**: Más CPU, pero insignificante en M2

---

### 2. Core Audio Directo - ⭐⭐⭐⭐⭐
**Impacto**: Puede reducir ~10-20ms adicional

**Cómo funciona**: sounddevice usa PortAudio como wrapper, que añade overhead. Core Audio directamente elimina una capa.

**Implementación requerida**:
```python
import coreaudio  # o usar PyObjC con CoreAudio
# Crear AudioUnit directamente
# Configurar kAudioDevicePropertyBufferFrameSize = 64
# Configurar kAudioDevicePropertyIOCycleUsage para baja latencia
```

**Ventajas específicas de macOS**:
- Hardware-level buffer control
- No PortAudio overhead
- Permite buffers de 32 samples (~0.67ms en 48kHz)
- Better scheduling del audio thread

**Pros**: Máxima reducción de latencia posible en macOS
**Cons**: Requiere reescribir audio_capture.py completamente, más complejo

---

### 3. Direct I/O Passthrough - ⭐⭐⭐⭐
**Impacto**: Puede reducir ~5-10ms

**Cómo funciona**: Conectar directamente input→output del dispositivo de audio sin procesamiento Python.

**Implementación**:
- Si el dispositivo soporta Input+Output en el mismo device
- Usar sounddevice con Stream de duplex (input+output en mismo callback)
- Pasar datos directamente sin guardar en buffer

**Requerimientos**:
- Dispositivo debe tener I/O duplex
- Capturadora HDMI debe tener output loopback

**Pros**: Elimina buffer circular completamente
**Cons**: Solo funciona con ciertos dispositivos

---

### 4. Reduce Channels (Mono) - ⭐⭐
**Impacto**: ~2-5ms

**Cómo funciona**: Procesar 1 canal en lugar de 2 reduce la cantidad de datos.

**Implementación**:
```bash
python main.py --audio-only --audio-channels 1
```

**Pros**: Menos datos = menos latencia de procesamiento
**Cons**: Pierdes estéreo

---

### 5. Format Optimization - ⭐⭐
**Impacto**: ~1-3ms

**Cómo funciona**: Usar formatos más eficientes para conversiones.

**Implementación**:
- Usar int16 en lugar de float32 (menos conversión)
- O usar float32 native del dispositivo

**Pros**: Menos overhead de conversión
**Cons**: Menos dinámica en audio (int16)

---

### 6. Thread Priority - ⭐⭐⭐
**Impacto**: ~2-5ms

**Cómo funciona**: Asignar prioridad alta al thread de audio.

**Implementación en macOS**:
```python
import ctypes
import ctypes.util

libSystem = ctypes.CDLL(ctypes.util.find_library('System'))
# Set thread policy to real-time
libSystem.thread_policy_set(...)
```

**Pros**: El sistema operativo prioriza el audio thread
**Cons**: Puede causar glitches en otros procesos

---

### 7. Memory Lock - ⭐⭐
**Impacto**: ~1-2ms

**Cómo funciona**: Evitar que la memoria del audio sea swappeada.

**Implementación**:
```python
import mlock
# Lock audio buffers in physical memory
mlock.mlock(audio_buffer, buffer_size)
```

**Pros**: Garantiza que el audio siempre está en RAM
**Cons**: Necesita permisos, no portable

---

### 8. Poll-Free Operation - ⭐⭐
**Impacto**: ~1-2ms

**Cómo funciona**: Eliminar cualquier polling en el audio path.

**Implementación**:
- Todo en callbacks (ya lo hacemos)
- No locks en el audio path crítico
- Usar ring buffers lock-free

**Pros**: Elimina overhead de locks
**Cons**: Más complejo de implementar correctamente

---

### 9. Disable Volume Control - ⭐
**Impacto**: ~1ms

**Cómo funciona**: Eliminar el multiplicador de volumen en el audio path.

**Implementación**: Bypass de `adjusted_data = data * self.volume_multiplier`

**Pros**: Menos operaciones por sample
**Cons**: Pierdes control de volumen

---

### 10. Exclusive Mode - ⭐⭐⭐
**Impacto**: ~3-5ms

**Cómo funciona**: Usar el dispositivo de audio en modo exclusivo (sin mezcla del OS).

**Implementación**:
```python
# sounddevice puede soportar esto
# o usar Core Audio con kAudioDevicePropertyHogMode
```

**Pros**: Sin mezcla del OS, latencia mínima
**Cons**: Bloquea el dispositivo para otras apps

---

## Recomendación de Implementación

### Fase 1: Fácil (ya disponible)
1. **48kHz sample rate** - `--audio-rate 48000`
   - Impacto: ~5-10ms reducción
   - Riesgo: Cero
   - **PROBAR AHORA**

### Fase 2: Media
2. **Thread priority** - ~2-5ms
   - Riesgo: Bajo
   - Dificultad: Media

3. **Exclusive mode** - ~3-5ms
   - Riesgo: Bajo-Medio (bloquea otros)
   - Dificultad: Media

### Fase 3: Avanzado
4. **Core Audio directo** - ~10-20ms
   - Riesgo: Medio (cambio grande)
   - Dificultad: Alta
   - **MÁXIMA REDUCCIÓN POSIBLE**

5. **Direct I/O passthrough** - ~5-10ms
   - Riesgo: Medio (depende del dispositivo)
   - Dificultad: Media-Alta

---

## Comando Recomendado AHORA MISMO

```bash
# Prueba esto primero - fácil y efectivo
python main.py --audio-only --audio-rate 48000

# Si funciona bien, combina con ultra-low
python main.py --audio-only --ultra-low-latency --audio-rate 48000
```

Esto debería bajar de ~20-30ms a **~12-20ms total** sin riesgo.

---

## Estimación de Latencia Final con Todas las Optimizaciones

| Modo | Latencia Actual | +48kHz | +Core Audio | +Direct I/O | Total Estimado |
|------|----------------|---------|-------------|-------------|-----------------|
| Audio-only | 20-30ms | 15-25ms | 5-15ms | 0-10ms | **~0-10ms** |

Con Core Audio directo, podríamos lograr **<10ms total** en óptimas condiciones.

---

## ¿Quieres que implemente alguna de estas?

1. **Implementar 48kHz support** (fácil, probar ahora)
2. **Implementar thread priority** (medio)
3. **Implementar Core Audio directo** (difícil, máximo impacto)
4. **Implementar Direct I/O passthrough** (medio-difícil)

¿Por cuál empezamos?
