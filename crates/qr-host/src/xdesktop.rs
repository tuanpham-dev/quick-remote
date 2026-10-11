//! A full X11 desktop (XFCE and the like) on the host's own virtual
//! display: a rootful, fullscreen Xwayland inside the headless sway, the
//! desktop command run in it, and a follower that keeps the X screen at the
//! size of Xwayland's window buffer and the desktop's UI scale in step with
//! the display's scale, so a display request or a pixel-ratio change from
//! the client carries through. `qr-host --source vdisplay --desktop <cmd>`.
//!
//! Chrome started in this desktop uses Vulkan: Chrome's OpenGL path crashes
//! its GPU process on rootful Xwayland (WebGL then reports "not
//! supported"), so a wrapper first on PATH and a menu entry pointing at it
//! add the flags.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::vdisplay::{VirtualDisplay, spawn, wait_for, x_sockets};

pub struct XDesktop {
    /// The X display (`:N`) the desktop runs on.
    pub display: String,
    dir: PathBuf,
    children: Vec<(&'static str, Child)>,
    stop: Arc<AtomicBool>,
    follower: Option<std::thread::JoinHandle<()>>,
    /// The display's current scale (sway's output scale), kept by the
    /// follower; what apps started later should be told.
    scale: Arc<Mutex<f64>>,
    wayland_display: String,
    /// The display's private runtime directory and session bus, for apps
    /// started into this desktop from outside (sound, portals, xfconf).
    session_env: Vec<(String, String)>,
}

/// The pointer's size at 1x; it grows with the desktop's integer scale.
const CURSOR_SIZE: u32 = 24;

/// Environment for a toolkit started at `scale`: GTK takes an integer
/// window scale plus a fractional font scale, Qt one factor, and the
/// cursor theme size follows the integer scale.
pub fn scale_env(scale: f64) -> Vec<(String, String)> {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let gdk = if s >= 2.0 { 2 } else { 1 };
    vec![
        ("XCURSOR_SIZE".into(), (CURSOR_SIZE * gdk).to_string()),
        ("GDK_SCALE".into(), gdk.to_string()),
        ("GDK_DPI_SCALE".into(), format!("{:.3}", s / gdk as f64)),
        ("QT_SCALE_FACTOR".into(), format!("{s:.3}")),
        ("QT_ENABLE_HIGHDPI_SCALING".into(), "0".into()),
    ]
}

fn env_of(vd: &VirtualDisplay) -> Vec<(String, String)> {
    vd.env().to_vec()
}

fn run_in(display: &str, env: &[(String, String)], prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("DISPLAY", display)
        .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()))
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {prog}"))?;
    if !out.status.success() {
        bail!("{prog} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl XDesktop {
    pub fn start(vd: &VirtualDisplay, command: &str, layout: Option<&str>, dpi: u32) -> Result<Self> {
        for prog in ["Xwayland", "xrandr"] {
            which(prog).with_context(|| format!("--desktop needs {prog} on PATH"))?;
        }
        let dir = vd.runtime.join("xdesktop");
        std::fs::create_dir_all(dir.join("bin"))?;
        std::fs::create_dir_all(dir.join("share/applications"))?;
        let mut env = env_of(vd);
        let wayland_display = env
            .iter()
            .find(|(k, _)| k == "WAYLAND_DISPLAY")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "wayland-1".into());
        let session_env: Vec<(String, String)> =
            env.iter().filter(|(k, _)| k == "XDG_RUNTIME_DIR" || k == "DBUS_SESSION_BUS_ADDRESS").cloned().collect();
        // A free X display number, from :20 up (sway's own Xwayland took a
        // lower one).
        let taken = x_sockets();
        let display = (20..200).map(|n| format!(":{n}")).find(|d| !taken.contains(d)).context("no free X display")?;
        let log = |n: &str| dir.join(format!("{n}.log"));

        let mut c = Command::new("Xwayland");
        c.args([display.as_str(), "-fullscreen", "-hidpi", "-noreset"]);
        for k in ["DISPLAY", "SWAYSOCK", "SESSION_MANAGER", "XDG_SESSION_ID"] {
            c.env_remove(k);
        }
        c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let mut me = Self {
            display: display.clone(),
            dir: dir.clone(),
            children: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
            follower: None,
            scale: Arc::new(Mutex::new(1.0)),
            wayland_display,
            session_env,
        };
        me.children.push(("Xwayland", spawn("Xwayland", &mut c, &log("Xwayland"))?));
        wait_for(&PathBuf::from(format!("/tmp/.X11-unix/X{}", &display[1..])), "Xwayland", Duration::from_secs(10))?;
        // Best effort: the keyboard layout (sway's seat sets it anyway) and
        // the font DPI for toolkits that read X resources.
        if let Some(l) = layout.filter(|l| !l.is_empty())
            && which("setxkbmap").is_some()
            && let Err(e) = run_in(&display, &env, "setxkbmap", &["-layout", l])
        {
            tracing::warn!("setxkbmap: {e:#}");
        }
        if which("xrdb").is_some() {
            let res = dir.join("Xresources");
            std::fs::write(&res, format!("Xft.dpi: {dpi}\n"))?;
            if let Err(e) = run_in(&display, &env, "xrdb", &["-merge", &res.display().to_string()]) {
                tracing::warn!("xrdb: {e:#}");
            }
        }

        // Chrome with Vulkan, for this desktop only.
        let wrapper = dir.join("bin/google-chrome-stable");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nexec /usr/bin/google-chrome-stable --use-angle=vulkan --enable-features=Vulkan \"$@\"\n",
        )?;
        std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        let _ = std::os::unix::fs::symlink("google-chrome-stable", dir.join("bin/google-chrome"));
        if let Ok(desktop) = std::fs::read_to_string("/usr/share/applications/google-chrome.desktop") {
            std::fs::write(
                dir.join("share/applications/google-chrome.desktop"),
                desktop.replace("/usr/bin/google-chrome-stable", &wrapper.display().to_string()),
            )?;
        }

        // The desktop itself, on the display's own session bus (the
        // follower talks to xfconfd there).
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
        let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        env.retain(|(k, _)| k != "WAYLAND_DISPLAY" && k != "XDG_SESSION_TYPE" && k != "XDG_CURRENT_DESKTOP");
        env.push(("DISPLAY".into(), display.clone()));
        env.push(("XDG_SESSION_TYPE".into(), "x11".into()));
        env.push(("XDG_CURRENT_DESKTOP".into(), "XFCE".into()));
        env.push(("PATH".into(), format!("{}:{path}", dir.join("bin").display())));
        env.push(("XDG_DATA_DIRS".into(), format!("{}:{data_dirs}", dir.join("share").display())));
        // No GDK_SCALE for the session itself: GTK then follows the
        // desktop's xsettings scale as it changes (an explicit GDK_SCALE
        // pins it for the process's life). Programs started later get the
        // current scale through `env()`.
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        for k in ["WAYLAND_DISPLAY", "SWAYSOCK", "SESSION_MANAGER", "XDG_SESSION_ID"] {
            c.env_remove(k);
        }
        c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        c.current_dir(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")));
        me.children.push(("desktop", spawn("desktop", &mut c, &log("desktop"))?));

        // The follower: X screen size and desktop scale from the output.
        let (stop, scale, sock, disp, fenv) =
            (me.stop.clone(), me.scale.clone(), vd.sway_socket(), display.clone(), env.clone());
        me.follower = Some(std::thread::Builder::new().name("xdesktop".into()).spawn(move || {
            follow(&stop, &scale, &sock, &disp, &fenv, dpi);
        })?);
        let shown = display.clone();
        tracing::info!(x_display = shown, command, "managed desktop started");
        Ok(me)
    }

    /// Environment for a program started on this desktop now: its display
    /// and the current scale.
    /// The display's current scale, as the follower keeps it.
    pub fn scale_handle(&self) -> Arc<Mutex<f64>> {
        self.scale.clone()
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let mut v =
            vec![("DISPLAY".into(), self.display.clone()), ("WAYLAND_DISPLAY".into(), self.wayland_display.clone())];
        v.extend(self.session_env.iter().cloned());
        v.extend(scale_env(*self.scale.lock().expect("lock")));
        v
    }

    /// The first of the desktop's processes (Xwayland, the session) that
    /// has exited, if any: the desktop is unusable then.
    pub fn exited(&mut self) -> Option<String> {
        for (name, c) in &mut self.children {
            if let Ok(Some(st)) = c.try_wait() {
                return Some(format!("{name} exited ({st}); see {}", self.dir.join(format!("{name}.log")).display()));
            }
        }
        None
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
        if let Some(t) = self.follower.take() {
            let _ = t.join();
        }
        if std::env::var_os("QR_VD_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl Drop for XDesktop {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Keeps the X screen at the size of Xwayland's window buffer (the
/// output's logical size times the integer scale sway gives Xwayland) and
/// XFCE's UI scale at that integer, polling the output twice a second.
fn follow(stop: &AtomicBool, scale: &Mutex<f64>, sock: &Path, display: &str, env: &[(String, String)], dpi: u32) {
    let mut last: Option<(u32, u32, u32)> = None;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() < next {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        next = Instant::now() + Duration::from_millis(500);
        let Ok((w, h, s)) = crate::vdisplay::output_state(sock) else { continue };
        if w == 0 || h == 0 {
            continue;
        }
        *scale.lock().expect("lock") = s;
        let k = (s - 1e-6).ceil().max(1.0) as u32;
        let want = (w * k, h * k, k);
        if last == Some(want) {
            continue;
        }
        let mode = format!("{}x{}", want.0, want.1);
        let modes = run_in(display, env, "xrandr", &[]).unwrap_or_default();
        if !modes.lines().any(|l| l.trim_start().starts_with(&format!("{mode} "))) {
            let _ = run_in(
                display,
                env,
                "xrandr",
                &["--newmode", &mode, "0", &want.0.to_string(), "0", "0", "0", &want.1.to_string(), "0", "0", "0"],
            );
            let _ = run_in(display, env, "xrandr", &["--addmode", "XWAYLAND0", &mode]);
        }
        match run_in(display, env, "xrandr", &["--output", "XWAYLAND0", "--mode", &mode]) {
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("xrandr: {e:#}");
                continue;
            }
        }
        if which("xfconf-query").is_some() {
            let _ = run_in(
                display,
                env,
                "xfconf-query",
                &["-c", "xsettings", "-p", "/Gdk/WindowScalingFactor", "-n", "-t", "int", "-s", &k.to_string()],
            );
            let _ = run_in(
                display,
                env,
                "xfconf-query",
                &["-c", "xsettings", "-p", "/Xft/DPI", "-n", "-t", "int", "-s", &dpi.to_string()],
            );
            // The pointer grows with the desktop too (XFCE scales windows
            // and fonts, not the cursor theme).
            let _ = run_in(
                display,
                env,
                "xfconf-query",
                &[
                    "-c",
                    "xsettings",
                    "-p",
                    "/Gtk/CursorThemeSize",
                    "-n",
                    "-t",
                    "int",
                    "-s",
                    &(CURSOR_SIZE * k).to_string(),
                ],
            );
        }
        if which("xrdb").is_some() {
            let res = std::env::temp_dir().join(format!("qr-xcursor-{}", std::process::id()));
            if std::fs::write(&res, format!("Xcursor.size: {}\n", CURSOR_SIZE * k)).is_ok() {
                let _ = run_in(display, env, "xrdb", &["-merge", &res.display().to_string()]);
                let _ = std::fs::remove_file(&res);
            }
        }
        tracing::info!(screen = %mode, scale = k, "managed desktop: X screen follows the display");
        last = Some(want);
    }
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(prog)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_env_splits_gtk_scale() {
        let e = scale_env(1.5);
        assert!(e.contains(&("GDK_SCALE".into(), "1".into())));
        assert!(e.contains(&("GDK_DPI_SCALE".into(), "1.500".into())));
        let e = scale_env(2.0);
        assert!(e.contains(&("GDK_SCALE".into(), "2".into())));
        assert!(e.contains(&("GDK_DPI_SCALE".into(), "1.000".into())));
    }
}
