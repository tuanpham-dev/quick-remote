//! The window client: host list, pairing, and the session view with its
//! stats overlay and input capture.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use qr_media::decode::DecodedFrame;
use qr_media::probe::DecoderInfo;
use qr_net::discovery::{Browser, Discovery, HostInfo, HostList};
use qr_net::keystore::KeyStore;
use qr_net::session::ClientAuthError;
use qr_net::{Identity, clock::now_us};
use qr_proto::InputEvent;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

use crate::keymap;
use crate::render::{VideoRenderer, fit};
use crate::session::{self, ConnectOpts, Connected, FrameTimes};

pub enum UserEvent {
    Frame,
    /// A pen sample (Windows: winit has no pressure or tilt).
    #[cfg(windows)]
    Pen(crate::pen::PenSample),
    /// A picture for the second window.
    ExtraFrame,
    Discovery(Discovery),
    Connected(Arc<Connected>),
    Ended(String),
    Failed(String),
    NeedPin(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Screen {
    Hosts,
    Pin { target: String },
    Connecting(String),
    Session,
    Reconnecting { target: String, since: std::time::Instant },
    Error(String),
}

struct Gpu {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    video: VideoRenderer,
    egui: egui_wgpu::Renderer,
}

/// The stream a second window shows.
const EXTRA_STREAM: u8 = 1;

/// A second window showing another monitor (view only).
struct ExtraWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    video: VideoRenderer,
}

/// The monitor after `cur` in the host's list (wrapping).
fn next_monitor(monitors: &[qr_proto::Monitor], cur: u8) -> Option<u8> {
    let i = monitors.iter().position(|m| m.id == cur).map_or(0, |i| i + 1);
    monitors.get(i % monitors.len().max(1)).map(|m| m.id)
}

pub struct Config {
    pub id: Identity,
    pub keystore: Arc<Mutex<KeyStore>>,
    pub decoders: Vec<DecoderInfo>,
    pub decoder_override: Option<String>,
    pub name: String,
    pub connect: Option<String>,
    pub pin: Option<String>,
    pub rendezvous: Option<String>,
}

pub struct App {
    cfg: Arc<Config>,
    rt: tokio::runtime::Runtime,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    screen: Screen,
    hosts: HostList,
    _browser: Option<Browser>,
    session: Option<Arc<Connected>>,
    target: Option<String>,
    frame_slot: Arc<Mutex<Option<(DecodedFrame, FrameTimes)>>>,
    shown: Option<FrameTimes>,
    extra: Option<ExtraWindow>,
    extra_slot: Arc<Mutex<Option<DecodedFrame>>>,
    // UI state.
    pin_input: String,
    addr_input: String,
    show_stats: bool,
    relative: bool,
    sharp: bool,
    mods: ModifiersState,
    held: HashSet<u16>,
    cursor: (f64, f64),
}

const HOTKEY: ModifiersState = ModifiersState::CONTROL.union(ModifiersState::ALT).union(ModifiersState::SHIFT);

pub fn run(cfg: Config) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let browser = {
        let _g = rt.enter();
        match Browser::start() {
            Ok((b, mut rx)) => {
                let p = proxy.clone();
                rt.spawn(async move {
                    while let Some(d) = rx.recv().await {
                        if p.send_event(UserEvent::Discovery(d)).is_err() {
                            break;
                        }
                    }
                });
                Some(b)
            }
            Err(e) => {
                tracing::warn!("LAN discovery unavailable: {e}");
                None
            }
        }
    };
    let connect = cfg.connect.clone();
    let mut app = App {
        cfg: Arc::new(cfg),
        rt,
        proxy,
        window: None,
        gpu: None,
        egui_ctx: egui::Context::default(),
        egui_state: None,
        screen: Screen::Hosts,
        hosts: HostList::default(),
        _browser: browser,
        session: None,
        target: None,
        frame_slot: Arc::default(),
        extra: None,
        extra_slot: Arc::default(),
        shown: None,
        pin_input: String::new(),
        addr_input: String::new(),
        show_stats: false,
        relative: false,
        sharp: false,
        mods: ModifiersState::empty(),
        held: HashSet::new(),
        cursor: (0.0, 0.0),
    };
    if let Some(t) = connect {
        let pin = app.cfg.pin.clone();
        app.start_connect(t, pin);
    }
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl App {
    fn start_connect(&mut self, target: String, pin: Option<String>) {
        self.screen = Screen::Connecting(target.clone());
        self.target = Some(target.clone());
        let (cfg, proxy, slot) = (self.cfg.clone(), self.proxy.clone(), self.frame_slot.clone());
        let size = self.window.as_ref().map(|w| w.inner_size());
        self.rt.spawn(async move {
            let r = connect_once(&cfg, &target, pin, size, slot, proxy.clone()).await;
            match r {
                Ok(c) => {
                    let _ = proxy.send_event(UserEvent::Connected(c));
                }
                Err(e) if matches!(e.downcast_ref::<ClientAuthError>(), Some(ClientAuthError::PinNeeded)) => {
                    let _ = proxy.send_event(UserEvent::NeedPin(target));
                }
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::Failed(format!("{e:#}")));
                }
            }
        });
    }

    /// Reconnects with backoff (250 ms doubling to 2 s) for up to 60 s.
    fn start_reconnect(&mut self, target: String) {
        self.screen = Screen::Reconnecting { target: target.clone(), since: std::time::Instant::now() };
        let (cfg, proxy, slot) = (self.cfg.clone(), self.proxy.clone(), self.frame_slot.clone());
        let size = self.window.as_ref().map(|w| w.inner_size());
        self.rt.spawn(async move {
            let start = std::time::Instant::now();
            let mut wait = Duration::from_millis(250);
            loop {
                tokio::time::sleep(wait).await;
                match connect_once(&cfg, &target, None, size, slot.clone(), proxy.clone()).await {
                    Ok(c) => {
                        let _ = proxy.send_event(UserEvent::Connected(c));
                        return;
                    }
                    Err(e) if start.elapsed() > Duration::from_secs(60) => {
                        let _ = proxy.send_event(UserEvent::Failed(format!("Reconnecting gave up: {e:#}")));
                        return;
                    }
                    Err(e) => tracing::info!("reconnect failed: {e:#}"),
                }
                wait = (wait * 2).min(Duration::from_secs(2));
            }
        });
    }

    fn set_relative(&mut self, on: bool) {
        self.relative = on;
        if let Some(w) = &self.window {
            if on {
                let ok =
                    w.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
                if ok.is_err() {
                    tracing::warn!("pointer lock unavailable");
                }
                w.set_cursor_visible(false);
            } else {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
                w.set_cursor_visible(true);
            }
            self.update_title();
        }
    }

    fn update_title(&self) {
        let Some(w) = &self.window else { return };
        let t = match (&self.screen, &self.session) {
            (Screen::Session, Some(s)) => {
                let path = s.shared.lock().expect("lock").stats.path.clone();
                let lock = if self.relative { " · mouse locked (Ctrl+Alt+Shift+M)" } else { "" };
                format!("{} — {path}{lock}", s.host_name)
            }
            (Screen::Reconnecting { target, .. }, _) => format!("{target} — Reconnecting"),
            (Screen::Connecting(t), _) => format!("{t} — Connecting"),
            _ => "QuickRemote".into(),
        };
        w.set_title(&t);
    }

    fn release_all(&mut self) {
        if let Some(s) = &self.session {
            s.input.send(InputEvent::ReleaseAll);
        }
        self.held.clear();
    }

    /// Maps window coordinates to stream pixels (None outside the picture).
    fn to_stream(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let gpu = self.gpu.as_ref()?;
        let (pw, ph) = gpu.video.size()?;
        let (ox, oy, w, h) = fit(gpu.config.width, gpu.config.height, pw, ph);
        let sx = (x as f32 - ox) / w * pw as f32;
        let sy = (y as f32 - oy) / h * ph as f32;
        if sx < 0.0 || sy < 0.0 || sx >= pw as f32 || sy >= ph as f32 {
            return None;
        }
        Some((sx as u16, sy as u16))
    }

    fn hotkey(&mut self, k: KeyCode, el: &ActiveEventLoop) -> bool {
        if self.mods != HOTKEY {
            return false;
        }
        match k {
            KeyCode::KeyM => self.set_relative(!self.relative),
            KeyCode::KeyS => self.show_stats = !self.show_stats,
            KeyCode::KeyF => {
                if let Some(w) = &self.window {
                    w.set_fullscreen(if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) });
                }
            }
            KeyCode::KeyT => {
                self.sharp = !self.sharp;
                if let Some(s) = &self.session {
                    let modes = if self.sharp { qr_proto::msg::mode::SHARP_TEXT } else { 0 };
                    (s.ctl)(qr_proto::Msg::SetMode { modes });
                }
            }
            // Next monitor in this window.
            KeyCode::KeyN => {
                if let Some(s) = &self.session {
                    let (mons, cur) = {
                        let sh = s.shared.lock().expect("lock");
                        (sh.monitors.clone(), sh.stats.monitor)
                    };
                    if let Some(monitor) = next_monitor(&mons, cur).filter(|m| *m != cur) {
                        (s.ctl)(qr_proto::Msg::SelectMonitor { stream: 0, monitor });
                    }
                }
            }
            // A second window on the next monitor (again: close it).
            KeyCode::KeyW => {
                if self.extra.is_some() {
                    self.close_extra();
                } else {
                    self.open_extra(el);
                }
            }
            KeyCode::KeyQ => {
                if let Some(s) = self.session.take() {
                    self.release_all();
                    self.rt.block_on(session::bye(&s));
                }
                self.set_relative(false);
                self.screen = Screen::Hosts;
                self.update_title();
                if self.cfg.connect.is_some() {
                    el.exit();
                }
            }
            _ => return false,
        }
        true
    }

    fn open_extra(&mut self, el: &ActiveEventLoop) {
        let (Some(s), Some(gpu)) = (&self.session, &self.gpu) else { return };
        let (mons, cur) = {
            let sh = s.shared.lock().expect("lock");
            (sh.monitors.clone(), sh.stats.monitor)
        };
        let Some(monitor) = next_monitor(&mons, cur) else { return };
        let attrs = Window::default_attributes()
            .with_title(format!("QuickRemote — monitor {monitor}"))
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 540.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::warn!("can't open a second window: {e}");
                return;
            }
        };
        let surface = match gpu.instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("can't render a second window: {e}");
                return;
            }
        };
        let size = window.inner_size();
        let config =
            wgpu::SurfaceConfiguration { width: size.width.max(1), height: size.height.max(1), ..gpu.config.clone() };
        surface.configure(&gpu.device, &config);
        let video = VideoRenderer::new(&gpu.device, config.format);
        (s.ctl)(qr_proto::Msg::SelectMonitor { stream: EXTRA_STREAM, monitor });
        self.extra = Some(ExtraWindow { window, surface, config, video });
    }

    fn close_extra(&mut self) {
        if self.extra.take().is_some()
            && let Some(s) = &self.session
        {
            (s.ctl)(qr_proto::Msg::StreamStop { stream: EXTRA_STREAM });
        }
    }

    fn extra_window_event(&mut self, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => self.close_extra(),
            WindowEvent::Resized(size) => {
                let (Some(x), Some(gpu)) = (self.extra.as_mut(), &self.gpu) else { return };
                x.config.width = size.width.max(1);
                x.config.height = size.height.max(1);
                x.surface.configure(&gpu.device, &x.config);
                if let Some(s) = &self.session {
                    let (width, height) = (size.width as u16, size.height as u16);
                    (s.ctl)(qr_proto::Msg::Resize { stream: EXTRA_STREAM, width, height, refresh: 0 });
                }
            }
            WindowEvent::RedrawRequested => self.redraw_extra(),
            // Pointer on this window lands on its monitor.
            WindowEvent::CursorMoved { position, .. } => {
                let (Some(x), Some(s)) = (&self.extra, &self.session) else { return };
                let Some((pw, ph)) = x.video.size() else { return };
                let (ox, oy, w, h) = fit(x.config.width, x.config.height, pw, ph);
                let sx = (position.x as f32 - ox) / w * pw as f32;
                let sy = (position.y as f32 - oy) / h * ph as f32;
                if sx >= 0.0 && sy >= 0.0 && sx < pw as f32 && sy < ph as f32 {
                    s.input.abs(EXTRA_STREAM, sx as u16, sy as u16);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    _ => return,
                };
                if let Some(s) = &self.session {
                    s.input.send(InputEvent::Button { button: b, down: state == ElementState::Pressed });
                }
            }
            _ => {}
        }
    }

    fn redraw_extra(&mut self) {
        let (Some(x), Some(gpu)) = (self.extra.as_mut(), &self.gpu) else { return };
        if let Some(f) = self.extra_slot.lock().expect("lock").take() {
            x.video.upload(&gpu.device, &gpu.queue, &f);
        }
        let frame = match x.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                x.surface.configure(&gpu.device, &x.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("extra video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            x.video.draw(&gpu.queue, &mut pass, x.config.width, x.config.height);
        }
        gpu.queue.submit([enc.finish()]);
        x.window.pre_present_notify();
        gpu.queue.present(frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Option<(String, Option<String>)> {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let mut connect = None;
        match self.screen.clone() {
            Screen::Session => {
                if self.show_stats
                    && let Some(s) = &self.session
                {
                    let st = s.shared.lock().expect("lock").stats.clone();
                    egui::Area::new("stats".into()).fixed_pos([12.0, 12.0]).show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.monospace(format!(
                                "{}×{} · {:.0} fps · {} ({})\n{} kbit/s · loss {:.1}% · FEC {} · lost {}\n{} · RTT {:.1} ms · parity {}%\ncap {:.1}  enc {:.1}  net {:.1}  dec {:.1}  pres {:.1}  = {:.1} ms",
                                st.width, st.height, st.fps, st.codec, st.decoder, st.kbps, st.loss_pct,
                                st.fec_repaired, st.frames_lost, st.path, st.rtt_ms, st.parity_pct, st.capture_ms,
                                st.encode_ms, st.network_ms, st.decode_ms, st.present_ms, st.total_ms
                            ));
                        });
                    });
                }
            }
            Screen::Hosts => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("QuickRemote");
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("On this network").strong());
                    let ks = self.cfg.keystore.lock().expect("lock");
                    let mut found: Vec<&HostInfo> = self.hosts.0.values().collect();
                    found.sort_by(|a, b| a.name.cmp(&b.name));
                    if found.is_empty() {
                        ui.weak("Looking for hosts…");
                    }
                    for h in found {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  ·  {}", h.name, h.os));
                            let paired = ks.by_host_id(&h.host_id).is_some();
                            if ui.button(if paired { "Connect" } else { "Pair…" }).clicked() {
                                connect = Some((h.host_id.clone(), None));
                            }
                        });
                    }
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("Paired").strong());
                    for p in ks.peers.iter().filter(|p| !p.host_id.is_empty()) {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  ·  {}", p.name, p.host_id));
                            if ui.button("Connect").clicked() {
                                connect = Some((p.host_id.clone(), None));
                            }
                        });
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.label("Address or host ID");
                        ui.text_edit_singleline(&mut self.addr_input);
                        if ui.button("Connect").clicked() && !self.addr_input.trim().is_empty() {
                            connect = Some((self.addr_input.trim().to_string(), None));
                        }
                    });
                });
            }
            Screen::Pin { target } => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Pair with this host");
                    ui.label("Enter the 6-digit PIN shown on the host.");
                    let r = ui.text_edit_singleline(&mut self.pin_input);
                    let go = ui.button("Pair").clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go && self.pin_input.trim().len() == 6 {
                        connect = Some((target.clone(), Some(self.pin_input.trim().to_string())));
                        self.pin_input.clear();
                    }
                    if ui.button("Cancel").clicked() {
                        self.screen = Screen::Hosts;
                    }
                });
            }
            Screen::Connecting(t) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading(format!("Connecting to {t}…"));
                });
            }
            Screen::Reconnecting { target, since } => {
                egui::Area::new("reconnect".into()).fixed_pos([12.0, 12.0]).show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(format!(
                            "Connection to {target} lost. Reconnecting… ({} s)",
                            since.elapsed().as_secs()
                        ));
                    });
                });
            }
            Screen::Error(e) => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Couldn't connect");
                    ui.label(e);
                    if ui.button("Back").clicked() {
                        self.screen = Screen::Hosts;
                    }
                });
            }
        }
        connect
    }

    fn redraw(&mut self) {
        let (Some(window), Some(gpu)) = (self.window.clone(), self.gpu.as_mut()) else {
            return;
        };
        let new_frame = self.frame_slot.lock().expect("lock").take();
        if let Some((f, t)) = &new_frame {
            if gpu.video.size() != Some((f.width, f.height)) {
                tracing::info!(
                    picture = ?(f.width, f.height),
                    format = ?f.format,
                    strides = ?f.strides,
                    surface = ?(gpu.config.width, gpu.config.height),
                    scale = window.scale_factor(),
                    "video size"
                );
            }
            if let Ok(p) = std::env::var("QR_DUMP_FRAME")
                && !std::path::Path::new(&p).exists()
            {
                let _ = std::fs::write(&p, &f.data);
            }
            gpu.video.upload(&gpu.device, &gpu.queue, f);
            self.shown = Some(*t);
        }
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.screen == Screen::Session || matches!(self.screen, Screen::Reconnecting { .. }) {
                gpu.video.draw(&gpu.queue, &mut pass, gpu.config.width, gpu.config.height);
            }
        }
        // egui on top.
        let raw = self.egui_state.as_mut().expect("egui").take_egui_input(&window);
        let ctx = self.egui_ctx.clone();
        let mut connect = None;
        let out = ctx.run_ui(raw, |ui| connect = self.ui(ui));
        let gpu = self.gpu.as_mut().expect("gpu");
        self.egui_state.as_mut().expect("egui").handle_platform_output(&window, out.platform_output);
        let tris = ctx.tessellate(out.shapes, out.pixels_per_point);
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                gpu.egui.update_texture(&gpu.device, &gpu.queue, *id, d);
            }
        }
        let sd = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gpu.config.width, gpu.config.height],
            pixels_per_point: out.pixels_per_point,
        };
        gpu.egui.update_buffers(&gpu.device, &gpu.queue, &mut enc, &tris, &sd);
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            gpu.egui.render(&mut pass, &tris, &sd);
        }
        let mut deltas = out.textures_delta;
        for id in &deltas.free {
            gpu.egui.free_texture(id);
        }
        // All applied above; egui insists on an explicit clear.
        deltas.clear();
        gpu.queue.submit([enc.finish()]);
        window.pre_present_notify();
        gpu.queue.present(frame);
        if let (Some(_), Some(t), Some(s)) = (&new_frame, &self.shown, &self.session) {
            s.shared.lock().expect("lock").presented(t, now_us() as i64);
        }
        if let Some((target, pin)) = connect {
            self.start_connect(target, pin);
        }
        // UI screens animate (spinners, timers); the session view redraws
        // when frames arrive.
        if self.screen != Screen::Session || self.show_stats {
            let w = window.clone();
            self.rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                w.request_redraw();
            });
        }
    }
}

async fn connect_once(
    cfg: &Config,
    target: &str,
    pin: Option<String>,
    size: Option<winit::dpi::PhysicalSize<u32>>,
    slot: Arc<Mutex<Option<(DecodedFrame, FrameTimes)>>>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<Arc<Connected>> {
    let (target, expect_fp) = {
        let ks = cfg.keystore.lock().expect("lock").peers.clone();
        crate::resolve(target, &KeyStore::from_peers(ks), cfg.rendezvous.as_deref()).await?
    };
    let p2 = proxy.clone();
    let sink: session::FrameSink = Arc::new(move |f, t| {
        *slot.lock().expect("lock") = Some((f, t));
        let _ = p2.send_event(UserEvent::Frame);
    });
    let want = size.map(|s| (s.width as u16, s.height as u16, 0)).unwrap_or((0, 0, 0));
    let opts = ConnectOpts {
        target,
        expect_fp,
        pin,
        name: cfg.name.clone(),
        want,
        decoders: cfg.decoders.clone(),
        decoder_override: cfg.decoder_override.clone(),
        keep_samples: false,
        audio_out: Some(qr_media::audio::AudioSink::Default),
        mic: None,
        recv_dir: std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default().join("Downloads"),
        clipboard: std::env::var("DISPLAY").ok(),
    };
    let c = Arc::new(session::connect(&cfg.id, &cfg.keystore, opts, sink).await?);
    let mut ended = c.ended.clone();
    tokio::spawn(async move {
        if ended.changed().await.is_ok() {
            let reason = ended.borrow().clone().unwrap_or_default();
            let _ = proxy.send_event(UserEvent::Ended(reason));
        }
    });
    Ok(c)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("QuickRemote")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(window.clone())));
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .expect("no GPU adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");
        let caps = surface.get_capabilities(&adapter);
        // The shader outputs gamma-encoded values: use a non-sRGB format.
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        tracing::info!(adapter = %adapter.get_info().name, ?present_mode, ?format, "renderer");
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let video = VideoRenderer::new(&device, format);
        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        ));
        #[cfg(windows)]
        {
            let p = self.proxy.clone();
            if let Err(e) = crate::pen::hook(&window, move |s| {
                let _ = p.send_event(UserEvent::Pen(s));
            }) {
                tracing::warn!("pen input is off: {e:#}");
            }
        }
        self.gpu = Some(Gpu { instance, surface, device, queue, config, video, egui });
        self.window = Some(window.clone());
        window.request_redraw();
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            #[cfg(windows)]
            UserEvent::Pen(p) => {
                // Window pixels to stream pixels × 16 (sub-pixel).
                if let (Some(gpu), Some(s)) = (self.gpu.as_ref(), &self.session)
                    && let Some((pw, ph)) = gpu.video.size()
                {
                    let (ox, oy, w, h) = fit(gpu.config.width, gpu.config.height, pw, ph);
                    let sx = (p.x as f32 - ox) / w * pw as f32;
                    let sy = (p.y as f32 - oy) / h * ph as f32;
                    if sx >= 0.0 && sy >= 0.0 && sx < pw as f32 && sy < ph as f32 {
                        s.input.send(InputEvent::Pen {
                            stream: 0,
                            x: (sx * 16.0) as u32,
                            y: (sy * 16.0) as u32,
                            pressure: p.pressure,
                            tilt_x: p.tilt_x,
                            tilt_y: p.tilt_y,
                            flags: p.flags,
                        });
                    }
                }
                return;
            }
            UserEvent::ExtraFrame => {
                if let Some(x) = &self.extra {
                    x.window.request_redraw();
                }
                return;
            }
            UserEvent::Discovery(d) => self.hosts.apply(d),
            UserEvent::Connected(c) => {
                crate::start_pads(&c);
                let (slot, proxy) = (self.extra_slot.clone(), self.proxy.clone());
                *c.extra_sink.lock().expect("lock") = Some(Arc::new(move |stream, f| {
                    if stream == EXTRA_STREAM {
                        *slot.lock().expect("lock") = Some(f);
                        let _ = proxy.send_event(UserEvent::ExtraFrame);
                    }
                }));
                self.session = Some(c);
                self.screen = Screen::Session;
                self.update_title();
            }
            UserEvent::NeedPin(target) => self.screen = Screen::Pin { target },
            UserEvent::Failed(e) => self.screen = Screen::Error(e),
            UserEvent::Ended(reason) => {
                self.session = None;
                self.extra = None;
                self.held.clear();
                if reason == "bye" || self.screen != Screen::Session {
                    return;
                }
                tracing::info!("session lost: {reason}");
                match self.target.clone() {
                    Some(t) => self.start_reconnect(t),
                    None => self.screen = Screen::Error(reason),
                }
                let _ = el;
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, ev: DeviceEvent) {
        if let (DeviceEvent::MouseMotion { delta }, true, Some(s)) = (ev, self.relative, &self.session) {
            s.input.rel(delta.0, delta.1);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        if self.extra.as_ref().is_some_and(|x| x.window.id() == id) {
            self.extra_window_event(ev);
            return;
        }
        let in_session = self.screen == Screen::Session && self.session.is_some();
        if let (Some(st), Some(w)) = (self.egui_state.as_mut(), &self.window)
            && !in_session
        {
            let r = st.on_window_event(w, &ev);
            if r.repaint {
                w.request_redraw();
            }
        }
        match ev {
            WindowEvent::CloseRequested => {
                if let Some(s) = self.session.take() {
                    self.rt.block_on(session::bye(&s));
                }
                el.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gpu.as_mut() {
                    g.config.width = size.width.max(1);
                    g.config.height = size.height.max(1);
                    g.surface.configure(&g.device, &g.config);
                }
                if let (Some(s), Screen::Session) = (&self.session, &self.screen) {
                    let c = s.control.clone();
                    let (w, h) = (size.width as u16, size.height as u16);
                    self.rt.spawn(async move {
                        let m = qr_proto::Msg::Resize { stream: 0, width: w, height: h, refresh: 0 };
                        let _ = c.lock().await.send(&m).await;
                    });
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::DroppedFile(path) => {
                if let Some(s) = &self.session {
                    let (files, ctl) = (s.files.clone(), s.ctl.clone());
                    self.rt.spawn(async move {
                        if let Err(e) = files.offer(&path, &ctl).await {
                            tracing::warn!("can't send {}: {e:#}", path.display());
                        }
                    });
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(false) => {
                self.release_all();
                if self.relative {
                    self.set_relative(false);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if in_session => {
                let PhysicalKey::Code(k) = event.physical_key else {
                    return;
                };
                if event.state == ElementState::Pressed && self.hotkey(k, el) {
                    return;
                }
                let Some(code) = keymap::evdev(k) else { return };
                let down = event.state == ElementState::Pressed;
                if down {
                    self.held.insert(code);
                } else if !self.held.remove(&code) {
                    return;
                }
                if let Some(s) = &self.session {
                    s.input.send(InputEvent::Key { code, down });
                }
            }
            WindowEvent::CursorMoved { position, .. } if in_session => {
                self.cursor = (position.x, position.y);
                if !self.relative
                    && let (Some((x, y)), Some(s)) = (self.to_stream(position.x, position.y), &self.session)
                {
                    s.input.abs(0, x, y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } if in_session => {
                let b = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    MouseButton::Back => 4,
                    MouseButton::Forward => 5,
                    MouseButton::Other(_) => return,
                };
                if let Some(s) = &self.session {
                    s.input.send(InputEvent::Button { button: b, down: state == ElementState::Pressed });
                }
            }
            WindowEvent::MouseWheel { delta, .. } if in_session => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i16, (y * 120.0) as i16),
                    MouseScrollDelta::PixelDelta(p) => ((p.x * 2.0) as i16, (p.y * 2.0) as i16),
                };
                if let Some(s) = &self.session {
                    s.input.send(InputEvent::Wheel { dx, dy });
                }
            }
            _ => {}
        }
    }
}
