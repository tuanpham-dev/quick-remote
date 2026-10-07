//! Windows service mode (`qr-host --service`, installed with
//! `--install-service`): keeps a host running in the active console session
//! as SYSTEM, so the login screen, the lock screen and UAC prompts can be
//! reached too, and restarts it when the session changes or it exits.

use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const NAME: &str = "QuickRemoteHost";
const STILL_ACTIVE: u32 = 259;

/// The host arguments the service passes on (set by `run`).
static FORWARD: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Runs as the service (the service manager starts `qr-host --service`).
pub fn run(forward: Vec<String>) -> Result<()> {
    let _ = FORWARD.set(forward);
    service_dispatcher::start(NAME, ffi_service_main).context("not started by the service manager")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_body() {
        tracing::error!("service: {e:#}");
    }
}

fn service_body() -> Result<()> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let status = service_control_handler::register(NAME, move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let set = |state, accept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
    };
    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN);
    let forward = FORWARD.get().cloned().unwrap_or_default();
    let mut child: Option<(u32, PROCESS_INFORMATION)> = None;
    loop {
        // SAFETY: no preconditions.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let running = child.as_ref().is_some_and(|(s, p)| *s == session && alive(p));
        if !running {
            if let Some((_, p)) = child.take() {
                stop(&p);
            }
            if session != u32::MAX {
                match launch(session, &forward) {
                    Ok(p) => child = Some((session, p)),
                    Err(e) => tracing::warn!("starting the host in session {session}: {e:#}"),
                }
            }
        }
        if stop_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
            break;
        }
    }
    if let Some((_, p)) = child.take() {
        stop(&p);
    }
    set(ServiceState::Stopped, ServiceControlAccept::empty());
    Ok(())
}

fn alive(p: &PROCESS_INFORMATION) -> bool {
    let mut code = 0u32;
    // SAFETY: a process handle we own.
    unsafe { GetExitCodeProcess(p.hProcess, &mut code) }.is_ok() && code == STILL_ACTIVE
}

fn stop(p: &PROCESS_INFORMATION) {
    // SAFETY: handles we own, closed once.
    unsafe {
        let _ = TerminateProcess(p.hProcess, 0);
        let _ = CloseHandle(p.hProcess);
        let _ = CloseHandle(p.hThread);
    }
}

/// Starts the host in `session` as SYSTEM on the interactive desktop: this
/// service's own token, moved to that session.
fn launch(session: u32, forward: &[String]) -> Result<PROCESS_INFORMATION> {
    let exe = std::env::current_exe()?;
    let mut cmd = format!("\"{}\" --source d3d11", exe.display());
    for a in forward {
        cmd.push_str(&format!(" \"{}\"", a.replace('"', "\\\"")));
    }
    let mut cmd: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    // SAFETY: Win32 calls on handles we own, each closed below.
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own)?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        dup?;
        let set = SetTokenInformation(
            token,
            TokenSessionId,
            &session as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        if let Err(e) = set {
            let _ = CloseHandle(token);
            bail!("moving the token to session {session}: {e}");
        }
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = CloseHandle(token);
        r?;
        tracing::info!(session, "host started");
        Ok(pi)
    }
}

/// Registers the service (automatic start, LocalSystem).
pub fn install(forward: &[String]) -> Result<()> {
    let m = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let mut args = vec![OsString::from("--service")];
    args.extend(forward.iter().map(OsString::from));
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "QuickRemote host".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: args,
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    m.create_service(&info, ServiceAccess::QUERY_STATUS)?;
    println!("Installed the {NAME} service (starts with Windows; `sc start {NAME}` to start it now).");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(NAME, ServiceAccess::DELETE | ServiceAccess::STOP)?;
    let _ = s.stop();
    s.delete()?;
    println!("Removed the {NAME} service.");
    Ok(())
}
