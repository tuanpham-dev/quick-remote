# Testing QuickRemote by hand

Everything here is in `dist/quickremote-test/` (built by `scripts/package.sh`):

| File | What |
|---|---|
| `qr-host` | The host: shares this machine's screen (the web client is built in) |
| `qr-client` | The desktop client for Linux |
| `qr-relay` | The rendezvous and relay server, for access over the internet |
| `quickremote.apk` | The Android client |
| `quickremote-host.desktop`, `systemd/` | KDE permission file and service units (optional) |

The Linux binaries are built on Debian 13 and need its GStreamer libraries.
Another Debian 13 machine works as a client. Other distributions need their
own build (`cargo build --release -p qr-client`).

## 1. One-time setup on the host machine

```
sudo scripts/setup-root.sh $USER     # packages, groups, uinput, UDP buffers
```

Log out and back in afterwards. The `render` group is the one that matters most:
without it the host can't use the GPU encoder and falls back to x264 on the CPU,
which adds latency. `input` is needed for gamepads and pen. If you only want
the groups:

```
sudo usermod -aG render,input $USER   # then log out and back in
```

After logging back in, clear GStreamer's plugin cache once. It still holds the
scan from when the GPU wasn't accessible, and keeps hiding the GPU encoder
until cleared:

```
rm -rf ~/.cache/gstreamer-1.0
```

Check that the GPU encoder is found:

```
./qr-host --probe        # expect a line like: vah264lpenc  h264  hw
```

Open the firewall if there is one: UDP 9450 (host), and TCP and UDP 9443 if you
use the web client.

## 2. Start the host

In your desktop session (GNOME, KDE, sway/Hyprland or X11; detected):

```
./qr-host
```

It prints the host's name, `HOST-ID`, `LISTEN` and a 6-digit `PIN`. The PIN is
only needed the first time a client pairs. Leave the terminal open.

Variants worth trying:

- A separate virtual desktop (headless sway on the GPU), the best path for
  games and 3D, and 120 fps:
  `./qr-host --source vdisplay --size 1920x1080 --fps 120`
  (`--source "vdisplay:<command>"` starts a program in it, for example a game or a
  terminal; with nothing given you get an empty sway desktop.)
- A full XFCE desktop on that virtual display, which follows the client's
  window size and pixel ratio:
  `./qr-host --source "vdisplay:$PWD/xfce-desktop.sh"`.
  Chrome started in it uses Vulkan, which gives WebGL on the GPU.
- Hardware H.265 on Intel GPUs (sharper at the same bitrate):
  `sudo ./setup-intel-hevc.sh` installs Intel's full media driver from
  Debian's non-free component. Older GPUs, like UHD 630, encode at most
  4096 pixels wide; larger requests are cut to that.
- With the web client: `./qr-host --web-listen 0.0.0.0:9443`. It prints a
  `WEB-URL` to open in the browser.
- On KDE, the first start writes `~/.local/share/applications/quickremote-host.desktop`
  so KWin lets the host capture the screen. Nothing to click.

## 3. Connect a client

### Desktop client (Linux)

```
./qr-client                                  # window: hosts on the LAN, pick one
./qr-client --connect 192.168.1.20:9450      # or straight to an address
```

It asks for the PIN the first time. Hotkeys are **Ctrl+Alt+Shift** plus:

| Key | Does |
|---|---|
| M | lock the mouse in the window (games); again to release |
| F | fullscreen |
| S | stats overlay (fps, bitrate, latency split) |
| T | sharp-text mode (4:4:4, for reading and code) |
| V | Display menu (resolution, pixel ratio, monitor, view) |
| + / − / 0 | zoom in, zoom out, back to 100% (Ctrl+wheel zooms too) |
| N | next host monitor in this window |
| W | a second window on another monitor (again: close it) |
| Q | disconnect |

Resizing the window resizes the remote desktop 300 ms after you stop (on the
virtual display, KDE and GNOME; X11 desktops keep their size and the picture is
fitted). The Display menu switches to a fixed resolution, sets the pixel ratio
(the desktop's UI scale follows it), picks a virtual monitor of the client's own
or the real monitor on KDE/GNOME, and sets the view: **Fit** (shrinks, never
enlarges), **Fill window** or **100%**. When zoomed in, the picture pans as the
pointer nears an edge. Settings are kept in `display.json` next to the client's
keys. A dropped connection reconnects by itself (after 1, 2, 4, 8, then every
10 seconds).

Drop a file on the window to send it to the host (it lands in
`~/Downloads/QuickRemote`). Files put in `~/QuickRemote/Outbox` on the host come
to the client's `~/Downloads`. The clipboard (text and images) syncs both ways.

### Web client

Open the `WEB-URL` from the host in Chrome or Edge (WebTransport; Firefox is
not supported). The page certificate is self-signed: accept the warning once.
Enter the PIN, then click **Start audio**. Hotkeys are Ctrl+Alt+Shift with M, S,
F and Q, as above.

- **Display** (bottom bar): Follow window or a fixed resolution (presets or
  custom, with refresh rate), pixel ratio, Virtual or Real monitor, and Fit /
  Fill window / 100%. Kept per browser.
- **Zoom:** the − / + buttons, Ctrl+wheel, or pinch; drag with two fingers to pan.
- **Fullscreen** (F or the button) captures browser shortcuts such as Ctrl+T;
  hold **Escape** for a second to leave it.
- **Touch screens:** tap = click, long press or two-finger tap = right click,
  three-finger tap = middle click, two-finger drag = scroll, four-finger tap
  or ⌨ = keyboard, with a strip of Esc, Tab, arrows and sticky Ctrl/Alt/Super.
- The page pauses the video while the tab is hidden, and reconnects by itself
  when the connection drops.

### Android

```
adb install quickremote.apk
```

Or copy the APK to the phone and open it. Hosts on the same Wi-Fi are listed;
otherwise type `address:9450`. It asks for the PIN the first time.

- **One finger:** a trackpad (move the pointer), or direct touch (Display → Touch). A tap is a click.
- **Long press** or **two-finger tap:** right click. **Three-finger tap:** middle click.
- **Two-finger drag:** scroll (pans when zoomed in). **Pinch:** zoom.
- **Four-finger tap** or the **⌨** button: keyboard, with a strip of Esc, Tab,
  arrows, Home/End/PgUp/PgDn and sticky Ctrl/Alt/Super (tap Ctrl, then a key).
- **Display** button: follow the screen (rotating the phone reshapes the remote
  desktop) or a fixed resolution, pixel ratio, monitor, view mode, touch mode.
- **Zoom %** button: back to 100%.
- **Back:** hides the keyboard, else disconnects.
- **Ctrl+Alt+F1** on a hardware keyboard shows stats; **Ctrl+Alt+F2** switches touch mode.
- In the background the host stops sending video; a dropped connection reconnects by itself.
- **Gamepads and a stylus** work when connected to the phone.

## 4. Over the internet (optional)

On a server with a public IP, open UDP 4500 and 40000–40999, then run:

```
./qr-relay --public-ip <server-ip>
```

On the host, run `./qr-host --rendezvous <server-ip>:4500`. On the client, run
`./qr-client --connect <HOST-ID> --rendezvous <server-ip>:4500`, with the
`HOST-ID` the host printed. The connection is direct when hole punching works
and goes through the relay otherwise.

## 5. What to try

- **Responsiveness:** move windows and type in an editor, with the stats overlay on (S).
  On a wired LAN the latency should be around 10 ms.
- **A game:** on the virtual display (`--source vdisplay --fps 120`), with the mouse locked
  (M) and a gamepad.
- **Video and audio:** play a video and check sound and lip sync.
- **Sharp text:** reading small text with T on and off.
- **Clipboard:** copy and paste both ways, text and a screenshot.
- **Files:** drag a large file onto the window, and use the Outbox in the other direction.
- **Multi-monitor:** N and W, if the host has more than one screen.
- **Resize and zoom:** resize the client window (or rotate the phone) and watch
  the remote desktop follow; try a fixed resolution, 2× pixel ratio, Fill and 100%,
  and zoom in and pan.
- **Monitor changes:** change the host's resolution or rotate a screen while
  connected. The picture should come back within about 2 seconds.
- **Weak networks:** Wi-Fi, or a phone hotspot (the bitrate should adapt).

## 6. If something goes wrong

Run with logs and send me the output:

```
RUST_LOG=debug ./qr-host 2>&1 | tee host.log
RUST_LOG=debug ./qr-client 2>&1 | tee client.log
```

On Android, `adb logcat -s QuickRemote` shows the app's logs. Note also what the
stats overlay showed (fps, latency).

## Known limits

- **Text outside the keyboard layout:** on KDE, characters the host's keyboard layout lacks
  (é on a US layout, CJK) are skipped. On GNOME they're entered by Unicode code point,
  which most apps accept.
- **KDE without a screen:** KWin's headless mode can't add a virtual monitor; the
  host uses the real one and the Display settings say so. A headless Plasma
  session has a single 1280×720 mode, so resolution changes there show a note.
- **X11 desktops** keep their own size; the picture is fitted to the window.
- **Windows:** the Windows host and client compile, but are not packaged or tested yet.
- **4K60:** not reached when the host and client share one integrated GPU; it needs real
  hardware on both ends.
