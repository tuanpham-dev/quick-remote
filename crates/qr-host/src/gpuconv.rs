//! Zero-copy capture conversion on the GPU: the compositor copies its
//! output into a DMA-BUF we allocated (GBM), a GLES shader turns it into
//! NV12 in another DMA-BUF in the tiling the VA encoder takes, and the
//! encoder reads that directly. No pixel touches the CPU.
//!
//! (The VA encoders here take NV12 only, and the driver has no
//! video-processing entry point to convert RGB.)

use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{Context, Result, anyhow, bail};
use glow::HasContext;
use khronos_egl as egl;
use qr_media::encode::DmaFrame;

// --- GBM (libgbm), the few calls needed. ---
#[repr(C)]
struct GbmDevice {
    _p: [u8; 0],
}
#[repr(C)]
struct GbmBo {
    _p: [u8; 0],
}
#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut GbmDevice;
    fn gbm_device_destroy(d: *mut GbmDevice);
    fn gbm_bo_create(d: *mut GbmDevice, w: u32, h: u32, format: u32, flags: u32) -> *mut GbmBo;
    fn gbm_bo_create_with_modifiers(
        d: *mut GbmDevice,
        w: u32,
        h: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
    ) -> *mut GbmBo;
    fn gbm_bo_destroy(bo: *mut GbmBo);
    fn gbm_bo_get_plane_count(bo: *mut GbmBo) -> i32;
    fn gbm_bo_get_fd_for_plane(bo: *mut GbmBo, plane: i32) -> i32;
    fn gbm_bo_get_stride_for_plane(bo: *mut GbmBo, plane: i32) -> u32;
    fn gbm_bo_get_offset(bo: *mut GbmBo, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut GbmBo) -> u64;
}
const GBM_BO_USE_RENDERING: u32 = 1 << 2;

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
pub const DRM_NV12: u32 = fourcc(b"NV12");
const DRM_R8: u32 = fourcc(b"R8  ");
const DRM_GR88: u32 = fourcc(b"GR88");
/// Intel Y-tiling: what the VA encoder's DMA-BUF input takes.
pub const I915_Y_TILED: u64 = (1 << 56) | 2;

/// A GBM buffer: its planes as (fd, offset, stride).
pub struct Bo {
    bo: *mut GbmBo,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub planes: Vec<(OwnedFd, u32, u32)>,
}

impl Bo {
    fn new(dev: *mut GbmDevice, w: u32, h: u32, format: u32, modifier: Option<u64>) -> Result<Self> {
        // SAFETY: GBM calls on a live device; the bo is owned by Self.
        unsafe {
            let bo = match modifier {
                Some(m) => gbm_bo_create_with_modifiers(dev, w, h, format, &m, 1),
                None => gbm_bo_create(dev, w, h, format, GBM_BO_USE_RENDERING),
            };
            if bo.is_null() {
                bail!("GBM can't allocate {w}x{h} {:?} (modifier {modifier:x?})", fourcc_name(format));
            }
            let n = gbm_bo_get_plane_count(bo).max(1);
            let mut planes = Vec::new();
            for p in 0..n {
                let fd = gbm_bo_get_fd_for_plane(bo, p);
                if fd < 0 {
                    gbm_bo_destroy(bo);
                    bail!("GBM gave no fd for plane {p}");
                }
                planes.push((OwnedFd::from_raw_fd(fd), gbm_bo_get_offset(bo, p), gbm_bo_get_stride_for_plane(bo, p)));
            }
            Ok(Self { bo, width: w, height: h, format, modifier: gbm_bo_get_modifier(bo), planes })
        }
    }
}

impl Drop for Bo {
    fn drop(&mut self) {
        // SAFETY: our bo.
        unsafe { gbm_bo_destroy(self.bo) };
    }
}

fn fourcc_name(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| b as char).collect()
}

// --- EGL constants (EGL_EXT_image_dma_buf_import(_modifiers), GBM platform). ---
const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const PLANE_ATTRS: [[egl::Attrib; 5]; 3] = [
    // fd, offset, pitch, modifier lo, modifier hi
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
];

type ImageTargetTexture = unsafe extern "system" fn(target: u32, image: *mut c_void);

const VERTEX: &str = "#version 300 es
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}";
// BT.709, limited range (what the encoder signals for HD).
const FRAG_Y: &str = "#version 300 es
precision highp float;
uniform sampler2D src;
uniform vec2 size;
uniform float flip;
out vec4 o;
void main() {
    vec2 uv = gl_FragCoord.xy / size;
    uv.y = mix(uv.y, 1.0 - uv.y, flip);
    vec3 c = texture(src, uv).rgb;
    o = vec4(0.0627 + dot(c, vec3(0.1826, 0.6142, 0.0620)), 0.0, 0.0, 1.0);
}";
// Half size; sampling at the corner of each 2x2 block averages it.
const FRAG_UV: &str = "#version 300 es
precision highp float;
uniform sampler2D src;
uniform vec2 size;
uniform float flip;
out vec4 o;
void main() {
    vec2 uv = gl_FragCoord.xy * 2.0 / size;
    uv.y = mix(uv.y, 1.0 - uv.y, flip);
    vec3 c = texture(src, uv).rgb;
    o = vec4(0.5020 + dot(c, vec3(-0.1006, -0.3386, 0.4392)), 0.5020 + dot(c, vec3(0.4392, -0.3989, -0.0403)), 0.0, 1.0);
}";

/// One NV12 output: the buffer, its two plane render targets, and the
/// frame handed to the encoder.
struct Output {
    _bo: Bo,
    fbos: [glow::Framebuffer; 2],
    textures: [glow::Texture; 2],
    images: [egl::Image; 2],
    frame: DmaFrame,
}

/// GPU conversion for one capture size.
pub struct GpuConv {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    image_target: ImageTargetTexture,
    programs: [glow::Program; 2],
    dev: *mut GbmDevice,
    _node: std::fs::File,
    /// The buffer the compositor copies into, and its texture (freed
    /// before the GBM device, in `drop`).
    pub capture: std::mem::ManuallyDrop<Bo>,
    capture_image: egl::Image,
    capture_tex: glow::Texture,
    outputs: Vec<Output>,
    next: usize,
}

// SAFETY: the context is only made current and used on one thread at a time
// (created on one, then moved to the capture thread, which calls `bind`).
unsafe impl Send for GpuConv {}

impl GpuConv {
    /// Sets up the GPU on `node` (a DRM render node) for `w`×`h` captures
    /// in DRM `format`, with `n` NV12 outputs.
    pub fn new(node: &str, w: u32, h: u32, format: u32, n: usize) -> Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(node).with_context(|| format!("opening {node}"))?;
        // SAFETY: a valid fd for the device's lifetime (`_node` keeps it).
        let dev = unsafe { gbm_create_device(file.as_raw_fd()) };
        if dev.is_null() {
            bail!("GBM can't use {node}");
        }
        tracing::debug!("gpuconv: GBM device");
        // SAFETY: loading libEGL.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }.map_err(|e| anyhow!("loading libEGL: {e}"))?;
        // SAFETY: the GBM device outlives the display (dropped after it).
        let display = unsafe { egl.get_platform_display(EGL_PLATFORM_GBM_KHR, dev as *mut c_void, &[egl::ATTRIB_NONE]) }?;
        egl.initialize(display)?;
        tracing::debug!("gpuconv: EGL display");
        let exts = egl.query_string(Some(display), egl::EXTENSIONS)?.to_string_lossy().into_owned();
        for e in ["EGL_EXT_image_dma_buf_import_modifiers", "EGL_KHR_surfaceless_context"] {
            if !exts.contains(e) {
                bail!("EGL lacks {e}");
            }
        }
        egl.bind_api(egl::OPENGL_ES_API)?;
        let config = egl
            .choose_first_config(display, &[egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT, egl::NONE])?
            .context("no GLES 3 config")?;
        let context = egl.create_context(display, config, None, &[egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE])?;
        egl.make_current(display, None, None, Some(context))?;
        // SAFETY: loader for the current context.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| egl.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void))
        };
        let image_target: ImageTargetTexture = {
            let f = egl.get_proc_address("glEGLImageTargetTexture2DOES").context("no glEGLImageTargetTexture2DOES")?;
            // SAFETY: the documented signature.
            unsafe { std::mem::transmute::<extern "system" fn(), ImageTargetTexture>(f) }
        };
        tracing::debug!("gpuconv: GLES context");
        let programs = [program(&gl, FRAG_Y)?, program(&gl, FRAG_UV)?];
        tracing::debug!("gpuconv: shaders");
        let capture = Bo::new(dev, w, h, format, None)?;
        tracing::debug!(modifier = capture.modifier, planes = capture.planes.len(), "gpuconv: capture buffer");
        let (capture_image, capture_tex) = import(&egl, display, &gl, image_target, &capture, format, (w, h), None)?;
        tracing::debug!("gpuconv: capture imported");
        let mut me = Self {
            egl,
            display,
            context,
            gl,
            image_target,
            programs,
            dev,
            _node: file,
            capture: std::mem::ManuallyDrop::new(capture),
            capture_image,
            capture_tex,
            outputs: Vec::new(),
            next: 0,
        };
        for _ in 0..n {
            let o = me.output(w, h)?;
            me.outputs.push(o);
        }
        me.egl.make_current(me.display, None, None, None)?;
        Ok(me)
    }

    fn output(&self, w: u32, h: u32) -> Result<Output> {
        // Mesa's GBM crashes allocating NV12 with Y-tiling, so the NV12
        // layout is made by hand: one Y-tiled R8 buffer holding the luma
        // rows (padded to the 32-row tile height) and then the chroma rows,
        // each plane imported as a view at its offset.
        let y_rows = h.div_ceil(32) * 32;
        let mut bo = Bo::new(self.dev, w, y_rows + h / 2, DRM_R8, Some(I915_Y_TILED))?;
        let stride = bo.planes[0].2;
        let uv_off = y_rows * stride;
        bo.planes.push((bo.planes[0].0.try_clone()?, uv_off, stride));
        tracing::debug!(stride, uv_off, modifier = bo.modifier, "gpuconv: NV12 buffer");
        let uv_stride = stride;
        let (y_image, y_tex) = import(&self.egl, self.display, &self.gl, self.image_target, &bo, DRM_R8, (w, h), Some(0))?;
        let (uv_image, uv_tex) =
            import(&self.egl, self.display, &self.gl, self.image_target, &bo, DRM_GR88, (w / 2, h / 2), Some(1))?;
        let mut fbos = Vec::new();
        for t in [y_tex, uv_tex] {
            // SAFETY: GL calls on the current context.
            unsafe {
                let f = self.gl.create_framebuffer().map_err(|e| anyhow!(e))?;
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
                self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
                let st = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if st != glow::FRAMEBUFFER_COMPLETE {
                    bail!("NV12 plane can't be rendered to (framebuffer status {st:#x})");
                }
                fbos.push(f);
            }
        }
        let size = {
            let fd = &bo.planes[0].0;
            // SAFETY: lseek on our fd.
            let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
            if end <= 0 {
                bail!("can't size the NV12 buffer");
            }
            end as usize
        };
        let fd = bo.planes[0].0.try_clone()?;
        let frame = DmaFrame::new(
            fd,
            size,
            w,
            h,
            [bo.planes[0].1 as usize, uv_off as usize],
            [bo.planes[0].2 as i32, uv_stride as i32],
        )?;
        Ok(Output { _bo: bo, fbos: [fbos[0], fbos[1]], textures: [y_tex, uv_tex], images: [y_image, uv_image], frame })
    }

    /// Makes the context current on this thread.
    pub fn bind(&self) -> Result<()> {
        self.egl.make_current(self.display, None, None, Some(self.context))?;
        Ok(())
    }

    /// Converts what the compositor last copied into `capture`; returns the
    /// NV12 frame, or `None` if every output is still with the encoder.
    pub fn convert(&mut self, y_invert: bool) -> Option<&DmaFrame> {
        let n = self.outputs.len();
        let i = (0..n).map(|k| (self.next + k) % n).find(|&k| !self.outputs[k].frame.busy())?;
        self.next = (i + 1) % n;
        let (w, h) = (self.capture.width as f32, self.capture.height as f32);
        let o = &self.outputs[i];
        // SAFETY: GL calls on the current context.
        unsafe {
            let gl = &self.gl;
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.capture_tex));
            for (k, (vw, vh)) in [(w, h), (w / 2.0, h / 2.0)].into_iter().enumerate() {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(o.fbos[k]));
                gl.viewport(0, 0, vw as i32, vh as i32);
                gl.use_program(Some(self.programs[k]));
                gl.uniform_1_i32(gl.get_uniform_location(self.programs[k], "src").as_ref(), 0);
                gl.uniform_2_f32(gl.get_uniform_location(self.programs[k], "size").as_ref(), w, h);
                gl.uniform_1_f32(gl.get_uniform_location(self.programs[k], "flip").as_ref(), y_invert as u8 as f32);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            // The encoder reads the buffer next: let the GPU finish.
            gl.finish();
        }
        Some(&self.outputs[i].frame)
    }
}

impl Drop for GpuConv {
    fn drop(&mut self) {
        let _ = self.bind();
        // SAFETY: deleting our own GL/EGL objects on the current context.
        unsafe {
            for o in &self.outputs {
                for f in o.fbos {
                    self.gl.delete_framebuffer(f);
                }
                for t in o.textures {
                    self.gl.delete_texture(t);
                }
                for i in o.images {
                    let _ = self.egl.destroy_image(self.display, i);
                }
            }
            self.gl.delete_texture(self.capture_tex);
            let _ = self.egl.destroy_image(self.display, self.capture_image);
            for p in self.programs {
                self.gl.delete_program(p);
            }
        }
        self.outputs.clear();
        // SAFETY: dropped once, here, before the device.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.capture) };
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
        // SAFETY: the display is gone; our device.
        unsafe { gbm_device_destroy(self.dev) };
    }
}

/// An EGL image and texture for `bo` seen as `format` at `w`×`h`: all of
/// its planes (`plane` `None`), or one.
#[allow(clippy::too_many_arguments)]
fn import(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    gl: &glow::Context,
    image_target: ImageTargetTexture,
    bo: &Bo,
    format: u32,
    (w, h): (u32, u32),
    plane: Option<usize>,
) -> Result<(egl::Image, glow::Texture)> {
    let mut a: Vec<egl::Attrib> = vec![egl::WIDTH as _, w as _, egl::HEIGHT as _, h as _, EGL_LINUX_DRM_FOURCC_EXT, format as _];
    let planes: Vec<usize> = plane.map_or_else(|| (0..bo.planes.len()).collect(), |p| vec![p]);
    for (i, p) in planes.iter().enumerate() {
        let (fd, offset, stride) = &bo.planes[*p];
        let k = PLANE_ATTRS[i];
        a.extend([k[0], fd.as_raw_fd() as _, k[1], *offset as _, k[2], *stride as _]);
        a.extend([k[3], (bo.modifier & 0xffff_ffff) as _, k[4], (bo.modifier >> 32) as _]);
    }
    a.push(egl::ATTRIB_NONE);
    // SAFETY: dma-buf imports take no context and a null client buffer.
    let image = unsafe {
        egl.create_image(
            display,
            egl::Context::from_ptr(egl::NO_CONTEXT),
            EGL_LINUX_DMA_BUF_EXT,
            egl::ClientBuffer::from_ptr(std::ptr::null_mut()),
            &a,
        )
    }
    .with_context(|| format!("importing a {w}x{h} {} buffer (modifier {:#x})", fourcc_name(format), bo.modifier))?;
    // SAFETY: GL calls on the current context.
    let tex = unsafe {
        let t = gl.create_texture().map_err(|e| anyhow!(e))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        image_target(glow::TEXTURE_2D, image.as_ptr());
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        t
    };
    Ok((image, tex))
}

fn program(gl: &glow::Context, frag: &str) -> Result<glow::Program> {
    // SAFETY: GL calls on the current context.
    unsafe {
        let p = gl.create_program().map_err(|e| anyhow!(e))?;
        for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, frag)] {
            let s = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
            gl.shader_source(s, src);
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                bail!("shader: {}", gl.get_shader_info_log(s));
            }
            gl.attach_shader(p, s);
        }
        gl.link_program(p);
        if !gl.get_program_link_status(p) {
            bail!("program: {}", gl.get_program_info_log(p));
        }
        Ok(p)
    }
}
