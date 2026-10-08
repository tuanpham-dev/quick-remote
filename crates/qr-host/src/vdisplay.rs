//! Virtual display: a headless sway (wlroots) on the GPU with its own
//! D-Bus, PipeWire and screencast portal, so the host can run a desktop or
//! a game with no monitor attached and capture it zero-copy.
//!
//! Everything lives in a private runtime directory; every child gets
//! `PR_SET_PDEATHSIG` and its own process group, so a killed host takes the
//! display down with it.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qr_proto::Monitor;

pub struct VirtualDisplay {
    pub runtime: PathBuf,
    children: Vec<(&'static str, Child)>,
    /// Environment for apps on this display and for the portal client.
    pub env: Vec<(String, String)>,
    swaysock: PathBuf,
    /// The display's Xwayland (`:N`), for X11 apps there.
    pub x_display: Option<String>,
}

/// The X displays with a socket now.
fn x_sockets() -> std::collections::HashSet<String> {
    std::fs::read_dir("/tmp/.X11-unix")
        .map(|d| {
            d.flatten().filter_map(|e| e.file_name().to_str()?.strip_prefix('X').map(|n| format!(":{n}"))).collect()
        })
        .unwrap_or_default()
}

fn spawn(name: &'static str, cmd: &mut Command, log: &Path) -> Result<Child> {
    let out = std::fs::File::create(log)?;
    cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("starting {name}"))
}

fn wait_for(path: &Path, what: &str, timeout: Duration) -> Result<()> {
    let end = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() > end {
            bail!("{what} did not start (no {})", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

impl VirtualDisplay {
    /// Starts the display at `w×h@refresh`. `desktop` runs inside it (for
    /// example `xfce4-session` under Xwayland, or a game); `gpu` selects the
    /// GLES renderer (otherwise pixman, in software).
    pub fn start(w: u32, h: u32, refresh: u32, desktop: Option<&str>, gpu: bool) -> Result<Self> {
        let rt = std::env::temp_dir().join(format!("qr-vd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(rt.join("cfg/xdg-desktop-portal"))?;
        std::fs::create_dir_all(rt.join("cfg/xdg-desktop-portal-wlr"))?;
        std::fs::create_dir_all(rt.join("cfg/pipewire/pipewire.conf.d"))?;
        std::fs::create_dir_all(rt.join("cfg/wireplumber/wireplumber.conf.d"))?;
        // Audio: apps play into `qr-out` (captured for the client) and
        // record from `qr-mic` (the client's microphone). The machine's own
        // sound devices stay out of it.
        std::fs::write(
            rt.join("cfg/pipewire/pipewire.conf.d/quickremote.conf"),
            // The microphone is a loopback: the host plays into the sink
            // `qr-mic-in`, apps record from the source `qr-mic` (a stream
            // can't play into a source node).
            r#"context.objects = [
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = "qr-out"
      node.description = "QuickRemote" media.class = Audio/Sink audio.position = [ FL FR ] } }
]
context.modules = [
  { name = libpipewire-module-loopback args = {
      node.description = "QuickRemote microphone"
      capture.props = { node.name = "qr-mic-in" media.class = Audio/Sink audio.position = [ MONO ] }
      playback.props = { node.name = "qr-mic" media.class = Audio/Source audio.position = [ MONO ] } } }
]
"#,
        )?;
        std::fs::write(
            rt.join("cfg/wireplumber/wireplumber.conf.d/quickremote.conf"),
            "wireplumber.profiles = {\n  main = {\n    monitor.alsa = disabled\n    monitor.alsa-midi = disabled\n    \
             monitor.bluez = disabled\n    monitor.bluez.midi = disabled\n    monitor.v4l2 = disabled\n    \
             monitor.libcamera = disabled\n  }\n}\n",
        )?;
        std::os::unix::fs::PermissionsExt::set_mode(&mut std::fs::metadata(&rt)?.permissions(), 0o700);
        std::fs::set_permissions(&rt, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        // Portals: wlr for screencast; the output to capture is whatever
        // name the host writes to `next-output` (the chooser prints it).
        std::fs::write(
            rt.join("cfg/xdg-desktop-portal/sway-portals.conf"),
            "[preferred]\ndefault=wlr;gtk\norg.freedesktop.impl.portal.ScreenCast=wlr\n",
        )?;
        std::fs::write(rt.join("next-output"), "HEADLESS-1\n")?;
        std::fs::write(
            rt.join("cfg/xdg-desktop-portal-wlr/config"),
            format!(
                "[screencast]\nchooser_type=dmenu\nchooser_cmd=cat {}\nmax_fps={refresh}\n",
                rt.join("next-output").display()
            ),
        )?;
        // The desktop command runs from a script: sway would expand `$…` in
        // it as its own variables and split it on `;` and `,`. Sway doesn't
        // give config commands the Xwayland display, so the script waits for
        // the host to find it (`x-display`).
        let exec = match desktop {
            Some(d) => {
                let script = rt.join("desktop.sh");
                let xd = rt.join("x-display");
                std::fs::write(
                    &script,
                    format!(
                        "#!/bin/sh\nfor _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do \
                         [ -s {xd} ] && break; sleep 0.1; done\n\
                         [ -s {xd} ] && export DISPLAY=\"$(cat {xd})\"\n{d}\n",
                        xd = xd.display()
                    ),
                )?;
                format!("exec sh {}\n", script.display())
            }
            None => String::new(),
        };
        std::fs::write(
            rt.join("sway.conf"),
            format!(
                "output HEADLESS-1 mode {w}x{h}@{refresh}Hz position 0 0\n\
                 xwayland enable\n\
                 default_border none\n\
                 seat * hide_cursor 0\n\
                 {exec}"
            ),
        )?;

        let bus = format!("unix:path={}", rt.join("bus").display());
        let mut env: Vec<(String, String)> = vec![
            ("XDG_RUNTIME_DIR".into(), rt.display().to_string()),
            ("XDG_CONFIG_HOME".into(), rt.join("cfg").display().to_string()),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
            ("XDG_CURRENT_DESKTOP".into(), "sway".into()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), bus.clone()),
        ];
        let mut me = Self {
            runtime: rt.clone(),
            children: Vec::new(),
            env: Vec::new(),
            swaysock: PathBuf::new(),
            x_display: None,
        };
        let cmd = |prog: &str, env: &[(String, String)]| {
            let mut c = Command::new(prog);
            // A clean session environment: nothing from the desktop the
            // host was started from.
            for k in ["WAYLAND_DISPLAY", "DISPLAY", "SWAYSOCK", "SESSION_MANAGER", "XDG_SESSION_ID"] {
                c.env_remove(k);
            }
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            c
        };
        let log = |n: &str| rt.join(format!("{n}.log"));

        let mut c = cmd("dbus-daemon", &env);
        c.args(["--session", &format!("--address={bus}"), "--nofork", "--nopidfile"]);
        me.children.push(("dbus", spawn("dbus-daemon", &mut c, &log("dbus"))?));
        wait_for(&rt.join("bus"), "D-Bus", Duration::from_secs(5))?;
        me.children.push(("pipewire", spawn("pipewire", &mut cmd("pipewire", &env), &log("pipewire"))?));
        wait_for(&rt.join("pipewire-0"), "PipeWire", Duration::from_secs(5))?;
        me.children.push(("wireplumber", spawn("wireplumber", &mut cmd("wireplumber", &env), &log("wireplumber"))?));
        me.children.push((
            "pipewire-pulse",
            spawn("pipewire-pulse", &mut cmd("pipewire-pulse", &env), &log("pipewire-pulse"))?,
        ));

        let mut c = cmd("sway", &env);
        c.args(["-c", &rt.join("sway.conf").display().to_string()]);
        c.env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", if gpu { "gles2" } else { "pixman" })
            .env("LIBSEAT_BACKEND", "noop")
            .env("WLR_HEADLESS_OUTPUTS", "1");
        let x_before = x_sockets();
        me.children.push(("sway", spawn("sway", &mut c, &log("sway"))?));
        wait_for(&rt.join("wayland-1"), "sway", Duration::from_secs(10))?;
        // Xwayland's socket appears with the compositor (it starts lazily).
        let end = Instant::now() + Duration::from_secs(3);
        while me.x_display.is_none() && Instant::now() < end {
            me.x_display = x_sockets().difference(&x_before).min().cloned();
            if me.x_display.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if let Some(d) = &me.x_display {
            std::fs::write(rt.join("x-display"), d)?;
            env.push(("DISPLAY".into(), d.clone()));
        }
        env.push(("WAYLAND_DISPLAY".into(), "wayland-1".into()));
        let end = Instant::now() + Duration::from_secs(5);
        let sock = loop {
            if let Some(s) = std::fs::read_dir(&rt)?
                .flatten()
                .map(|e| e.path())
                .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("sway-ipc.")))
            {
                break s;
            }
            if Instant::now() > end {
                bail!("sway gave no IPC socket");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        env.push(("SWAYSOCK".into(), sock.display().to_string()));
        me.swaysock = sock;

        me.children.push((
            "xdpw",
            spawn(
                "xdg-desktop-portal-wlr",
                cmd("/usr/libexec/xdg-desktop-portal-wlr", &env)
                    .args(std::env::var_os("QR_VD_DEBUG").map(|_| ["-l", "DEBUG"]).into_iter().flatten()),
                &log("xdpw"),
            )?,
        ));
        me.children.push((
            "xdp",
            spawn("xdg-desktop-portal", &mut cmd("/usr/libexec/xdg-desktop-portal", &env), &log("xdp"))?,
        ));
        me.env = env;
        me.default_audio();
        Ok(me)
    }

    /// Makes `qr-out` and `qr-mic` the defaults (best effort: with the
    /// hardware monitors off they are the only ones anyway).
    fn default_audio(&self) {
        let end = Instant::now() + Duration::from_secs(3);
        for (key, node) in [("default.configured.audio.sink", "qr-out"), ("default.configured.audio.source", "qr-mic")]
        {
            loop {
                let ok = Command::new("pw-metadata")
                    .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .args(["-n", "default", "0", key, &format!("{{ \"name\": \"{node}\" }}"), "Spa:String:JSON"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if ok || Instant::now() > end {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// A new connection to the display's PipeWire, for `pipewiresrc` /
    /// `pipewiresink fd=` (which take it over).
    pub fn pipewire_fd(&self) -> Result<i32> {
        use std::os::fd::IntoRawFd;
        let s = std::os::unix::net::UnixStream::connect(self.runtime.join("pipewire-0"))
            .context("connecting to the display's PipeWire")?;
        Ok(s.into_raw_fd())
    }

    /// The display's Wayland socket.
    pub fn wayland_socket(&self) -> PathBuf {
        self.runtime.join("wayland-1")
    }

    pub fn bus_address(&self) -> String {
        format!("unix:path={}", self.runtime.join("bus").display())
    }

    fn swaymsg(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("swaymsg").env("SWAYSOCK", &self.swaysock).args(args).output()?;
        if !out.status.success() {
            bail!("swaymsg {args:?}: {}", String::from_utf8_lossy(&out.stdout));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Changes an output's mode at run time (the portal stream follows).
    pub fn set_mode(&self, output: &str, w: u32, h: u32, refresh: u32) -> Result<()> {
        self.swaymsg(&["output", output, "mode", &format!("{w}x{h}@{refresh}Hz")])?;
        Ok(())
    }

    /// Sets an output's scale (100 = none): apps keep their apparent size
    /// at a higher resolution.
    pub fn set_scale(&self, output: &str, scale_pct: u16) -> Result<()> {
        let s = format!("{:.2}", scale_pct.clamp(50, 400) as f64 / 100.0);
        self.swaymsg(&["output", output, "scale", &s])?;
        Ok(())
    }

    /// Adds a headless output to the right of the others; returns its name.
    pub fn add_output(&self, w: u32, h: u32, refresh: u32) -> Result<String> {
        let before: Vec<String> = self.monitors()?.into_iter().map(|m| m.name).collect();
        self.swaymsg(&["create_output"])?;
        let name = self
            .monitors()?
            .into_iter()
            .map(|m| m.name)
            .find(|n| !before.contains(n))
            .context("sway created no output")?;
        let x: i32 =
            self.monitors()?.iter().filter(|m| m.name != name).map(|m| m.x + m.width as i32).max().unwrap_or(0);
        self.swaymsg(&["output", &name, "mode", &format!("{w}x{h}@{refresh}Hz"), "position", &x.to_string(), "0"])?;
        Ok(name)
    }

    /// The outputs as protocol monitors (ids in sway's order).
    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        let v: serde_json::Value = serde_json::from_str(&self.swaymsg(&["-t", "get_outputs", "-r"])?)?;
        Ok(v.as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, o)| Monitor {
                        id: i as u8,
                        name: o["name"].as_str().unwrap_or("").to_string(),
                        width: o["current_mode"]["width"].as_u64().unwrap_or(0) as u16,
                        height: o["current_mode"]["height"].as_u64().unwrap_or(0) as u16,
                        refresh_mhz: o["current_mode"]["refresh"].as_u64().unwrap_or(0) as u32,
                        x: o["rect"]["x"].as_i64().unwrap_or(0) as i32,
                        y: o["rect"]["y"].as_i64().unwrap_or(0) as i32,
                        primary: i == 0,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Which output the next screencast session captures.
    pub fn select_output(&self, name: &str) -> Result<()> {
        std::fs::write(self.runtime.join("next-output"), format!("{name}\n"))?;
        Ok(())
    }

    /// The first child that exited, if any (the display is unusable then).
    pub fn exited(&mut self) -> Option<String> {
        for (name, c) in &mut self.children {
            if let Ok(Some(st)) = c.try_wait() {
                return Some(format!(
                    "{name} exited ({st}); see {}",
                    self.runtime.join(format!("{name}.log")).display()
                ));
            }
        }
        None
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl VirtualDisplay {
    /// Ends everything the display started (its desktop included, through
    /// sway's process group) and removes its runtime directory. Runs on
    /// drop, and from the host's signal handler: a host stopped with
    /// SIGTERM leaves nothing behind.
    pub fn shutdown(&mut self) {
        // Reverse order: portals, sway, then the session services.
        for (_, c) in self.children.iter_mut().rev() {
            // SAFETY: signalling the child's own process group.
            unsafe { libc::kill(-(c.id() as i32), libc::SIGTERM) };
        }
        std::thread::sleep(Duration::from_millis(300));
        for (_, c) in self.children.iter_mut().rev() {
            unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
            let _ = c.wait();
        }
        self.children.clear();
        // QR_VD_KEEP=1 keeps the logs for debugging.
        if std::env::var_os("QR_VD_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.runtime);
        }
    }
}
