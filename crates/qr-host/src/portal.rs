//! Screen capture through the xdg-desktop-portal ScreenCast interface: a
//! PipeWire node and a remote fd for `pipewiresrc`. Used for the virtual
//! display (on its private bus) and for real Wayland desktops (on the
//! session bus, with a restore token so permission is asked once).

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};

/// A running screencast: keep it alive while the pipeline reads from it.
pub struct Cast {
    pub node: u32,
    pub fd: OwnedFd,
    pub size: Option<(i32, i32)>,
    pub restore_token: Option<String>,
    // Dropping the connection ends the portal session.
    _conn: zbus::Connection,
}

impl Cast {
    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

/// Starts a monitor screencast. `bus`: a D-Bus address, or `None` for the
/// session bus. `restore`: a token from an earlier cast (no prompt).
pub async fn start(bus: Option<&str>, restore: Option<&str>) -> Result<Cast> {
    let conn = match bus {
        Some(addr) => zbus::connection::Builder::address(addr)?.build().await.context("connecting to the portal bus")?,
        None => zbus::Connection::session().await.context("connecting to the session bus")?,
    };
    let proxy = Screencast::with_connection(conn.clone()).await.context("the ScreenCast portal is not available")?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore),
        )
        .await?
        .response()
        .context("selecting the screen to share was refused")?;
    let streams = proxy.start(&session, None, StartCastOptions::default()).await?.response().context("screen sharing was refused")?;
    let s = streams.streams().first().context("the portal returned no stream")?;
    let node = s.pipe_wire_node_id();
    let size = s.size();
    let restore_token = streams.restore_token().map(str::to_string);
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
    // The session must outlive this function: leak it into the connection's
    // lifetime (closing the connection closes it).
    std::mem::forget(session);
    Ok(Cast { node, fd, size, restore_token, _conn: conn })
}
