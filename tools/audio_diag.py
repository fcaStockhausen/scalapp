#!/usr/bin/env python3
"""Diagnóstico espectral de la cadena de audio de scalapp.

Analiza las grabaciones duales de SCALAPP_REC (entrada cruda del device)
y SCALAPP_REC_OUT (salida post-filtro = lo que se escucha) y reporta:

  1. Estadística básica (duración, RMS, pico) — detecta grabaciones mudas.
  2. Espectro promedio de alta resolución (ventanas de ~1.4s) con picos
     exactos por interpolación parabólica — tonos de acople, hum, batidos.
  3. Espectro de la ENVOLVENTE de banda baja (0.1-5Hz) — el "tum tum tum":
     si hay AM/batido, aparece un pico a la frecuencia del pulso.
  4. Detector de drops: discontinuidades de forma de onda (saltos de
     muestra >> ruido local) — drops de muestras del USB isócrono.
  5. Diferencial entrada→salida: qué atenuó cada etapa (notches/HPF).

Uso:
  python3 tools/audio_diag.py [/tmp/thump_in.raw] [/tmp/thump_out.raw]

Formato: f32le intercalado estéreo 48kHz (el que escribe scalapp).
"""
import sys

import numpy as np

SR = 48000
CH = 2


def load(path):
    x = np.fromfile(path, dtype="<f4")
    n = (len(x) // CH) * CH
    return x[:n].reshape(-1, CH).mean(axis=1)


def stats(name, mono):
    dur = len(mono) / SR
    rms = float(np.sqrt(np.mean(mono**2))) if len(mono) else 0.0
    peak = float(np.abs(mono).max()) if len(mono) else 0.0
    print(f"\n== {name}: {dur:.1f}s  RMS={rms:.6f} ({20*np.log10(max(rms,1e-12)):.1f} dBFS)  pico={peak:.6f}")
    if rms < 1e-7:
        print("   !! GRABACIÓN MUDA — no hay nada que analizar (¿la fuente no sonaba?)")
    return rms > 1e-7


def spectrum_peaks(mono, fmin=1.0, fmax=1000.0, window=65536, top=14):
    """Espectro promedio (Welch simple) + picos con frecuencia afinada."""
    if len(mono) < window * 2:
        window = 1 << int(np.log2(len(mono) // 2))
    win = np.hanning(window)
    acc = None
    nw = 0
    hop = window // 2
    for s in range(0, len(mono) - window, hop):
        p = np.abs(np.fft.rfft(mono[s : s + window] * win)) ** 2
        acc = p if acc is None else acc + p
        nw += 1
    acc /= nw
    f = np.fft.rfftfreq(window, 1 / SR)
    db = 10 * np.log10(acc / acc.max() + 1e-20)
    m = (f >= fmin) & (f <= fmax)
    fx, dx = f[m], db[m]
    med = np.median(dx)
    # picos locales que superen la mediana + 6dB
    peaks = []
    for i in range(1, len(dx) - 1):
        if dx[i] > dx[i - 1] and dx[i] >= dx[i + 1] and dx[i] > med + 6:
            # interpolación parabólica del pico en frecuencia
            denom = dx[i - 1] - 2 * dx[i] + dx[i + 1]
            delta = 0.5 * (dx[i - 1] - dx[i + 1]) / denom if abs(denom) > 1e-12 else 0.0
            ftrue = fx[i] + delta * (fx[1] - fx[0])
            peaks.append((ftrue, dx[i] - med))
    peaks.sort(key=lambda t: -t[1])
    return peaks[:top], med


def envelope_spectrum(mono, flo=15.0, fhi=300.0, amax=5.0):
    """AM de la banda baja: picos de la envolvente 0.1..amax Hz."""
    X = np.fft.rfft(mono)
    f = np.fft.rfftfreq(len(mono), 1 / SR)
    X[(f < flo) | (f > fhi)] = 0
    low = np.fft.irfft(X, len(mono))
    env = np.abs(low)
    E = np.fft.rfft(env - env.mean())
    fe = np.fft.rfftfreq(len(env), 1 / SR)
    Ae = np.abs(E) ** 2
    Ae /= Ae.max() + 1e-20
    m = (fe > 0.1) & (fe < amax)
    fx, ax = fe[m], 10 * np.log10(Ae[m] + 1e-20)
    idx = np.argsort(ax)[-6:][::-1]
    return [(fx[i], ax[i]) for i in idx]


def drop_detector(mono, factor=25.0):
    """Discontinuidades: |Δmuestra| >> dispersión local (drops de USB)."""
    d = np.abs(np.diff(mono))
    if len(d) == 0:
        return []
    w = 4096
    hits = []
    for s in range(0, len(d) - w, w):
        seg = d[s : s + w]
        mad = np.median(seg) + 1e-12
        bad = np.where(seg > factor * max(mad, 1e-4))[0]
        for b in bad:
            hits.append((s + b) / SR)
    # colapsar hits contiguos (<5ms) a un evento
    events = []
    for t in hits:
        if not events or t - events[-1] > 0.005:
            events.append(t)
    return events


def main():
    p_in = sys.argv[1] if len(sys.argv) > 1 else "/tmp/thump_in.raw"
    p_out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/thump_out.raw"
    report = {}
    for name, path in (("ENTRADA cruda (device)", p_in), ("SALIDA (lo que se escucha)", p_out)):
        try:
            mono = load(path)
        except Exception as e:
            print(f"\n== {name}: no se pudo leer {path}: {e}")
            continue
        if not stats(name, mono):
            report[name] = None
            continue
        peaks, med = spectrum_peaks(mono)
        print("   tonos 1-1000Hz (freq, dB sobre mediana):")
        for f, db in peaks:
            print(f"     {f:8.2f} Hz   +{db:5.1f} dB")
        am = envelope_spectrum(mono)
        if am:
            print("   AM/envolvente 0.1-5Hz (el 'tum tum'):", ", ".join(f"{f:.2f}Hz" for f, _ in am[:4]))
        ev = drop_detector(mono)
        if ev:
            ints = np.diff(ev)
            print(f"   drops: {len(ev)} eventos; intervalos medianos {np.median(ints)*1000:.1f}ms"
                  f" (p25 {np.percentile(ints,25)*1000:.1f} / p75 {np.percentile(ints,75)*1000:.1f})")
            print(f"     primeros: {[f'{t:.2f}s' for t in ev[:8]]}")
        else:
            print("   drops: ninguno detectado")
        report[name] = (peaks, am, ev)

    if report.get("ENTRADA cruda (device)") and report.get("SALIDA (lo que se escucha)"):
        pin, _, _ = report["ENTRADA cruda (device)"]
        mono_in, mono_out = load(p_in), load(p_out)
        print("\n== atenuación entrada→salida en los tonos de entrada:")
        for f, db in pin[:8]:
            # potencia de salida en un anzuelo de ±2Hz alrededor del tono
            Xo = np.abs(np.fft.rfft(mono_out))
            fo = np.fft.rfftfreq(len(mono_out), 1 / SR)
            Xi = np.abs(np.fft.rfft(mono_in))
            fi = np.fft.rfftfreq(len(mono_in), 1 / SR)
            gi = Xi[(fi > f - 2) & (fi < f + 2)].max()
            go = Xo[(fo > f - 2) & (fo < f + 2)].max()
            att = 20 * np.log10(max(go, 1e-12) / max(gi, 1e-12))
            print(f"     {f:8.2f} Hz: {att:+6.1f} dB")


if __name__ == "__main__":
    main()
