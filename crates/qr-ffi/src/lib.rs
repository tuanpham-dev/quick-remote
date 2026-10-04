//! Android bridge: the session core (QUIC, pairing, FEC, input) in Rust,
//! with decoding and display in Kotlin (MediaCodec). Exposed over JNI as
//! `dev.quickremote.Native`.
//!
//! Encoded frames and Opus packets are pulled by Kotlin threads; control
//! events (connected, stream started, errors, rumble, clipboard) are JSON
//! strings from `pollEvent`.

pub mod core;

use std::sync::Arc;

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JLongArray, JShortArray, JString};
use jni::sys::{jboolean, jint, jlong, jstring};

use crate::core::{Handle, handles};

fn throw(env: &mut JNIEnv, e: impl std::fmt::Display) {
    let _ = env.throw_new("java/lang/IllegalStateException", e.to_string());
}

fn string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    env.get_string(s).ok().map(Into::into)
}

fn session(h: jlong) -> Option<Arc<Handle>> {
    handles().lock().expect("lock").get(&h).cloned()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_init(_env: JNIEnv, _c: JClass) {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default().with_tag("QuickRemote").with_max_level(log::LevelFilter::Info),
    );
    qr_net::init_crypto();
}

/// Lists LAN hosts for `ms` milliseconds: JSON array.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_listHosts(mut env: JNIEnv, _c: JClass, ms: jint) -> jstring {
    let json = core::list_hosts(ms as u64);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Connects; returns a handle. `codecs`: bit 0 H.264, 1 H.265, 2 AV1 (hardware decode on this device).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_connect(
    mut env: JNIEnv,
    _c: JClass,
    config_dir: JString,
    target: JString,
    pin: JString,
    name: JString,
    rendezvous: JString,
    codecs: jint,
    width: jint,
    height: jint,
) -> jlong {
    let (Some(dir), Some(target), Some(name)) =
        (string(&mut env, &config_dir), string(&mut env, &target), string(&mut env, &name))
    else {
        throw(&mut env, "missing argument");
        return 0;
    };
    let pin = string(&mut env, &pin);
    let rendezvous = string(&mut env, &rendezvous);
    match core::connect(dir, target, pin, name, rendezvous, codecs as u32, (width as u16, height as u16)) {
        Ok(h) => h,
        Err(e) => {
            throw(&mut env, format!("{e:#}"));
            0
        }
    }
}

/// Next encoded frame into `buf` (direct ByteBuffer). Returns its length,
/// 0 if none within `timeout_ms`, −1 when the session ended. `meta`
/// receives: frame id, keyframe (0/1), codec, width, height, capture time
/// on this device's clock (µs, `System.nanoTime()/1000`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_nextVideo(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    buf: JByteBuffer,
    meta: JLongArray,
    timeout_ms: jint,
) -> jint {
    let Some(s) = session(h) else { return -1 };
    let Some(f) = s.next_video(timeout_ms as u64) else { return if s.ended() { -1 } else { 0 } };
    let Ok(ptr) = env.get_direct_buffer_address(&buf) else { return -1 };
    let cap = env.get_direct_buffer_capacity(&buf).unwrap_or(0);
    if f.au.len() > cap {
        s.request_keyframe();
        return 0;
    }
    // SAFETY: `ptr` points to a direct buffer of `cap` bytes owned by Java
    // for the duration of this call.
    unsafe { std::ptr::copy_nonoverlapping(f.au.as_ptr(), ptr, f.au.len()) };
    let m = [f.id as i64, f.keyframe as i64, f.codec as i64, f.width as i64, f.height as i64, f.capture_local_us];
    let _ = env.set_long_array_region(&meta, 0, &m);
    f.au.len() as jint
}

/// Next Opus packet into `buf`; returns its length (0 none, −1 ended).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_nextAudio(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    buf: JByteBuffer,
    timeout_ms: jint,
) -> jint {
    let Some(s) = session(h) else { return -1 };
    let Some(p) = s.next_audio(timeout_ms as u64) else { return if s.ended() { -1 } else { 0 } };
    let Ok(ptr) = env.get_direct_buffer_address(&buf) else { return -1 };
    let cap = env.get_direct_buffer_capacity(&buf).unwrap_or(0);
    let n = p.len().min(cap);
    // SAFETY: as in nextVideo.
    unsafe { std::ptr::copy_nonoverlapping(p.as_ptr(), ptr, n) };
    n as jint
}

/// A frame left the decoder: frame id and decode time (stats).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_decoded(
    _env: JNIEnv,
    _c: JClass,
    h: jlong,
    id: jlong,
    decode_us: jlong,
) {
    if let Some(s) = session(h) {
        s.decoded(id as u32, decode_us as u64);
    }
}

/// JSON event, or null when none is pending.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_pollEvent(mut env: JNIEnv, _c: JClass, h: jlong) -> jstring {
    let Some(ev) = session(h).and_then(|s| s.poll_event()) else { return std::ptr::null_mut() };
    env.new_string(ev).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_key(_env: JNIEnv, _c: JClass, h: jlong, code: jint, down: jboolean) {
    if let Some(s) = session(h) {
        s.input(qr_proto::InputEvent::Key { code: code as u16, down: down != 0 });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_button(
    _env: JNIEnv,
    _c: JClass,
    h: jlong,
    button: jint,
    down: jboolean,
) {
    if let Some(s) = session(h) {
        s.input(qr_proto::InputEvent::Button { button: button as u8, down: down != 0 });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_abs(_env: JNIEnv, _c: JClass, h: jlong, x: jint, y: jint) {
    if let Some(s) = session(h) {
        s.motion(0.0, 0.0, Some((x.max(0) as u16, y.max(0) as u16)));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_rel(_env: JNIEnv, _c: JClass, h: jlong, dx: f32, dy: f32) {
    if let Some(s) = session(h) {
        s.motion(dx as f64, dy as f64, None);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_wheel(_env: JNIEnv, _c: JClass, h: jlong, dx: jint, dy: jint) {
    if let Some(s) = session(h) {
        s.input(qr_proto::InputEvent::Wheel { dx: dx as i16, dy: dy as i16 });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_text(mut env: JNIEnv, _c: JClass, h: jlong, text: JString) {
    if let (Some(s), Some(t)) = (session(h), string(&mut env, &text)) {
        s.input(qr_proto::InputEvent::Text { text: t });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_pen(
    _env: JNIEnv,
    _c: JClass,
    h: jlong,
    x: jint,
    y: jint,
    pressure: jint,
    tilt_x: jint,
    tilt_y: jint,
    flags: jint,
) {
    if let Some(s) = session(h) {
        s.input(qr_proto::InputEvent::Pen {
            stream: 0,
            x: x.max(0) as u32,
            y: y.max(0) as u32,
            pressure: pressure.clamp(0, 65535) as u16,
            tilt_x: tilt_x.clamp(-90, 90) as i8,
            tilt_y: tilt_y.clamp(-90, 90) as i8,
            flags: flags as u8,
        });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_pad(
    mut env: JNIEnv,
    _c: JClass,
    h: jlong,
    idx: jint,
    buttons: jint,
    axes: JShortArray,
) {
    let Some(s) = session(h) else { return };
    let mut a = [0i16; 6];
    if env.get_short_array_region(&axes, 0, &mut a).is_ok() {
        s.pad(idx as u8, buttons as u32, a);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_padDetach(_env: JNIEnv, _c: JClass, h: jlong, idx: jint) {
    if let Some(s) = session(h) {
        s.control(qr_proto::Msg::PadDetach { idx: idx as u8 });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_clipboard(mut env: JNIEnv, _c: JClass, h: jlong, text: JString) {
    if let (Some(s), Some(t)) = (session(h), string(&mut env, &text)) {
        s.control(qr_proto::Msg::Clipboard { kind: qr_proto::msg::ClipKind::Text as u8, data: t.into_bytes() });
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_quickremote_Native_close(_env: JNIEnv, _c: JClass, h: jlong) {
    if let Some(s) = handles().lock().expect("lock").remove(&h) {
        s.close();
    }
}
