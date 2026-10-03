#!/usr/bin/env python3
"""Max normalized cross-correlation of two WAVs (first channel), best lag.

usage: wavcorr.py A.wav B.wav  →  prints "corr lag_ms"."""
import sys, wave
import numpy as np

def load(p):
    w = wave.open(p)
    n, ch, rate = w.getnframes(), w.getnchannels(), w.getframerate()
    a = np.frombuffer(w.readframes(n), dtype=np.int16).reshape(-1, ch)[:, 0].astype(np.float64)
    return a, rate

a, ra = load(sys.argv[1])
b, rb = load(sys.argv[2])
assert ra == rb, "sample rates differ"
# Compare a 1 s window from the middle of A against all of B.
seg = a[len(a) // 3: len(a) // 3 + ra]
seg = (seg - seg.mean()) / (seg.std() + 1e-9)
best, lag = 0.0, 0
step = len(seg)
for i in range(0, max(1, len(b) - step), 48):
    w = b[i:i + step]
    if len(w) < step:
        break
    w = (w - w.mean()) / (w.std() + 1e-9)
    c = float(np.dot(seg, w) / step)
    if c > best:
        best, lag = c, i
print(f"{best:.3f} {(lag - len(a) // 3) / ra * 1000:.1f}")
