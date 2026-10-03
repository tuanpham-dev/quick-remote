#!/usr/bin/env python3
"""SSIM of the luma planes of a dumped client frame and a reference.

usage: ssim.py DUMP (with DUMP.json) REF.i420 WIDTH HEIGHT"""
import json, sys
import numpy as np

dump, ref, w, h = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
meta = json.load(open(dump.rsplit(".", 1)[0] + ".json"))
raw = np.fromfile(dump, dtype=np.uint8)
stride, off = meta["strides"][0], meta["offsets"][0]
a = raw[off:off + stride * meta["height"]].reshape(meta["height"], stride)[:h, :w].astype(np.float64)
b = np.fromfile(ref, dtype=np.uint8)[: w * h].reshape(h, w).astype(np.float64)

def box(x, k=8):
    c = np.cumsum(np.cumsum(np.pad(x, ((1, 0), (1, 0))), 0), 1)
    return (c[k:, k:] - c[:-k, k:] - c[k:, :-k] + c[:-k, :-k]) / (k * k)

C1, C2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
ma, mb = box(a), box(b)
va, vb, cov = box(a * a) - ma * ma, box(b * b) - mb * mb, box(a * b) - ma * mb
s = ((2 * ma * mb + C1) * (2 * cov + C2)) / ((ma ** 2 + mb ** 2 + C1) * (va + vb + C2))
print(f"{s.mean():.4f}")
