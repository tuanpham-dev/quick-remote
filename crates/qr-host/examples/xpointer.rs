//! Prints the pointer position on an X display: `xpointer :77` → `x y`.
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt;

fn main() -> anyhow::Result<()> {
    let display = std::env::args().nth(1).unwrap_or(":0".into());
    let (conn, screen) = x11rb::connect(Some(&display))?;
    let root = conn.setup().roots[screen].root;
    let r = conn.query_pointer(root)?.reply()?;
    println!("{} {}", r.root_x, r.root_y);
    Ok(())
}
