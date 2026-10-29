#!/usr/bin/env python3
"""test_aislamiento_audio — mide cortes de audio objetivamente.

Grabación A: solo audio (45s)  → si hay cortes: el problema es USB/dispositivo
Grabación B: audio + video cv2 en paralelo → si B > A: contención en la app
Métrica: ventanas RMS de 20ms; ventana "silenciosa" entre contenido = corte.
"""
import sounddevice as sd
import numpy as np
import threading
import time
import sys

SR = 44100
DUR = 45

def find_hdmi_audio():
    for i, d in enumerate(sd.query_devices()):
        if d['max_input_channels'] > 0 and ('USB' in d['name'] or 'MS213' in d['name']
                                            or 'Digital Audio' in d['name']):
            return i
    return None

def analyze(name: str, data: np.ndarray):
    """Cuenta dropouts: ventanas silenciosas rodeadas de contenido."""
    mono = data.mean(axis=1)
    win = int(0.020 * SR)  # 20ms
    n = len(mono) // win
    rms = np.sqrt((mono[:n*win].reshape(n, win) ** 2).mean(axis=1))
    thr = 0.002
    quiet = rms < thr
    # dropouts = rachas de ventanas silenciosas con contenido antes y después
    dropouts, run = 0, 0
    started = False
    for q in quiet:
        if q:
            run += 1
        else:
            if run > 0 and started:
                dropouts += 1
            run = 0
            started = True
    total_silence = quiet.sum() * 0.020
    print(f"[{name}] dropouts={dropouts}  tiempo-silencio={total_silence:.2f}s de {len(mono)/SR:.0f}s")
    return dropouts

def record(dev, name, with_video):
    frames = []
    ev = threading.Event()
    def cb(indata, n, t, st):
        if st:
            print(f"[{name}] status: {st}")
        frames.append(indata.copy())
    stream = sd.InputStream(samplerate=SR, channels=2, device=dev,
                            blocksize=256, callback=cb, latency='low')
    stream.start()
    grab = None
    if with_video:
        import cv2
        grab = cv2.VideoCapture(0)
        grab.set(cv2.CAP_PROP_FRAME_WIDTH, 1920)
        grab.set(cv2.CAP_PROP_FRAME_HEIGHT, 1080)
        n_frames = [0]
        def vloop():
            while not ev.is_set():
                grab.read()  # consume frames continuamente
                n_frames[0] += 1
        vt = threading.Thread(target=vloop); vt.start()
    print(f"[{name}] grabando {DUR}s...")
    time.sleep(DUR)
    ev.set()
    stream.stop(); stream.close()
    if grab:
        grab.release()
        print(f"[{name}] video frames consumidos: {n_frames[0]}")
    data = np.concatenate(frames)
    return analyze(name, data)

if __name__ == "__main__":
    dev = find_hdmi_audio()
    print(f"dispositivo HDMI audio: {dev} ({sd.query_devices(dev)['name']})")
    if dev is None:
        sys.exit("no encontrado")
    solo = record(dev, "A-solo-audio", with_video=False)
    print("--- descansa 5s ---")
    time.sleep(5)
    convid = record(dev, "B-audio+video", with_video=True)
    print(f"\nVEREDICTO: solo={solo}  con-video={convid}")
    if solo == 0 and convid > 0:
        print("→ CORTES POR CONTENCION EN LA APP (GIL/CPU del proceso con video)")
    elif solo > 0:
        print("→ CORTES EN EL CAMINO USB/DISPOSITIVO (audio llega cortado)")
    elif solo == 0 and convid == 0:
        print("→ sin cortes en 45s — el problema es intermitente: correr más tiempo")
