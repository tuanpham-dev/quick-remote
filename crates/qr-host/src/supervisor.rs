//! System service mode (`qr-host --supervise`, as root under systemd): follow
//! logind's active session on seat0 and keep one host running in it — as
//! the session's user on their desktop, or as root on the display manager's
//! X display at the login screen — restarting it when the session changes
//! or the host exits.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use zbus::zvariant::OwnedObjectPath;

const LOGIN1: &str = "org.freedesktop.login1";

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Wayland,
    X11 {
        display: String,
        xauth: Option<String>,
    },
    /// The display manager's login screen (X11 greeters: LightDM, SDDM on X).
    Greeter {
        display: String,
        xauth: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Target {
    uid: u32,
    gid: u32,
    user: String,
    home: String,
    kind: Kind,
}

/// A variable from a `/proc/<pid>/environ` blob.
fn env_var(environ: &[u8], key: &str) -> Option<String> {
    environ
        .split(|&b| b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .map(str::to_string)
}

/// The `-auth` file of an X server serving `display`, from its arguments.
fn xauth_from_args(args: &[&str], display: &str) -> Option<String> {
    if !args.contains(&display) {
        return None;
    }
    args.windows(2).find(|w| w[0] == "-auth").map(|w| w[1].to_string())
}

/// The X server for `display`: its authority file.
fn x_server_auth(display: &str) -> Option<String> {
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<&str> = raw.split(|&b| b == 0).filter_map(|a| std::str::from_utf8(a).ok()).collect();
        let is_x = args.first().is_some_and(|a| a.ends_with("Xorg") || a.ends_with("/X") || a.ends_with("Xwayland"));
        if is_x && let Some(a) = xauth_from_args(&args, display) {
            return Some(a);
        }
    }
    None
}

/// A user's (gid, home).
fn passwd(uid: u32) -> Option<(u32, String)> {
    let mut buf = vec![0u8; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r with a buffer it may fill; `out` is set on success.
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    // SAFETY: pw_dir points into `buf`, NUL-terminated.
    let home = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) }.to_string_lossy().into_owned();
    Some((pw.pw_gid, home))
}

/// What should run now, from seat0's active session.
async fn active(bus: &zbus::Connection) -> Result<Option<Target>> {
    let seat =
        zbus::Proxy::new(bus, LOGIN1, "/org/freedesktop/login1/seat/seat0", "org.freedesktop.login1.Seat").await?;
    let (id, path): (String, OwnedObjectPath) = seat.get_property("ActiveSession").await?;
    if id.is_empty() {
        return Ok(None);
    }
    let s = zbus::Proxy::new(bus, LOGIN1, path, "org.freedesktop.login1.Session").await?;
    let (uid, _): (u32, OwnedObjectPath) = s.get_property("User").await?;
    let user: String = s.get_property("Name").await?;
    let ty: String = s.get_property("Type").await?;
    let class: String = s.get_property("Class").await?;
    let display: String = s.get_property("Display").await?;
    let leader: u32 = s.get_property("Leader").await?;
    let (gid, home) = passwd(uid).context("unknown session user")?;
    let leader_env = std::fs::read(format!("/proc/{leader}/environ")).unwrap_or_default();
    let kind = match (class.as_str(), ty.as_str()) {
        ("greeter", "x11") if !display.is_empty() => Kind::Greeter { xauth: x_server_auth(&display), display },
        ("user", "wayland") => Kind::Wayland,
        ("user", "x11") if !display.is_empty() => {
            let xauth = env_var(&leader_env, "XAUTHORITY").or_else(|| x_server_auth(&display));
            Kind::X11 { display, xauth }
        }
        _ => {
            tracing::debug!(class, ty, "nothing to host in this session");
            return Ok(None);
        }
    };
    Ok(Some(Target { uid, gid, user, home, kind }))
}

/// The user's Wayland socket in their runtime directory.
fn wayland_socket(runtime: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(runtime)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        .collect();
    names.sort();
    names.into_iter().next()
}

fn spawn(t: &Target, forward: &[String]) -> Result<Child> {
    let mut c = Command::new(std::env::current_exe()?);
    c.args(forward);
    let runtime = PathBuf::from(format!("/run/user/{}", t.uid));
    let as_user = |c: &mut Command| {
        c.uid(t.uid)
            .gid(t.gid)
            .env("HOME", &t.home)
            .env("USER", &t.user)
            .env("LOGNAME", &t.user)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}/bus", runtime.display()))
            .current_dir(&t.home);
        let user = std::ffi::CString::new(t.user.clone()).unwrap_or_default();
        let gid = t.gid;
        // SAFETY: initgroups between fork and exec, before the uid change.
        unsafe {
            c.pre_exec(move || {
                if libc::initgroups(user.as_ptr(), gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    };
    match &t.kind {
        Kind::Wayland => {
            let socket = wayland_socket(&runtime).context("the session has no Wayland socket yet")?;
            c.args(["--source", "wayland"]).env("WAYLAND_DISPLAY", socket);
            as_user(&mut c);
        }
        Kind::X11 { display, xauth } => {
            c.args(["--source", &format!("x11:{display}")]).env("DISPLAY", display);
            if let Some(a) = xauth {
                c.env("XAUTHORITY", a);
            }
            as_user(&mut c);
        }
        Kind::Greeter { display, xauth } => {
            // Root, with its own identity: the login screen is another host
            // to clients (paired separately).
            c.args(["--source", &format!("x11:{display}"), "--config-dir", "/var/lib/quickremote/login"])
                .env("DISPLAY", display);
            if let Some(a) = xauth {
                c.env("XAUTHORITY", a);
            }
        }
    }
    // SAFETY: async-signal-safe calls only.
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    tracing::info!(user = t.user, kind = ?t.kind, "starting the host");
    Ok(c.spawn()?)
}

fn stop(c: &mut Child) {
    // SAFETY: signalling our own child.
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    for _ in 0..30 {
        if matches!(c.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = c.kill();
    let _ = c.wait();
}

/// Runs until killed. `forward`: arguments for every host (not `--source`).
pub async fn run(forward: Vec<String>) -> Result<()> {
    // QR_SUPERVISE_DRY=1: only log what would run (any user; for checks).
    let dry = std::env::var_os("QR_SUPERVISE_DRY").is_some();
    // SAFETY: no preconditions.
    if !dry && unsafe { libc::geteuid() } != 0 {
        bail!("--supervise runs as root (the systemd service); a user's own host needs no supervisor");
    }
    let bus = zbus::Connection::system().await.context("connecting to the system bus")?;
    let mut current: Option<(Target, Child)> = None;
    loop {
        if let Some((_, c)) = &mut current
            && let Ok(Some(st)) = c.try_wait()
        {
            tracing::warn!("the host exited ({st}); starting it again");
            current = None;
        }
        let want = active(&bus).await.unwrap_or_else(|e| {
            tracing::warn!("reading the active session: {e:#}");
            None
        });
        if current.as_ref().map(|c| &c.0) != want.as_ref() {
            if let Some((_, mut c)) = current.take() {
                stop(&mut c);
            }
            if let Some(t) = want.clone().filter(|_| dry) {
                tracing::info!(user = t.user, kind = ?t.kind, "would start the host");
                current = Some((t, Command::new("sleep").arg("infinity").spawn()?));
            } else if let Some(t) = want {
                match spawn(&t, &forward) {
                    Ok(c) => current = Some((t, c)),
                    Err(e) => tracing::warn!("starting the host: {e:#}"),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_environment_blobs() {
        let env = b"PATH=/usr/bin\0XAUTHORITY=/run/user/1000/gdm/Xauthority\0DISPLAY=:0\0";
        assert_eq!(env_var(env, "XAUTHORITY").as_deref(), Some("/run/user/1000/gdm/Xauthority"));
        assert_eq!(env_var(env, "DISPLAY").as_deref(), Some(":0"));
        assert_eq!(env_var(env, "HOME"), None);
    }

    #[test]
    fn finds_the_x_servers_authority() {
        let args =
            ["/usr/lib/xorg/Xorg", ":0", "-seat", "seat0", "-auth", "/var/run/lightdm/root/:0", "-nolisten", "tcp"];
        assert_eq!(xauth_from_args(&args, ":0").as_deref(), Some("/var/run/lightdm/root/:0"));
        assert_eq!(xauth_from_args(&args, ":1"), None);
    }
}
