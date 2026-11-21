#!/usr/bin/env python3
"""scalapp — Suite de validación de sonido v3.

Diseñada para NO perder pulsos (lección 20/11: el Welch promediado
diluye transientes periódicos — un pulso es energía broadband que el
promedio esparce). Metodología:

  NIVELES     RMS / pico / crest (dBFS)
  TONOS       Welch Hann + prominencia sobre piso LOCAL + interpolación
              parabólica (detecta tonos escondidos, no solo los graves)
  PULSOS      ★ por cada banda estándar: energía por frame STFT →
              serie temporal → espectro 0.05-10Hz → profundidad en dB.
              Un pulso periódico aparece como pico a su tasa.
  ESPECTROGRA- STFT completa: imágenes de ridges + dump PNG opcional
  MA          (matplotlib si está disponible)
  DIFERENCIAL entrada→salida por tono

  --selftest  valida la suite contra señales sintéticas con verdad
              conocida (AM, pulso periódico, tonos, ruido) — si el
              selftest no pasa, no se puede confiar en ninguna medición.

Formato de entrada: f32le interleaved (SCALAPP_REC / SCALAPP_REC_OUT)
o cualquier .raw con --sr/--ch.
"""
import argparse
import sys

import numpy as np

BANDS = [
    ("sub-bass", 20, 60),
    ("bass", 60, 250),
    ("low-mid", 250, 500),
    ("mid", 500, 2000),
    ("high-mid", 2000, 4000),
    ("presence", 4000, 6000),
    ("brilliance", 6000, 20000),
]

# ----------------------------------------------------------------------
# carga y utilidades


def load_raw(path, ch):
    x = np.fromfile(path, dtype=np.dtype("<f4"))
    n = (len(x) // ch) * ch
    return x[:n].reshape(-1, ch).mean(axis=1)


def welch(x, fs, nfft=0):
    """PSD Welch con Hann. nfft=0 → máxima resolución posible (la lección
    del selftest: nfft fijo de 32k metía 59.7 y 60.3 en el MISMO bin —
    resolución 1.46Hz; con datos largos hay que usar FFTs largas)."""
    if nfft <= 0:
        nfft = 1 << int(np.log2(max(64, len(x) // 2)))
        nfft = min(nfft, 1 << 19)
    if len(x) < nfft * 2:
        nfft = 1 << int(np.log2(max(64, len(x) // 2)))
    win = np.hanning(nfft)
    acc, nw = None, 0
    hop = nfft // 2
    for s in range(0, len(x) - nfft, hop):
        p = np.abs(np.fft.rfft(x[s : s + nfft] * win)) ** 2
        acc = p if acc is None else acc + p
        nw += 1
    psd = acc / nw / (win.sum() ** 2 / nfft) / nfft
    f = np.fft.rfftfreq(nfft, 1 / fs)
    return f, 10 * np.log10(psd + 1e-24)


def local_median(v, frac=0.25):
    """Piso local: mediana por BLOQUES (el enfoque por-bin era O(n·w) y
    con FFTs de 2^19 tardaba minutos — el selftest lo destapó).
    Bloques no solapados de n·frac/2, repetidos: suficiente suavidad
    para prominencia de tonos."""
    w = max(16, int(len(v) * frac / 2))
    nblocks = (len(v) + w - 1) // w
    out = np.empty_like(v)
    for b in range(nblocks):
        lo, hi = b * w, min(len(v), (b + 1) * w)
        out[lo:hi] = np.median(v[lo:hi])
    return out


def find_tones(f, psd_db, min_prom=6.0):
    prom = psd_db - local_median(psd_db)
    idx = np.where(
        (psd_db[1:-1] > psd_db[:-2]) & (psd_db[1:-1] >= psd_db[2:]) & (prom[1:-1] >= min_prom)
    )[0] + 1
    out = []
    df = f[1] - f[0]
    for i in idx:
        y0, y1, y2 = psd_db[i - 1], psd_db[i], psd_db[i + 1]
        den = y0 - 2 * y1 + y2
        delta = 0.5 * (y0 - y2) / den if abs(den) > 1e-12 else 0.0
        out.append((f[i] + delta * df, psd_db[i], prom[i]))
    out.sort(key=lambda t: -t[2])
    return out


# ----------------------------------------------------------------------
# detector de pulsos: STFT → energía por banda → espectro de la envolvente


def stft_frames(x, fs, nfft=4096, hop=1024):
    win = np.hanning(nfft)
    n = 1 + (len(x) - nfft) // hop
    if n < 4:
        return None, None, None
    out = np.empty((n, nfft // 2 + 1))
    for k in range(n):
        seg = x[k * hop : k * hop + nfft] * win
        out[k] = np.abs(np.fft.rfft(seg)) ** 2
    f = np.fft.rfftfreq(nfft, 1 / fs)
    return out, f, fs / hop  # mag2[time, freq], freqs, frame_rate


def pulse_spectrum(x, fs, fmin=0.05, fmax=10.0):
    """Por banda estándar: serie de energía (dB) → picos de modulación.

    Devuelve {banda: [(rate_hz, depth_db, confianza), ...]} — la
    profundidad es semiamplitud del ripple en dB; confianza = pico /
    mediana del espectro de envolvente.
    """
    mag2, fbin, fr = stft_frames(x, fs)
    if mag2 is None:
        return {}
    res = {}
    for name, lo, hi in BANDS:
        m = (fbin >= lo) & (fbin < hi)
        if not m.any():
            continue
        energy = 10 * np.log10(mag2[:, m].sum(axis=1) + 1e-24)
        energy = energy - np.mean(energy)
        E = np.abs(np.fft.rfft(energy)) / len(energy) * 2
        fe = np.fft.rfftfreq(len(energy), 1 / fr)
        sel = (fe > fmin) & (fe < fmax)
        if not sel.any():
            continue
        fes, amps = fe[sel], E[sel]
        med = np.median(amps) + 1e-12
        order = np.argsort(amps)[::-1][:3]
        hits = []
        for k in order:
            # umbrales calibrados por selftest: el ruido puro da conf~3.1
            # por azar; señal real da conf 9-27. conf>5 separa limpio.
            if amps[k] < 0.3 or amps[k] / med < 5.0:
                continue
            hits.append((fes[k], amps[k], amps[k] / med))
        res[name] = hits
    return res


# ----------------------------------------------------------------------
# reporte


def report(path, fs, ch, label, show_spectrogram=False):
    print(f"\n{'=' * 66}\n{label}: {path}\n{'=' * 66}")
    x = load_raw(path, ch)
    dur = len(x) / fs
    rms = np.sqrt(np.mean(x**2))
    peak = np.abs(x).max()
    if rms < 1e-7:
        print(f"  {dur:.1f}s — SILENCIO DIGITAL (RMS={rms:.8f}) — nada que analizar")
        return None
    print(
        f"  {dur:.1f}s | RMS {20*np.log10(rms):+.2f} dBFS | pico {20*np.log10(peak):+.2f}"
        f" | crest {20*np.log10(peak/rms):+.1f} dB"
    )
    f, psd = welch(x, fs)
    print("\n  TONOS (prominencia ≥6dB sobre piso local):")
    tt = find_tones(f, psd)
    for fr, db, pr in tt[:14]:
        band = next((n for n, lo, hi in BANDS if lo <= fr < hi), "?")
        print(f"    {fr:9.2f} Hz  {db:+7.1f} dB  +{pr:4.1f}dB piso  [{band}]")
    print("\n  PULSOS (energía por banda modulada, 0.05-10Hz):")
    pulses = pulse_spectrum(x, fs)
    any_hit = False
    for name, hits in pulses.items():
        for rate, depth, conf in hits[:1]:
            if depth >= 0.3 and conf > 5:
                any_hit = True
                print(
                    f"    {name:<11}: {rate:.3f} Hz (período {1/rate:5.2f}s) "
                    f"profundidad {depth:.2f} dB  conf {conf:.0f}x"
                )
    if not any_hit:
        print("    — sin modulación periódica significativa —")
    if show_spectrogram:
        try:
            import matplotlib

            matplotlib.use("Agg")
            import matplotlib.pyplot as plt

            mag2, fbin, fr = stft_frames(x, fs)
            if mag2 is not None:
                plt.figure(figsize=(14, 6))
                plt.imshow(
                    10 * np.log10(mag2.T + 1e-24),
                    origin="lower",
                    aspect="auto",
                    extent=[0, dur, 0, fs / 2],
                    vmin=10 * np.log10(mag2.max()) - 90,
                    vmax=10 * np.log10(mag2.max()),
                )
                plt.ylim(0, min(fs / 2, 2000))
                plt.xlabel("s")
                plt.ylabel("Hz")
                plt.colorbar(label="dB")
                out_png = path + ".spectrogram.png"
                plt.savefig(out_png, dpi=110, bbox_inches="tight")
                print(f"\n  espectrograma → {out_png}")
        except ImportError:
            print("  (matplotlib no disponible: sin PNG)")
    return f, psd


def differential(f1, psd1, f2, psd2):
    print("\n  DIFERENCIAL entrada→salida (tonos de entrada):")
    df = f2[1] - f2[0]
    for fr, db, pr in find_tones(f1, psd1)[:12]:
        i2 = int(round(fr / df))
        near = psd2[max(0, i2 - 8) : i2 + 9].max() if i2 + 9 < len(psd2) else -200
        att = near - db
        flag = "MATADO" if att < -15 else ("parcial" if att < -6 else "SOBREVIVE")
        print(f"    {fr:8.2f}Hz ({pr:+4.1f}dB piso): {att:+6.1f} dB  {flag}")


# ----------------------------------------------------------------------
# SELFTEST: la suite debe detectar lo que SABEMOS que está


def selftest():
    fs = 48000
    t = np.arange(int(fs * 20)) / fs  # 20s
    rng = np.random.default_rng(42)
    ok = True

    # 1) tonos escondidos: 59.7/60.3 a +30dB sobre ruido
    x = rng.normal(0, 0.003, len(t)) + 0.09 * np.sin(2 * np.pi * 59.7 * t) + 0.09 * np.sin(
        2 * np.pi * 60.3 * t
    )
    f, psd = welch(x, fs)
    tones = find_tones(f, psd)
    hits = [fr for fr, _, _ in tones if abs(fr - 59.7) < 0.05 or abs(fr - 60.3) < 0.05]
    print(f"  [1] tonos 59.7/60.3 escondidos: detectados {hits}")
    ok &= len(hits) == 2

    # 2) AM de ganancia 1.7dB a 0.617Hz sobre tono de 440Hz
    d = 10 ** (1.7 / 20) - 1
    x = 0.2 * (1 + d * np.sin(2 * np.pi * 0.617 * t)) * np.sin(2 * np.pi * 440 * t)
    pulses = pulse_spectrum(x, fs, fmin=0.2, fmax=2.0)
    found = False
    for name, hp in pulses.items():
        for rate, depth, conf in hp:
            if abs(rate - 0.617) < 0.05 and depth > 1.0 and conf > 3:
                found = True
                print(
                    f"  [2] AM 1.7dB@0.617Hz: {rate:.3f}Hz prof {depth:.2f}dB conf {conf:.0f}x [{name}] ✓"
                )
    ok &= found

    # 3) pulso periódico broadband: ráfagas de 15ms cada 1.7s (~0.59Hz)
    x = rng.normal(0, 0.0005, len(t))
    burst = int(0.015 * fs)
    period = int(1.7 * fs)
    for k in range(0, len(t) - burst, period):
        x[k : k + burst] += rng.normal(0, 0.25, burst)
    pulses = pulse_spectrum(x, fs, fmin=0.2, fmax=2.0)
    found = False
    for name, hp in pulses.items():
        for rate, depth, conf in hp:
            if abs(rate - 0.588) < 0.05 and conf > 3:
                found = True
                print(
                    f"  [3] ráfagas cada 1.7s: {rate:.3f}Hz prof {depth:.1f}dB conf {conf:.0f}x [{name}] ✓"
                )
    ok &= found

    # 4) ruido puro: sin falsos positivos (prof <0.3dB o conf <3)
    x = rng.normal(0, 0.01, len(t))
    pulses = pulse_spectrum(x, fs, fmin=0.05, fmax=10)
    false = [
        (n, r, de, c)
        for n, hp in pulses.items()
        for r, de, c in hp
        if de >= 0.3 and c > 3
    ]
    print(f"  [4] ruido puro sin falsos: {len(false)} falsos {false[:2] if false else ''}")
    ok &= len(false) == 0

    print(f"\n  SELFTEST: {'PASA' if ok else 'FALLA'}")
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("infile", nargs="?")
    ap.add_argument("outfile", nargs="?")
    ap.add_argument("--sr", type=int, default=48000)
    ap.add_argument("--ch", type=int, default=2)
    ap.add_argument("--spectrogram", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()
    if args.selftest:
        return selftest()
    if not args.infile:
        ap.print_help()
        return 1
    r1 = report(args.infile, args.sr, args.ch, "ENTRADA", args.spectrogram)
    if args.outfile:
        r2 = report(args.outfile, args.sr, args.ch, "SALIDA", args.spectrogram)
        if r1 and r2:
            differential(r1[0], r1[1], r2[0], r2[1])
    return 0


if __name__ == "__main__":
    sys.exit(main())
