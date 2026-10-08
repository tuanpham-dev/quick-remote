//! Video renderer: decoded YUV planes → textures → BT.709 YUV-to-RGB in a
//! shader, aspect-fitted into the window.

use qr_media::decode::{DecodedFrame, PixelFormat};

const SHADER: &str = r#"
struct U { scale: vec2<f32>, offset: vec2<f32>, mode: u32, full_range: u32, pad0: u32, pad1: u32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var ty: texture_2d<f32>;
@group(0) @binding(2) var tu: texture_2d<f32>;
@group(0) @binding(3) var tv: texture_2d<f32>;
@group(0) @binding(4) var smp: sampler;

struct V { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let x = f32(i & 1u);
    let y = f32((i >> 1u) & 1u);
    var o: V;
    o.pos = vec4<f32>((x * 2.0 - 1.0) * u.scale.x + u.offset.x, (1.0 - y * 2.0) * u.scale.y + u.offset.y, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

@fragment
fn fs(i: V) -> @location(0) vec4<f32> {
    var y = textureSample(ty, smp, i.uv).r;
    var c: vec2<f32>;
    if (u.mode == 0u) {
        c = textureSample(tu, smp, i.uv).rg;
    } else {
        c = vec2<f32>(textureSample(tu, smp, i.uv).r, textureSample(tv, smp, i.uv).r);
    }
    if (u.full_range == 0u) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = (c - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0);
    } else {
        c = c - vec2<f32>(128.0 / 255.0);
    }
    let r = y + 1.5748 * c.y;
    let g = y - 0.1873 * c.x - 0.4681 * c.y;
    let b = y + 1.8556 * c.x;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    scale: [f32; 2],
    /// The picture's centre in clip space (0, 0: the window's centre).
    offset: [f32; 2],
    mode: u32,
    full_range: u32,
    pad: [u32; 2],
}

struct Planes {
    width: u32,
    height: u32,
    format: PixelFormat,
    textures: [wgpu::Texture; 3],
    bind_group: wgpu::BindGroup,
}

pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Option<Planes>,
}

/// The picture's rectangle in window pixels: `(x, y, w, h)`.
pub fn fit(win_w: u32, win_h: u32, pic_w: u32, pic_h: u32) -> (f32, f32, f32, f32) {
    if pic_w == 0 || pic_h == 0 || win_w == 0 || win_h == 0 {
        return (0.0, 0.0, win_w as f32, win_h as f32);
    }
    let s = (win_w as f32 / pic_w as f32).min(win_h as f32 / pic_h as f32);
    let (w, h) = (pic_w as f32 * s, pic_h as f32 * s);
    ((win_w as f32 - w) / 2.0, (win_h as f32 - h) / 2.0, w, h)
}

impl VideoRenderer {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, bgl, sampler, uniforms, planes: None }
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|p| (p.width, p.height))
    }

    fn plane_dims(f: PixelFormat, w: u32, h: u32) -> [(u32, u32, wgpu::TextureFormat); 3] {
        let r8 = wgpu::TextureFormat::R8Unorm;
        match f {
            PixelFormat::Nv12 => {
                [(w, h, r8), (w.div_ceil(2), h.div_ceil(2), wgpu::TextureFormat::Rg8Unorm), (1, 1, r8)]
            }
            PixelFormat::I420 => [(w, h, r8), (w.div_ceil(2), h.div_ceil(2), r8), (w.div_ceil(2), h.div_ceil(2), r8)],
            PixelFormat::Y444 => [(w, h, r8), (w, h, r8), (w, h, r8)],
        }
    }

    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &DecodedFrame) {
        let dims = Self::plane_dims(f.format, f.width, f.height);
        let stale =
            self.planes.as_ref().is_none_or(|p| p.width != f.width || p.height != f.height || p.format != f.format);
        if stale {
            let textures = dims.map(|(w, h, format)| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("plane"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            });
            let views = textures.each_ref().map(|t| t.create_view(&Default::default()));
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("yuv"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[0]) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[1]) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&views[2]) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.planes = Some(Planes { width: f.width, height: f.height, format: f.format, textures, bind_group });
        }
        let p = self.planes.as_ref().expect("just made");
        let planes = if f.format == PixelFormat::Nv12 { 2 } else { 3 };
        for (i, &(w, h, fmt)) in dims.iter().enumerate().take(planes) {
            let bpp = if fmt == wgpu::TextureFormat::Rg8Unorm { 2 } else { 1 };
            let start = f.offsets[i];
            let len = f.strides[i] * (h as usize - 1) + w as usize * bpp;
            let Some(data) = f.data.get(start..start + len) else {
                continue;
            };
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &p.textures[i],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(f.strides[i] as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
    }

    /// Draws the picture letterboxed into a `win_w × win_h` target.
    pub fn draw(&self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass<'_>, win_w: u32, win_h: u32) {
        let Some(p) = &self.planes else { return };
        self.draw_at(queue, pass, (win_w, win_h), fit(win_w, win_h, p.width, p.height));
    }

    /// Draws the picture into `rect` (`x, y, w, h` in target pixels) of a
    /// `win` target; parts outside it are clipped.
    pub fn draw_at(
        &self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        win: (u32, u32),
        (x, y, w, h): (f32, f32, f32, f32),
    ) {
        let Some(p) = &self.planes else { return };
        let (ww, wh) = (win.0.max(1) as f32, win.1.max(1) as f32);
        let u = Uniforms {
            scale: [w / ww, h / wh],
            offset: [(x + w / 2.0) / ww * 2.0 - 1.0, 1.0 - (y + h / 2.0) / wh * 2.0],
            mode: if p.format == PixelFormat::Nv12 { 0 } else { 1 },
            full_range: 0,
            pad: [0; 2],
        };
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &p.bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders a left-black / right-white NV12 picture offscreen and reads
    /// back the target: the split must land in the middle.
    #[test]
    fn draws_whole_picture_aspect_fitted() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let fmt = wgpu::TextureFormat::Rgba8Unorm;
        let (tw, th) = (256u32, 128u32);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: fmt,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let (pw, ph) = (64u32, 32u32);
        let mut data = vec![128u8; (pw * ph * 3 / 2) as usize];
        for y in 0..ph {
            for x in 0..pw {
                data[(y * pw + x) as usize] = if x < pw / 2 { 16 } else { 235 };
            }
        }
        let frame = DecodedFrame {
            frame_id: 0,
            width: pw,
            height: ph,
            format: PixelFormat::Nv12,
            data,
            offsets: [0, (pw * ph) as usize, 0],
            strides: [pw as usize, pw as usize, 0],
            decode_us: 0,
            work_us: 0,
        };
        let mut r = VideoRenderer::new(&device, fmt);
        r.upload(&device, &queue, &frame);
        let view = target.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::RED), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            r.draw(&queue, &mut pass, tw, th);
        }
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (tw * th * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(tw * 4),
                    rows_per_image: Some(th),
                },
            },
            wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let px = buf.slice(..).get_mapped_range().unwrap().to_vec();
        let at = |x: u32, y: u32| px[((y * tw + x) * 4) as usize];
        let row: Vec<u8> = (0..tw).step_by(16).map(|x| at(x, th / 2)).collect();
        eprintln!("row: {row:?}");
        assert!(at(4, th / 2) < 20, "left is black");
        assert!(at(tw / 2 - 8, th / 2) < 20, "still black left of centre");
        assert!(at(tw / 2 + 8, th / 2) > 235, "white right of centre");
        assert!(at(tw - 4, th / 2) > 235, "right is white");
    }
}
