//! 3D draped terrain and the 2D hillshade/slope overlay. Both use terrain.wgsl.

use crate::pool::TilePool;
use crate::{FORMAT, TileKey};
use smf_geo::{EARTH_CIRCUMFERENCE_M, MapView, TileId, mercator_to_lnglat};
use smf_terrain::{Camera3D, Chunk, TERRAIN_MAX_ZOOM, tile_bounds_mercator};
use wgpu::util::DeviceExt;

pub const GRID: u32 = 64;
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// User-facing shading options shared by 2D and 3D.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadeParams {
    pub hillshade: bool,
    pub hill_strength: f32,
    pub slope: bool,
    pub slope_alpha: f32,
    /// false: degree classes (27/30/35/40); true: percent-grade classes for fire behaviour.
    pub slope_fire_preset: bool,
    pub sun_azimuth_deg: f32,
    pub sun_altitude_deg: f32,
    /// Vertical exaggeration for 3D geometry and lighting (slope classes stay true).
    pub exaggeration: f32,
}

impl Default for ShadeParams {
    fn default() -> Self {
        Self {
            hillshade: false,
            hill_strength: 0.7,
            slope: false,
            slope_alpha: 0.55,
            slope_fire_preset: false,
            sun_azimuth_deg: 315.0,
            sun_altitude_deg: 40.0,
            exaggeration: 1.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    viewport: [f32; 4],
    shade: [f32; 4],
    flags: [f32; 4],
    fog: [f32; 4],
    sky: [f32; 4],
    extra: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GridVertex {
    /// u, v, skirt flag
    p: [f32; 3],
}

/// Per-chunk instance. `tile` = SW corner relative to the eye (x, y), size, real metres per height texel.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ChunkInstance {
    pub tile: [f32; 4],
    /// u0, v0, scale, layer (negative = no imagery yet)
    pub img: [f32; 4],
    /// u0, v0, scale, layer (negative = no heights yet)
    pub hgt: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShadeVertex {
    pub pos: [f32; 2],
    pub h_uv: [f32; 2],
    pub layer: f32,
    pub texel_m: f32,
}

/// A draped overlay layer: the base chunks with this layer's imagery, drawn with opacity.
pub struct OverlayFrame {
    pub instances: Vec<ChunkInstance>,
    pub opacity: f32,
}

pub struct TerrainFrame {
    pub instances: Vec<ChunkInstance>,
    pub overlays: Vec<OverlayFrame>,
    pub view_proj: [[f32; 4]; 4],
    pub k: f32,
    pub eye_z: f32,
    pub fog: [f32; 2],
}

pub struct Terrain3dPlan {
    pub frame: TerrainFrame,
    pub imagery_requests: Vec<(TileKey, f32)>,
    pub height_requests: Vec<(TileKey, f32)>,
}

pub struct TerrainGpu {
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    terrain_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    shade_pipeline: wgpu::RenderPipeline,
    grid_vb: wgpu::Buffer,
    grid_ib: wgpu::Buffer,
    index_count: u32,
    instances: wgpu::Buffer,
    instance_capacity: u64,
    shade_vb: wgpu::Buffer,
    shade_capacity: u64,
    depth: Option<(wgpu::Texture, wgpu::TextureView, [u32; 2])>,
}

fn build_grid() -> (Vec<GridVertex>, Vec<u32>) {
    let n = GRID + 1;
    let mut v = Vec::new();
    for j in 0..n {
        for i in 0..n {
            v.push(GridVertex {
                p: [i as f32 / GRID as f32, j as f32 / GRID as f32, 0.0],
            });
        }
    }
    let mut idx = Vec::new();
    for j in 0..GRID {
        for i in 0..GRID {
            let a = j * n + i;
            let (b, c, d) = (a + 1, a + n, a + n + 1);
            idx.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }
    // Skirts: a copy of each edge vertex flagged for the shader to drop, joined by a strip.
    let mut skirt = |edge: Vec<u32>| {
        let base = v.len() as u32;
        for &e in &edge {
            let mut s = v[e as usize];
            s.p[2] = 1.0;
            v.push(s);
        }
        for k in 0..edge.len() as u32 - 1 {
            let (a, b) = (edge[k as usize], edge[k as usize + 1]);
            let (sa, sb) = (base + k, base + k + 1);
            idx.extend_from_slice(&[a, b, sb, a, sb, sa]);
        }
    };
    skirt((0..n).collect());
    skirt((0..n).map(|i| GRID * n + i).collect());
    skirt((0..n).map(|j| j * n).collect());
    skirt((0..n).map(|j| j * n + GRID).collect());
    (v, idx)
}

impl TerrainGpu {
    pub fn new(
        device: &wgpu::Device,
        imagery: &TilePool,
        heights: &TilePool,
        sampler: &wgpu::Sampler,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terrain"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/terrain.wgsl").into()),
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let both = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let tex = |binding, vis, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: vis,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: both,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex(1, wgpu::ShaderStages::FRAGMENT, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // R32Float is not filterable; the shader bilinear-filters with textureLoad.
                tex(3, both, false),
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(imagery.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(heights.view()),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terrain layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let grid_attrs = wgpu::vertex_attr_array![0 => Float32x3];
        let inst_attrs = wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4, 3 => Float32x4];
        let make_terrain_pipeline =
            |label: &str,
             fs_entry: &str,
             blend: Option<wgpu::BlendState>,
             depth_write: bool,
             depth_compare: wgpu::CompareFunction| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_terrain"),
                        compilation_options: Default::default(),
                        buffers: &[
                            Some(wgpu::VertexBufferLayout {
                                array_stride: std::mem::size_of::<GridVertex>() as u64,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &grid_attrs,
                            }),
                            Some(wgpu::VertexBufferLayout {
                                array_stride: std::mem::size_of::<ChunkInstance>() as u64,
                                step_mode: wgpu::VertexStepMode::Instance,
                                attributes: &inst_attrs,
                            }),
                        ],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(fs_entry),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: FORMAT,
                            blend,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    // Skirts are single-sided strips and winding flips with mirrored views, so draw both sides.
                    primitive: wgpu::PrimitiveState {
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        depth_write_enabled: Some(depth_write),
                        depth_compare: Some(depth_compare),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            };
        // Reversed-Z: near = 1, far = 0, so the base test is Greater and the clear is 0.
        let terrain_pipeline = make_terrain_pipeline(
            "terrain",
            "fs_terrain",
            None,
            true,
            wgpu::CompareFunction::Greater,
        );
        // Overlays re-draw identical geometry, so they pass on equal depth and never write it.
        let overlay_pipeline = make_terrain_pipeline(
            "terrain overlay",
            "fs_overlay",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            false,
            wgpu::CompareFunction::GreaterEqual,
        );

        let shade_attrs =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32];
        let shade_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shade2d"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_shade"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ShadeVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &shade_attrs,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_shade"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let (gv, gi) = build_grid();
        let grid_vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("grid vb"),
            contents: bytemuck::cast_slice(&gv),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let grid_ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("grid ib"),
            contents: bytemuck::cast_slice(&gi),
            usage: wgpu::BufferUsages::INDEX,
        });
        let instance_capacity = 1024;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("chunk instances"),
            size: instance_capacity * std::mem::size_of::<ChunkInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shade_capacity = 4096;
        let shade_vb = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shade vb"),
            size: shade_capacity * std::mem::size_of::<ShadeVertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            globals,
            bind_group,
            terrain_pipeline,
            overlay_pipeline,
            shade_pipeline,
            grid_vb,
            grid_ib,
            index_count: gi.len() as u32,
            instances,
            instance_capacity,
            shade_vb,
            shade_capacity,
            depth: None,
        }
    }

    pub fn ensure_depth(&mut self, device: &wgpu::Device, size: [u32; 2]) {
        if self.depth.as_ref().is_some_and(|d| d.2 == size) {
            return;
        }
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain depth"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.depth = Some((tex, view, size));
    }

    fn write_globals(
        &self,
        queue: &wgpu::Queue,
        size: [u32; 2],
        p: &ShadeParams,
        f: Option<&TerrainFrame>,
        overlay_opacity: f32,
    ) {
        let g = Globals {
            view_proj: f.map_or([[0.0; 4]; 4], |f| f.view_proj),
            viewport: [
                size[0] as f32,
                size[1] as f32,
                f.map_or(1.0, |f| f.k),
                f.map_or(0.0, |f| f.eye_z),
            ],
            shade: [
                p.hill_strength,
                p.slope_alpha,
                p.sun_azimuth_deg.to_radians(),
                p.sun_altitude_deg.to_radians(),
            ],
            flags: [
                p.hillshade as u32 as f32,
                p.slope as u32 as f32,
                p.slope_fire_preset as u32 as f32,
                p.exaggeration,
            ],
            fog: [
                f.map_or(1e12, |f| f.fog[0]),
                f.map_or(1e12 + 1.0, |f| f.fog[1]),
                0.0,
                0.0,
            ],
            sky: [0.62, 0.74, 0.90, 1.0],
            extra: [overlay_opacity, 0.0, 0.0, 0.0],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&g));
    }

    pub fn render_3d(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        size: [u32; 2],
        frame: &TerrainFrame,
        params: &ShadeParams,
    ) {
        self.ensure_depth(device, size);
        self.write_globals(queue, size, params, Some(frame), 1.0);
        if frame.instances.len() as u64 > self.instance_capacity {
            self.instance_capacity = (frame.instances.len() as u64).next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("chunk instances"),
                size: self.instance_capacity * std::mem::size_of::<ChunkInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&frame.instances));
        let depth = &self.depth.as_ref().expect("depth").1;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("terrain"),
        });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terrain pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.62,
                            g: 0.74,
                            b: 0.90,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.terrain_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.grid_vb.slice(..));
            pass.set_vertex_buffer(1, self.instances.slice(..));
            pass.set_index_buffer(self.grid_ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.index_count, 0, 0..frame.instances.len() as u32);
        }
        queue.submit(Some(enc.finish()));

        // Each overlay needs its own opacity uniform and instance data. Queue writes land before the
        // *next submit*, so every overlay is submitted separately to see its own values.
        for ov in &frame.overlays {
            if ov.instances.is_empty() {
                continue;
            }
            self.write_globals(queue, size, params, Some(frame), ov.opacity);
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&ov.instances));
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("terrain overlay"),
            });
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("terrain overlay pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: depth,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.overlay_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.grid_vb.slice(..));
                pass.set_vertex_buffer(1, self.instances.slice(..));
                pass.set_index_buffer(self.grid_ib.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.index_count, 0, 0..ov.instances.len() as u32);
            }
            queue.submit(Some(enc.finish()));
        }
    }

    /// Record the 2D hillshade/slope overlay into an already-open pass (after the basemap).
    pub fn draw_shade(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        size: [u32; 2],
        params: &ShadeParams,
        verts: &[ShadeVertex],
    ) {
        if verts.is_empty() || !(params.hillshade || params.slope) {
            return;
        }
        self.write_globals(queue, size, params, None, 1.0);
        if verts.len() as u64 > self.shade_capacity {
            self.shade_capacity = (verts.len() as u64).next_power_of_two();
            self.shade_vb = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shade vb"),
                size: self.shade_capacity * std::mem::size_of::<ShadeVertex>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.shade_vb, 0, bytemuck::cast_slice(verts));
        pass.set_pipeline(&self.shade_pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.shade_vb.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }
}

/// Real metres per height texel for a tile at zoom `z` centred at latitude `lat_deg`.
pub fn texel_meters(z: u8, lat_deg: f64) -> f32 {
    (EARTH_CIRCUMFERENCE_M * lat_deg.to_radians().cos() / (256.0 * (1u64 << z) as f64)) as f32
}

/// The ancestor of `id` at or above the terrain's max zoom (heights don't exist deeper).
pub fn height_id(id: TileId) -> TileId {
    let mut t = id;
    while t.z > TERRAIN_MAX_ZOOM {
        t = t.parent().expect("z>0");
    }
    t
}

/// Queue `key` plus its missing ancestors: coarse ancestors are cheap and load first,
/// giving the renderer a fallback to draw while the exact tile is in flight.
pub fn request_chain(pool: &TilePool, key: TileKey, prio: f32, out: &mut Vec<(TileKey, f32)>) {
    let mut cur = Some(key.id);
    let mut first = true;
    while let Some(id) = cur {
        let k = TileKey {
            source: key.source,
            id,
        };
        if pool.contains(&k) {
            break;
        }
        out.push((
            k,
            if first {
                prio
            } else {
                prio - 10_000.0 + id.z as f32
            },
        ));
        first = false;
        cur = id.parent();
    }
}

fn chunk_center_lat(id: TileId) -> f64 {
    let b = tile_bounds_mercator(id);
    mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0).lat
}

/// Build instance data for the chosen chunks, resolving imagery/heights (with ancestor
/// fallback) and listing the tiles that still need fetching.
pub fn plan_terrain(
    cam: &Camera3D,
    chunks: &[Chunk],
    source: u16,
    overlays: &[(u16, f32, u8)],
    imagery: &mut TilePool,
    heights: &mut TilePool,
    frame: u64,
) -> Terrain3dPlan {
    let eye = cam.eye();
    let mut instances = Vec::with_capacity(chunks.len());
    let mut overlay_frames: Vec<OverlayFrame> = overlays
        .iter()
        .map(|(_, opacity, _)| OverlayFrame {
            instances: Vec::with_capacity(chunks.len()),
            opacity: *opacity,
        })
        .collect();
    let (mut ireq, mut hreq) = (Vec::new(), Vec::new());
    for ch in chunks {
        let b = tile_bounds_mercator(ch.id);
        let size = b[2] - b[0];
        let center_dist = ((b[0] + b[2]) / 2.0 - eye.x).hypot((b[1] + b[3]) / 2.0 - eye.y);
        let prio = (center_dist / 100.0) as f32;
        let ikey = TileKey { source, id: ch.id };
        let hkey = TileKey {
            source: 0,
            id: ch.id,
        };
        let img = imagery.resolve(&ikey, frame);
        if !img.is_some_and(|r| r.is_exact()) {
            request_chain(imagery, ikey, prio, &mut ireq);
        }
        let hgt = heights.resolve(&hkey, frame);
        if !hgt.is_some_and(|r| r.zoom >= height_id(ch.id).z) {
            request_chain(
                heights,
                TileKey {
                    source: 0,
                    id: height_id(ch.id),
                },
                prio,
                &mut hreq,
            );
        }
        let texel = hgt.map_or(1.0, |r| texel_meters(r.zoom, chunk_center_lat(ch.id)));
        let pack = |r: Option<crate::pool::Resolved>| match r {
            Some(r) => [r.u0, r.v0, r.scale, r.layer as f32],
            None => [0.0, 0.0, 1.0, -1.0],
        };
        let base = ChunkInstance {
            tile: [
                (b[0] - eye.x) as f32,
                (b[1] - eye.y) as f32,
                size as f32,
                texel,
            ],
            img: pack(img),
            hgt: pack(hgt),
        };
        // Overlays share geometry and heights; only the imagery differs (and none = skip this chunk).
        for ((osrc, _, omax), frame_ov) in overlays.iter().zip(&mut overlay_frames) {
            // An overlay may stop at a coarser zoom than the basemap: never ask for tiles it lacks.
            let mut want = ch.id;
            while want.z > *omax {
                want = want.parent().expect("z>0");
            }
            let ores = imagery.resolve(
                &TileKey {
                    source: *osrc,
                    id: ch.id,
                },
                frame,
            );
            if !ores.is_some_and(|r| r.zoom >= want.z) {
                request_chain(
                    imagery,
                    TileKey {
                        source: *osrc,
                        id: want,
                    },
                    prio,
                    &mut ireq,
                );
            }
            if let Some(r) = ores {
                frame_ov.instances.push(ChunkInstance {
                    img: pack(Some(r)),
                    ..base
                });
            }
        }
        instances.push(base);
    }
    let scene_dist = cam.distance_m * cam.k();
    Terrain3dPlan {
        frame: TerrainFrame {
            instances,
            overlays: overlay_frames,
            view_proj: cam.view_proj_rel().as_mat4().to_cols_array_2d(),
            k: cam.k() as f32,
            eye_z: eye.z as f32,
            fog: [(scene_dist * 6.0) as f32, (scene_dist * 40.0) as f32],
        },
        imagery_requests: ireq,
        height_requests: hreq,
    }
}

/// Overlay vertices for every visible 2D tile that has (some) elevation data, plus height requests.
pub fn plan_shade_2d(
    view: &MapView,
    tz: u8,
    heights: &mut TilePool,
    frame: u64,
) -> (Vec<ShadeVertex>, Vec<(TileKey, f32)>) {
    let mut verts = Vec::new();
    let mut reqs = Vec::new();
    for t in view.visible_tiles(tz) {
        let id = TileId::new(tz, smf_geo::wrap_x(t.x, tz), t.y);
        let key = TileKey { source: 0, id };
        let prio = view.tile_distance(tz, t.x, t.y) as f32;
        let res = heights.resolve(&key, frame);
        if !res.is_some_and(|r| r.zoom >= height_id(id).z) {
            request_chain(
                heights,
                TileKey {
                    source: 0,
                    id: height_id(id),
                },
                prio,
                &mut reqs,
            );
        }
        let Some(r) = res else { continue };
        let c = view.tile_corners(tz, t.x, t.y);
        let texel = texel_meters(r.zoom, chunk_center_lat(id));
        let uv = |cu: f32, cv: f32| [r.u0 + cu * r.scale, r.v0 + cv * r.scale];
        let v = |i: usize, cu: f32, cv: f32| ShadeVertex {
            pos: [c[i][0] as f32, c[i][1] as f32],
            h_uv: uv(cu, cv),
            layer: r.layer as f32,
            texel_m: texel,
        };
        let (nw, ne, se, sw) = (
            v(0, 0.0, 0.0),
            v(1, 1.0, 0.0),
            v(2, 1.0, 1.0),
            v(3, 0.0, 1.0),
        );
        verts.extend_from_slice(&[nw, ne, se, nw, se, sw]);
    }
    (verts, reqs)
}
