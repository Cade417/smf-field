//! wgpu 2D raster map renderer. Draws into its own offscreen texture so the same
//! path serves the egui panel, headless tests and (later) print output.
//! Knows nothing about egui or networking.

use smf_geo::TileId;
use std::collections::HashMap;

pub use wgpu;

pub const TILE_PX: u32 = 256;
/// 256 is the default max array layers, so this works on every backend including WebGL2.
const POOL_LAYERS: u32 = 256;
/// sRGB so the 8-bit tile bytes are decoded to linear on sample and re-encoded on write.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub source: u16,
    pub id: TileId,
}

/// One tile placed on screen. `uv` is (u0, v0, u1, v1) so an ancestor tile can
/// stand in for a missing child by drawing only the matching sub-rectangle.
#[derive(Debug, Clone, Copy)]
pub struct TileQuad {
    /// NW, NE, SE, SW in physical pixels.
    pub corners: [[f32; 2]; 4],
    pub uv: [f32; 4],
    pub layer: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    layer: u32,
}

struct Target {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
}

pub struct MapRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    pool: wgpu::Texture,
    globals: wgpu::Buffer,
    vertices: wgpu::Buffer,
    vertex_capacity: u64,
    layers: HashMap<TileKey, u32>,
    layer_owner: Vec<Option<TileKey>>,
    layer_used: Vec<u64>,
    frame: u64,
    target: Option<Target>,
    pub background: [f64; 4],
}

impl MapRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tiles"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/tiles.wgsl").into()),
        });
        let pool = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tile pool"),
            size: wgpu::Extent3d {
                width: TILE_PX,
                height: TILE_PX,
                depth_or_array_layers: POOL_LAYERS,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let pool_view = pool.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tiles bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tiles bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&pool_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tiles layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tiles"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
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
        let vertex_capacity = 4096;
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tile vertices"),
            size: vertex_capacity * std::mem::size_of::<Vertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            bind_group,
            pool,
            globals,
            vertices,
            vertex_capacity,
            layers: HashMap::new(),
            layer_owner: vec![None; POOL_LAYERS as usize],
            layer_used: vec![0; POOL_LAYERS as usize],
            frame: 0,
            target: None,
            background: [0.82, 0.84, 0.86, 1.0],
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Pool layer for a resident tile; marks it used this frame so it isn't evicted.
    pub fn layer_of(&mut self, key: &TileKey) -> Option<u32> {
        let l = *self.layers.get(key)?;
        self.layer_used[l as usize] = self.frame;
        Some(l)
    }

    pub fn resident_tiles(&self) -> usize {
        self.layers.len()
    }

    pub fn upload_tile(&mut self, key: TileKey, rgba: &[u8]) {
        assert_eq!(rgba.len(), (TILE_PX * TILE_PX * 4) as usize);
        let layer = match self.layers.get(&key) {
            Some(&l) => l,
            None => {
                // Evict the least recently drawn layer (free layers have used == 0).
                let l = (0..POOL_LAYERS as usize)
                    .min_by_key(|&i| self.layer_used[i])
                    .unwrap();
                if let Some(old) = self.layer_owner[l].take() {
                    self.layers.remove(&old);
                }
                self.layer_owner[l] = Some(key);
                self.layers.insert(key, l as u32);
                l as u32
            }
        };
        self.layer_used[layer as usize] = self.frame;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.pool,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: 0,
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TILE_PX * 4),
                rows_per_image: Some(TILE_PX),
            },
            wgpu::Extent3d {
                width: TILE_PX,
                height: TILE_PX,
                depth_or_array_layers: 1,
            },
        );
    }

    /// True when the target had to be (re)created; the host must re-register it with egui.
    pub fn ensure_target(&mut self, size: [u32; 2]) -> bool {
        let size = [size[0].max(1), size[1].max(1)];
        if self.target.as_ref().is_some_and(|t| t.size == size) {
            return false;
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("map target"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.target = Some(Target { tex, view, size });
        true
    }

    pub fn target_view(&self) -> Option<&wgpu::TextureView> {
        self.target.as_ref().map(|t| &t.view)
    }

    pub fn render(&mut self, quads: &[TileQuad]) {
        let Some(target) = &self.target else { return };
        let size = target.size;
        self.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::cast_slice(&[size[0] as f32, size[1] as f32, 0.0f32, 0.0f32]),
        );
        let mut verts: Vec<Vertex> = Vec::with_capacity(quads.len() * 6);
        for q in quads {
            let [u0, v0, u1, v1] = q.uv;
            let v = |i: usize, u: f32, vv: f32| Vertex {
                pos: q.corners[i],
                uv: [u, vv],
                layer: q.layer,
            };
            let (nw, ne, se, sw) = (v(0, u0, v0), v(1, u1, v0), v(2, u1, v1), v(3, u0, v1));
            verts.extend_from_slice(&[nw, ne, se, nw, se, sw]);
        }
        if verts.len() as u64 > self.vertex_capacity {
            self.vertex_capacity = (verts.len() as u64).next_power_of_two();
            self.vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tile vertices"),
                size: self.vertex_capacity * std::mem::size_of::<Vertex>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue
            .write_buffer(&self.vertices, 0, bytemuck::cast_slice(&verts));

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("map") });
        {
            let [r, g, b, a] = self.background;
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("map pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertices.slice(..));
            pass.draw(0..verts.len() as u32, 0..1);
        }
        self.queue.submit(Some(enc.finish()));
    }

    /// Copy the target back to the CPU as tightly packed RGBA8 (tests / print).
    pub fn read_pixels(&self) -> Option<Vec<u8>> {
        let target = self.target.as_ref()?;
        let [w, h] = target.size;
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(enc.finish()));
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        let data = slice.get_mapped_range().ok()?;
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            out.extend_from_slice(&data[(y * row) as usize..(y * row + w * 4) as usize]);
        }
        Some(out)
    }
}

/// Build the quads for one frame from the view. For each visible tile: draw it if
/// resident, otherwise the nearest resident ancestor's matching sub-rectangle, so
/// panning/zooming never shows holes. Also returns tiles to request, best first.
pub struct FramePlan {
    pub quads: Vec<TileQuad>,
    /// (key, priority) with lower priority loading first.
    pub requests: Vec<(TileKey, f32)>,
}

pub fn plan_frame(
    r: &mut MapRenderer,
    view: &smf_geo::MapView,
    source: u16,
    max_zoom: u8,
) -> FramePlan {
    let tz = (view.zoom.round().max(0.0) as u8).min(max_zoom);
    let mut quads = Vec::new();
    let mut requests = Vec::new();
    let mut tiles = view.visible_tiles(tz);
    // Draw ancestors first-class: sort so nearest-to-centre tiles are requested first.
    tiles.sort_by(|a, b| {
        view.tile_distance(tz, a.x, a.y)
            .total_cmp(&view.tile_distance(tz, b.x, b.y))
    });
    for t in tiles {
        let wx = smf_geo::wrap_x(t.x, tz);
        let id = TileId::new(tz, wx, t.y);
        let dist = view.tile_distance(tz, t.x, t.y) as f32;
        let corners32 = |c: [[f64; 2]; 4]| c.map(|p| [p[0] as f32, p[1] as f32]);
        let corners = corners32(view.tile_corners(tz, t.x, t.y));
        if let Some(layer) = r.layer_of(&TileKey { source, id }) {
            quads.push(TileQuad {
                corners,
                uv: [0.0, 0.0, 1.0, 1.0],
                layer,
            });
            continue;
        }
        requests.push((TileKey { source, id }, dist));
        // Walk up for the nearest resident ancestor and draw its matching sub-rect.
        let mut anc = id;
        let mut found = false;
        while let Some(p) = anc.parent() {
            anc = p;
            if let Some(layer) = r.layer_of(&TileKey { source, id: anc }) {
                let shift = tz - anc.z;
                let span = 1u32 << shift;
                let u0 = (id.x - (anc.x << shift)) as f32 / span as f32;
                let v0 = (id.y - (anc.y << shift)) as f32 / span as f32;
                let d = 1.0 / span as f32;
                quads.push(TileQuad {
                    corners,
                    uv: [u0, v0, u0 + d, v0 + d],
                    layer,
                });
                found = true;
                break;
            }
            // Low-zoom ancestors are tiny and load first so a fallback exists almost immediately.
            requests.push((TileKey { source, id: anc }, dist - 10_000.0 + anc.z as f32));
        }
        let _ = found;
    }
    FramePlan { quads, requests }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_geo::{LngLat, MapView};

    fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&Default::default())).ok()
    }

    fn solid(r: u8, g: u8, b: u8) -> Vec<u8> {
        [r, g, b, 255].repeat((TILE_PX * TILE_PX) as usize)
    }

    /// Renders a resident z1 tile and an ancestor fallback, then checks pixels read back from the GPU.
    #[test]
    fn renders_tile_and_ancestor_fallback() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut r = MapRenderer::new(&device, &queue);
        r.background = [0.0, 0.0, 0.0, 1.0];
        r.ensure_target([256, 256]);
        // Only the z0 world tile is resident (pure red); view at z2 must fall back to it.
        r.begin_frame();
        r.upload_tile(
            TileKey {
                source: 0,
                id: TileId::new(0, 0, 0),
            },
            &solid(255, 0, 0),
        );
        let view = MapView::new(LngLat::new(0.0, 0.0), 2.0, [256.0, 256.0]);
        let plan = plan_frame(&mut r, &view, 0, 19);
        assert!(!plan.quads.is_empty());
        assert!(
            plan.requests.iter().any(|(k, _)| k.id.z == 2),
            "should request z2 tiles"
        );
        r.render(&plan.quads);
        let px = r.read_pixels().unwrap();
        let at = |x: usize, y: usize| &px[(y * 256 + x) * 4..(y * 256 + x) * 4 + 4];
        assert_eq!(at(128, 128), &[255, 0, 0, 255], "fallback ancestor pixel");
        assert_eq!(at(5, 250), &[255, 0, 0, 255]);

        // Now a resident z2 tile of pure green replaces the fallback where it lies.
        r.begin_frame();
        r.upload_tile(
            TileKey {
                source: 0,
                id: TileId::new(2, 2, 2),
            },
            &solid(0, 255, 0),
        );
        let plan = plan_frame(&mut r, &view, 0, 19);
        r.render(&plan.quads);
        let px = r.read_pixels().unwrap();
        let at = |x: usize, y: usize| &px[(y * 256 + x) * 4..(y * 256 + x) * 4 + 4];
        assert_eq!(
            at(192, 192),
            &[0, 255, 0, 255],
            "z2 tile (2,2) covers SE of centre"
        );
        assert_eq!(at(64, 64), &[255, 0, 0, 255], "NW still the fallback");
    }
}
