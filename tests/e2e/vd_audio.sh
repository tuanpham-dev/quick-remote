# Audio with the virtual display: apps there play into its private sink and
# record from its private microphone.
#   A17-vd: a click track played by an app (PulseAudio API, through
#           pipewire-pulse) reaches the client; host capture → client
#           playout ≤ 40 ms.
#   A18-vd: a WAV fed into the client's microphone is what an app in the
#           display records (correlation ≥ 0.9).
# Needs the render group: sg render -c "tests/e2e/run.sh vd_audio lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A17-vd "the GPU render node is not accessible (needs the render group)"
	return 0
fi
# The microphone input: 6 s of repeating chirps (200 Hz → 3 kHz, 0.7 s
# each, with gaps), which Opus keeps and which correlate at one lag only.
python3 - "$LOGDIR/mic.wav" <<'PY'
import sys, wave
import numpy as np
r = 48000
t = np.arange(int(0.7 * r)) / r
f0, f1 = 200.0, 3000.0
chirp = np.sin(2 * np.pi * (f0 * t + (f1 - f0) / (2 * 0.7) * t * t)) * np.hanning(len(t))
x = np.zeros(6 * r)
for k, start in enumerate(range(0, 6 * r - len(t), int(0.9 * r))):
    x[start:start + len(t)] = chirp * (0.4 + 0.1 * (k % 3))
x = (x * 0.6 * 32767).astype(np.int16)
w = wave.open(sys.argv[1], "wb")
w.setnchannels(1); w.setsampwidth(2); w.setframerate(r); w.writeframes(x.tobytes())
PY
rec="$LOGDIR/rec.raw"
desk="sh -c \"weston-simple-egl & gst-launch-1.0 -q audiotestsrc wave=ticks is-live=true ! pulsesink & exec gst-launch-1.0 -q pulsesrc ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,channels=1,rate=48000 ! filesink location=$rec\""
start_host host vahost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --size 1280x720 --fps 60 --input none
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec \
	--audio-out "file:$LOGDIR/out.wav" --mic "file:$LOGDIR/mic.wav" --duration 10 \
	--stats-json "$LOGDIR/vd_audio.json" >"$LOGDIR/vd_audio-client.txt" 2>&1
sleep 0.5
python3 - "$LOGDIR" >"$LOGDIR/vd_audio-summary.txt" <<'PY'
import json, sys, wave
import numpy as np
d = sys.argv[1]
# What the client played: the click track, so it must have energy.
w = wave.open(f"{d}/out.wav")
out = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(float)
rms = float(np.sqrt((out ** 2).mean())) if len(out) else 0.0
rows = [json.loads(l) for l in open(f"{d}/vd_audio.json")][3:]
lat = [r["audio_ms"] for r in rows if r["audio_ms"] > 0]
# A/V offset: both latencies run from the host's capture timestamps on the
# synced clock, so their difference is how far the picture trails the sound.
vid = [r["total_ms"] for r in rows if r["total_ms"] > 0]
# What the app recorded against what the client's microphone sent:
# normalized cross-correlation of 2 s of the input at every lag (FFT).
mic = wave.open(f"{d}/mic.wav")
m = np.frombuffer(mic.readframes(mic.getnframes()), dtype=np.int16).astype(float)
r = np.fromfile(f"{d}/rec.raw", dtype=np.int16).astype(float)
seg = m[48000:3 * 48000]
best = 0.0
if len(r) > len(seg):
    n = 1 << int(np.ceil(np.log2(len(r) + len(seg))))
    xc = np.fft.irfft(np.fft.rfft(r, n) * np.conj(np.fft.rfft(seg, n)), n)[: len(r) - len(seg)]
    c2 = np.cumsum(np.concatenate([[0.0], r * r]))
    energy = np.sqrt(np.maximum(c2[len(seg):len(seg) + len(xc)] - c2[: len(xc)], 1e-9))
    best = float(np.max(xc / (energy * np.linalg.norm(seg))))
av = (np.median(vid) - np.median(lat)) if (vid and lat) else 999
print(f"out_rms={rms:.0f} audio_ms={np.median(lat) if lat else -1:.1f} video_ms={np.median(vid) if vid else -1:.1f} av_ms={av:.1f} rec_s={len(r)/48000:.1f} corr={best:.3f}")
PY
cat "$LOGDIR/vd_audio-summary.txt"
v() { sed -E "s/.*[ ^]$1=([-0-9.]+).*/\1/; s/^$1=([-0-9.]+).*/\1/" "$LOGDIR/vd_audio-summary.txt"; }
check A17-vd "an app's click track reaches the client (rms $(v out_rms)), host capture → playout $(v audio_ms) ms (≤ 40)" \
	python3 -c "import sys; sys.exit(0 if $(v out_rms) > 200 and 0 < $(v audio_ms) <= 40 else 1)"
check A17b-vd "A/V offset: the picture trails the sound by $(v av_ms) ms (video $(v video_ms) ms, audio $(v audio_ms) ms from capture; within ±40)" \
	python3 -c "import sys; sys.exit(0 if abs($(v av_ms)) <= 40 else 1)"
check A18-vd "the client's microphone WAV is what an app in the display records (correlation $(v corr), ≥ 0.9)" \
	python3 -c "import sys; sys.exit(0 if $(v corr) >= 0.9 else 1)"
