//! The places view's GPU scene: placed meshes drawn offscreen with a depth buffer, one directional
//! light plus ambient, into a texture the view shows through egui. Markers, labels and the gizmo stay
//! egui overlays on top of that image.
//!
//! Geometry is uploaded once per mesh (flat triangles, a normal per triangle); every frame draws the
//! requested instances, each with its own model matrix and colour. Back faces are not culled: the
//! winding of the game's meshes is not established, so both sides are lit.

use std::collections::HashMap;
use std::sync::Arc;

use wgpu::util::DeviceExt;

use crate::dtex::CpuTexture;
use crate::place_mesh::Geom;
use crate::place_tex::TextureSet;

/// The texture id of a textured mesh's untextured range (0 means "none, white", real ids are
/// 32-bit name hashes).
const PLAIN: u32 = u32::MAX;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// One mesh instance: geometry (keyed so it uploads once), column-major model matrix, colour.
pub struct Item {
    pub key: String,
    pub geom: Arc<Geom>,
    pub model: [[f32; 4]; 4],
    pub colour: [f32; 3],
}

/// One frame for the GPU: the target size in pixels, the camera and what to draw.
pub struct DrawList {
    pub size: [u32; 2],
    /// Column-major view-projection.
    pub view_proj: [[f32; 4]; 4],
    pub items: Vec<Item>,
    /// Diffuse textures for the items' materials; `None` draws everything untextured (item colour).
    pub textures: Option<Arc<TextureSet>>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    m3: [f32; 4],
    colour: [f32; 4],
}

struct GpuMesh {
    vbo: wgpu::Buffer,
    count: u32,
    /// `(first vertex, vertex count, diffuse texture hash or 0)` per material range, in order.
    ranges: Vec<(u32, u32, u32)>,
    /// Whether it was uploaded with texture assignments (a mesh first seen untextured is re-uploaded
    /// when textures arrive, and the other way round).
    textured: bool,
}

pub struct SceneGpu {
    pipeline: wgpu::RenderPipeline,
    sky_pipeline: wgpu::RenderPipeline,
    camera_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    tex_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// A 1×1 white texture: untextured ranges sample it, so one shader path serves both.
    white: wgpu::BindGroup,
    /// A 1×1 neutral grey for a textured mesh's ranges that have no texture.
    plain: wgpu::BindGroup,
    textures: HashMap<u32, wgpu::BindGroup>,
    target: Option<([u32; 2], wgpu::Texture, wgpu::TextureView, wgpu::TextureView)>,
    meshes: HashMap<String, GpuMesh>,
    /// The texture's id in egui, once registered.
    pub tex_id: Option<egui::TextureId>,
}

impl SceneGpu {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> SceneGpu {
        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("places camera"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("places camera bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("places camera bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: camera_buf.as_entire_binding() }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("places shader"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("places texture bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("places sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let white = upload_texture(device, queue, &tex_bgl, &sampler, 1, 1, &[255, 255, 255, 255]);
        let plain = upload_texture(device, queue, &tex_bgl, &sampler, 1, 1, &[150, 146, 138, 255]);
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("places layout"), bind_group_layouts: &[&bgl, &tex_bgl], push_constant_ranges: &[] });
        let vattrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 7 => Float32x2];
        let iattrs = wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("places pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[
                    wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Vertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &vattrs },
                    wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Instance>() as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &iattrs },
                ],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState { format: COLOR_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        let sky_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("places sky"), source: wgpu::ShaderSource::Wgsl(SKY_SHADER.into()),
        });
        let sky_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("places sky layout"), bind_group_layouts: &[], push_constant_ranges: &[],
        });
        let sky_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("places sky pipeline"), layout: Some(&sky_layout),
            vertex: wgpu::VertexState { module: &sky_shader, entry_point: "vs_sky", buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &sky_shader, entry_point: "fs_sky", compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: COLOR_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT, depth_write_enabled: false, depth_compare: wgpu::CompareFunction::Always,
                stencil: Default::default(), bias: Default::default(),
            }),
            multisample: Default::default(), multiview: None,
        });
        SceneGpu { pipeline, sky_pipeline, camera_buf, bind_group, tex_bgl, sampler, white, plain, textures: HashMap::new(), target: None, meshes: HashMap::new(), tex_id: None }
    }

    /// Draw `list` into the offscreen texture and submit. Returns true when the texture was
    /// (re)created, so the caller re-points the egui texture at [`SceneGpu::view`].
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, list: &DrawList) -> bool {
        let size = [list.size[0].clamp(1, 8192), list.size[1].clamp(1, 8192)];
        let recreated = self.target.as_ref().map_or(true, |t| t.0 != size);
        if recreated {
            let ext = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 };
            let color = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("places color"),
                size: ext,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: COLOR_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | if cfg!(test) { wgpu::TextureUsages::COPY_SRC } else { wgpu::TextureUsages::empty() },
                view_formats: &[],
            });
            let depth = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("places depth"),
                size: ext,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let cv = color.create_view(&Default::default());
            let dv = depth.create_view(&Default::default());
            self.target = Some((size, color, cv, dv));
        }
        // Textures: each diffuse is uploaded once (with a mip chain), the first time a draw list
        // offers it.
        if let Some(ts) = &list.textures {
            for (h, t) in &ts.tex {
                if !self.textures.contains_key(h) {
                    let bg = upload_texture(device, queue, &self.tex_bgl, &self.sampler, t.width, t.height, &t.rgba);
                    self.textures.insert(*h, bg);
                }
            }
        }
        let textured = list.textures.is_some();
        // terrain preview patches are rebuilt under a new key each time: drop the ones this frame does not use
        self.meshes.retain(|k, _| !k.starts_with("terrain|") || list.items.iter().any(|it| &it.key == k));
        for it in &list.items {
            let ranges = material_ranges(&it.geom, list.textures.as_deref());
            if self.meshes.get(&it.key).map_or(true, |m| m.textured != textured || m.ranges != ranges) {
                let verts: Vec<Vertex> = it
                    .geom
                    .tris
                    .iter()
                    .zip(&it.geom.normals)
                    .zip(&it.geom.uvs)
                    .flat_map(|((t, n), uv)| (0..3).map(move |k| Vertex { pos: t[k], normal: *n, uv: uv[k] }))
                    .collect();
                // A mesh with some textured ranges draws its untextured ones in a neutral grey (so they
                // read as "no texture", not as overexposed white); a mesh with none keeps the item
                // colour as before.
                let vbo = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("places mesh"), contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX });
                self.meshes.insert(it.key.clone(), GpuMesh { vbo, count: verts.len() as u32, ranges, textured });
            }
        }
        queue.write_buffer(&self.camera_buf, 0, bytemuck::cast_slice(&list.view_proj));
        let mut order: Vec<usize> = (0..list.items.len()).collect();
        order.sort_by(|a, b| list.items[*a].key.cmp(&list.items[*b].key));
        let instances: Vec<Instance> = order
            .iter()
            .map(|&i| {
                let it = &list.items[i];
                let c = it.colour;
                Instance { m0: it.model[0], m1: it.model[1], m2: it.model[2], m3: it.model[3], colour: [c[0], c[1], c[2], 1.0] }
            })
            .collect();
        let ibo = (!instances.is_empty()).then(|| device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("places instances"), contents: bytemuck::cast_slice(&instances), usage: wgpu::BufferUsages::VERTEX }));
        let (_, _, cv, dv) = self.target.as_ref().unwrap();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("places frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("places pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: cv,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.012, g: 0.013, b: 0.016, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: dv,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.sky_pipeline);
            pass.draw(0..3, 0..1);
            if let Some(ibo) = &ibo {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(1, ibo.slice(..));
                let mut k = 0usize;
                while k < order.len() {
                    let key = &list.items[order[k]].key;
                    let mut e = k + 1;
                    while e < order.len() && &list.items[order[e]].key == key {
                        e += 1;
                    }
                    if let Some(m) = self.meshes.get(key) {
                        pass.set_vertex_buffer(0, m.vbo.slice(..));
                        for &(first, count, tex) in &m.ranges {
                            let bg = match tex {
                                0 => &self.white,
                                PLAIN => &self.plain,
                                // A texture the scene named but that never uploaded: grey, not white.
                                t => self.textures.get(&t).unwrap_or(&self.plain),
                            };
                            pass.set_bind_group(1, bg, &[]);
                            pass.draw(first..first + count, k as u32..e as u32);
                        }
                    }
                    k = e;
                }
            }
        }
        queue.submit(std::iter::once(enc.finish()));
        recreated
    }

    /// The colour target egui shows.
    pub fn view(&self) -> Option<&wgpu::TextureView> {
        self.target.as_ref().map(|t| &t.2)
    }

    #[cfg(test)]
    pub fn readback(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Vec<u8> {
        let (size, texture, _, _) = self.target.as_ref().expect("render before readback");
        let stride = (size[0] * 4 + 255) & !255;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("places test readback"), size: (stride * size[1]) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::ImageCopyBuffer { buffer: &buffer, layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(size[1]) } },
            wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| sender.send(result).unwrap());
        device.poll(wgpu::Maintain::Wait);
        receiver.recv().unwrap().unwrap();
        let pixels = buffer.slice(..).get_mapped_range();
        pixels.chunks_exact(stride as usize).flat_map(|row| row[..size[0] as usize * 4].iter().copied()).collect()
    }
}

fn material_ranges(geometry: &Geom, textures: Option<&TextureSet>) -> Vec<(u32, u32, u32)> {
    let any = textures.map_or(false, |set| geometry.ranges.iter().any(|range| set.mat_tex.contains_key(&range.material)));
    geometry.ranges.iter().map(|range| {
        let texture = textures.and_then(|set| set.mat_tex.get(&range.material).copied());
        (range.first * 3, range.count * 3, texture.unwrap_or(if any { PLAIN } else { 0 }))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_material_resolution_changes_existing_gpu_ranges() {
        let geometry = Geom {
            tris: Vec::new(), normals: Vec::new(), uvs: Vec::new(), provenance: String::new(),
            ranges: vec![crate::place_mesh::GeomRange { first: 0, count: 1, material: 10 }, crate::place_mesh::GeomRange { first: 1, count: 2, material: 20 }],
        };
        let mut textures = TextureSet::empty("pending");
        let pending = material_ranges(&geometry, Some(&textures));
        assert_eq!(pending, vec![(0, 3, 0), (3, 6, 0)]);
        textures.mat_tex.insert(10, 100);
        let partial = material_ranges(&geometry, Some(&textures));
        assert_eq!(partial, vec![(0, 3, 100), (3, 6, PLAIN)]);
        assert_ne!(pending, partial);
        textures.mat_tex.insert(20, 200);
        let complete = material_ranges(&geometry, Some(&textures));
        assert_eq!(complete, vec![(0, 3, 100), (3, 6, 200)]);
        assert_ne!(partial, complete);
        assert_eq!(material_ranges(&geometry, None), pending);
    }
}

/// Half-size box filter of an RGBA8 image (each output texel averages up to four input texels).
fn half_size(src: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0u32; 4];
            let mut n = 0;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (sx, sy) = ((x * 2 + dx).min(w - 1), (y * 2 + dy).min(h - 1));
                let i = ((sy * w + sx) * 4) as usize;
                for k in 0..4 {
                    acc[k] += src[i + k] as u32;
                }
                n += 1;
            }
            let o = ((y * nw + x) * 4) as usize;
            for k in 0..4 {
                out[o + k] = (acc[k] / n) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// Upload straight-alpha RGBA8 as an sRGB texture with a full mip chain and wrap it in a bind group.
/// A buffer shorter than `w * h * 4` is padded with zeros rather than panicking.
fn upload_texture(device: &wgpu::Device, queue: &wgpu::Queue, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32, rgba: &[u8]) -> wgpu::BindGroup {
    let (w, h) = (w.max(1), h.max(1));
    let levels = 32 - w.max(h).leading_zeros();
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("places diffuse"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut data = rgba.to_vec();
    data.resize((w * h * 4) as usize, 0);
    let (mut lw, mut lh) = (w, h);
    for level in 0..levels {
        queue.write_texture(
            wgpu::ImageCopyTexture { texture: &tex, mip_level: level, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &data,
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(lw * 4), rows_per_image: Some(lh) },
            wgpu::Extent3d { width: lw, height: lh, depth_or_array_layers: 1 },
        );
        if level + 1 < levels {
            let (d, nw, nh) = half_size(&data, lw, lh);
            data = d;
            lw = nw;
            lh = nh;
        }
    }
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("places texture bg"),
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) }],
    })
}

// Editor daylight backdrop; room geometry naturally covers it indoors.
const SKY_SHADER: &str = r#"
struct SkyVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) height: f32,
};
@vertex fn vs_sky(@builtin(vertex_index) index: u32) -> SkyVertex {
    var corners = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: SkyVertex;
    out.clip = vec4<f32>(corners[index], 1.0, 1.0);
    out.height = corners[index].y * 0.5 + 0.5;
    return out;
}
@fragment fn fs_sky(in: SkyVertex) -> @location(0) vec4<f32> {
    let colour = mix(vec3<f32>(0.64, 0.73, 0.78), vec3<f32>(0.16, 0.36, 0.62), smoothstep(0.15, 1.0, in.height));
    return vec4<f32>(colour, 1.0);
}
"#;

const SHADER: &str = r#"
struct Camera { view_proj: mat4x4<f32> };
@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var diffuse: texture_2d<f32>;
@group(1) @binding(1) var diffuse_samp: sampler;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(7) uv: vec2<f32>,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) colour: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) colour: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    let m = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    var out: VsOut;
    out.clip = camera.view_proj * (m * vec4<f32>(in.pos, 1.0));
    out.normal = (m * vec4<f32>(in.normal, 0.0)).xyz;
    out.colour = in.colour.rgb;
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let l = normalize(vec3<f32>(0.35, 0.85, 0.4));
    let n = normalize(in.normal);
    let shade = 0.25 + 0.75 * abs(dot(n, l));
    // Untextured ranges sample a white texel, so the item colour is the whole answer for them.
    let c = textureSample(diffuse, diffuse_samp, in.uv);
    if (c.a < 0.5) {
        discard;
    }
    return vec4<f32>(in.colour * c.rgb * shade, 1.0);
}
"#;
