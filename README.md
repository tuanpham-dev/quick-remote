# QuickRemote

A remote desktop and LAN game-streaming system with its own protocol: a
Rust host for Linux (a virtual GPU display, or an X11, KDE, GNOME or wlroots
desktop) and Windows, a native client, a web client (WebTransport) and an
Android app, all sharing one QUIC session that carries video and audio as
datagrams with forward error correction. Over the internet, hosts behind home
routers are reached through a self-hosted rendezvous and relay server.

- `docs/protocol.md`: the wire protocol (version 1.2) and the WebSocket
  transport.
- `docs/TESTING.md`: setting up a host and trying every client by hand.
- `plans/`: the spec and plan the project was built from.

## Building

Rust (stable, edition 2024), GStreamer 1.22 or newer with its development
packages, libpipewire, libopus, libxkbcommon, libva and ALSA headers; see
`scripts/setup-root.sh` for the Debian package list and the one-time system
setup (uinput, groups, UDP buffers).

```
cargo build --release -p qr-host -p qr-client -p qr-relay
scripts/build-web.sh       # the web client, embedded into qr-host
```

## Running a host

```
qr-host                                   # share this desktop (detected)
qr-host --source vdisplay --fps 120       # a headless GPU display for games
qr-host --source vdisplay --desktop xfce4-session --keyboard-layout us
                                          # a full XFCE desktop that follows the client
qr-host --web-listen 0.0.0.0:9443         # also serve the web client
qr-host --ws-listen 127.0.0.1:14600       # also serve WebSocket viewers (Perch)
```

The host prints a PIN; clients pair with it once and keep their keys.

## Perch

The [Perch](https://github.com/tuanpham-dev/perch) Remote Desktop extension
runs `qr-host` with `--ws-listen` and shows the desktop in a tab. Its
traffic rides Perch's own TCP proxy, so the host serves it over a plain
WebSocket on loopback (no pairing; Perch's login is the guard), and any
number of Perch tabs share one session, the first of them driving the
display's size and scale. Releases of `qr-host` for Linux x86_64 and arm64
are built by the workflow in `.github/workflows/release.yml`; the extension
downloads the one for its host.

## Testing

`tests/e2e/run.sh phase1` runs the acceptance scripts (network namespaces,
headless desktops, headless Chrome). `cargo test --workspace` runs the unit
and conformance tests; `cd web && npm test` checks the TypeScript codec
against the same vectors.

## License

MIT or Apache-2.0, at your option.

The virtual display renders on the GPU when this process can open the render node (`/dev/dri/renderD*`, usually the `render` group) and in software otherwise; the host's log says which. A service started outside your login session (Perch's, for example) is often outside that group and gets the software path, which is fine for desktop work.
