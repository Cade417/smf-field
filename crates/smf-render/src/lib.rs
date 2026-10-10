//! wgpu map renderer: 2D raster tiles with an optional hillshade/slope overlay,
//! and a 3D draped-terrain view. Draws into its own offscreen texture so the same
//! path serves the egui panel, headless tests and (later) print output.
//! Knows nothing about egui or networking.

mod pool;
mod terrain;

use smf_geo::TileId;
use smf_terrain::{Camera3D, Chunk};

pub use pool::{POOL_LAYERS, Resolved, TILE_PX, TilePool};
pub use terrain::{
    ChunkInstance, GRID, OverlayFrame, ShadeParams, ShadeVertex, Terrain3dPlan, TerrainFrame,
    height_id, plan_shade_2d, plan_terrain, texel_meters,
};
pub use wgpu;

/// sRGB so the 8-bit tile bytes are decoded to linear on sample and re-encoded on write.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Pool key source id reserved for elevation tiles (imagery uses the caller's basemap index).
pub const HEIGHT_SOURCE: u16 = 0;

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
    /// Layer opacity 0..=1 (overlays); basemaps use 1.
    pub alpha: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    layer: u32,
    alpha: f32,
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
    imagery: TilePool,
    heights: TilePool,
    terrain: terrain::TerrainGpu,
    globals: wgpu::Buffer,
    vertices: wgpu::Buffer,
    vertex_capacity: u64,
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
        let imagery = TilePool::new(device, "imagery pool", FORMAT, 4);
        let heights = TilePool::new(device, "height pool", wgpu::TextureFormat::R32Float, 4);
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
                    resource: wgpu::BindingResource::TextureView(imagery.view()),
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
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32, 3 => Float32],
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
        let terrain = terrain::TerrainGpu::new(device, &imagery, &heights, &sampler);
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            bind_group,
            imagery,
            heights,
            terrain,
            globals,
            vertices,
            vertex_capacity,
            frame: 0,
            target: None,
            background: [0.82, 0.84, 0.86, 1.0],
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Imagery layer for a resident tile; marks it used this frame so it isn't evicted.
    pub fn layer_of(&mut self, key: &TileKey) -> Option<u32> {
        self.imagery.touch(key, self.frame)
    }

    /// Resolve imagery for `key`, falling back to the nearest resident ancestor.
    pub fn resolve_imagery(&mut self, key: &TileKey) -> Option<Resolved> {
        self.imagery.resolve(key, self.frame)
    }

    pub fn resident_tiles(&self) -> usize {
        self.imagery.len()
    }

    pub fn resident_heights(&self) -> usize {
        self.heights.len()
    }

    pub fn has_height(&self, id: TileId) -> bool {
        self.heights.contains(&TileKey {
            source: HEIGHT_SOURCE,
            id,
        })
    }

    pub fn upload_tile(&mut self, key: TileKey, rgba: &[u8]) {
        self.imagery.upload(&self.queue, key, rgba, self.frame);
    }

    /// Upload a 256x256 elevation tile in metres.
    pub fn upload_height(&mut self, id: TileId, heights: &[f32]) {
        self.heights.upload(
            &self.queue,
            TileKey {
                source: HEIGHT_SOURCE,
                id,
            },
            bytemuck::cast_slice(heights),
            self.frame,
        );
    }

    /// Overlay geometry for the 2D hillshade/slope pass, plus the elevation tiles still needed.
    pub fn plan_shade(
        &mut self,
        view: &smf_geo::MapView,
        tz: u8,
    ) -> (Vec<ShadeVertex>, Vec<(TileKey, f32)>) {
        plan_shade_2d(view, tz, &mut self.heights, self.frame)
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

    pub fn target_size(&self) -> Option<[u32; 2]> {
        self.target.as_ref().map(|t| t.size)
    }

    /// Plain 2D frame without terrain shading.
    pub fn render(&mut self, quads: &[TileQuad]) {
        self.render_2d(quads, &[], &ShadeParams::default());
    }

    /// 2D basemap quads, then (when enabled) the hillshade/slope overlay on top.
    pub fn render_2d(&mut self, quads: &[TileQuad], shade: &[ShadeVertex], params: &ShadeParams) {
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
                alpha: q.alpha,
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
            self.terrain
                .draw_shade(&self.device, &self.queue, &mut pass, size, params, shade);
        }
        self.queue.submit(Some(enc.finish()));
    }

    /// Plan and draw a 3D frame. Returns the plan so the caller can issue tile requests.
    pub fn render_3d(
        &mut self,
        cam: &Camera3D,
        chunks: &[Chunk],
        source: u16,
        overlays: &[(u16, f32, u8)],
        params: &ShadeParams,
    ) -> Terrain3dPlan {
        let plan = plan_terrain(
            cam,
            chunks,
            source,
            overlays,
            &mut self.imagery,
            &mut self.heights,
            self.frame,
        );
        if let Some(t) = &self.target {
            self.terrain.render_3d(
                &self.device,
                &self.queue,
                &t.view,
                t.size,
                &plan.frame,
                params,
            );
        }
        plan
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
    /// Zoom level the tiles were laid out at.
    pub tile_zoom: u8,
}

pub fn plan_frame(
    r: &mut MapRenderer,
    view: &smf_geo::MapView,
    source: u16,
    max_zoom: u8,
    opacity: f32,
) -> FramePlan {
    let tz = (view.zoom.round().max(0.0) as u8).min(max_zoom);
    plan_frame_at(r, view, source, tz, opacity)
}

/// Like `plan_frame` but with an explicit tile zoom. Printing uses the next zoom *up*, so tiles are
/// downsampled instead of stretched and output is at least as sharp as the screen.
pub fn plan_frame_at(
    r: &mut MapRenderer,
    view: &smf_geo::MapView,
    source: u16,
    tz: u8,
    opacity: f32,
) -> FramePlan {
    let mut quads = Vec::new();
    let mut requests = Vec::new();
    let mut tiles = view.visible_tiles(tz);
    // Nearest-to-centre tiles are requested first.
    tiles.sort_by(|a, b| {
        view.tile_distance(tz, a.x, a.y)
            .total_cmp(&view.tile_distance(tz, b.x, b.y))
    });
    for t in tiles {
        let id = TileId::new(tz, smf_geo::wrap_x(t.x, tz), t.y);
        let dist = view.tile_distance(tz, t.x, t.y) as f32;
        let corners = view
            .tile_corners(tz, t.x, t.y)
            .map(|p| [p[0] as f32, p[1] as f32]);
        let key = TileKey { source, id };
        let res = r.resolve_imagery(&key);
        if let Some(res) = res {
            quads.push(TileQuad {
                corners,
                uv: [res.u0, res.v0, res.u0 + res.scale, res.v0 + res.scale],
                layer: res.layer,
                alpha: opacity,
            });
        }
        if !res.is_some_and(|x| x.is_exact()) {
            requests.push((key, dist));
            // Low-zoom ancestors are tiny and load first so a fallback exists almost immediately.
            let mut anc = id;
            while let Some(p) = anc.parent() {
                anc = p;
                let k = TileKey { source, id: anc };
                if r.imagery.contains(&k) {
                    break;
                }
                requests.push((k, dist - 10_000.0 + anc.z as f32));
            }
        }
    }
    FramePlan {
        quads,
        requests,
        tile_zoom: tz,
    }
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

    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
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
        let plan = plan_frame(&mut r, &view, 0, 19, 1.0);
        assert!(!plan.quads.is_empty());
        assert!(
            plan.requests.iter().any(|(k, _)| k.id.z == 2),
            "should request z2 tiles"
        );
        r.render(&plan.quads);
        let p = r.read_pixels().unwrap();
        assert_eq!(
            px(&p, 256, 128, 128),
            [255, 0, 0, 255],
            "fallback ancestor pixel"
        );
        assert_eq!(px(&p, 256, 5, 250), [255, 0, 0, 255]);

        // Now a resident z2 tile of pure green replaces the fallback where it lies.
        r.begin_frame();
        r.upload_tile(
            TileKey {
                source: 0,
                id: TileId::new(2, 2, 2),
            },
            &solid(0, 255, 0),
        );
        let plan = plan_frame(&mut r, &view, 0, 19, 1.0);
        r.render(&plan.quads);
        let p = r.read_pixels().unwrap();
        assert_eq!(
            px(&p, 256, 192, 192),
            [0, 255, 0, 255],
            "z2 tile (2,2) covers SE of centre"
        );
        assert_eq!(
            px(&p, 256, 64, 64),
            [255, 0, 0, 255],
            "NW still the fallback"
        );
    }

    /// A 45 degree ramp rising east: slope shading must classify it as the steepest class,
    /// and flat ground must be left untouched. Verifies the shader's normals, texel scale and
    /// the 2D overlay path on a real GPU.
    #[test]
    fn slope_overlay_classifies_ramp_and_ignores_flat() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let lat = 38.6;
        let id = TileId::from_lnglat(LngLat::new(-92.3, lat), 10);
        let b = smf_terrain::tile_bounds_mercator(id);
        let center = smf_geo::mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let texel = texel_meters(10, center.lat);
        let mut ramp = vec![0f32; 256 * 256];
        for y in 0..256 {
            for x in 0..256 {
                ramp[y * 256 + x] = x as f32 * texel; // rise == run: 45 degrees
            }
        }
        let mut r = MapRenderer::new(&device, &queue);
        r.background = [0.0, 0.0, 0.0, 1.0];
        r.ensure_target([256, 256]);
        r.begin_frame();
        r.upload_tile(TileKey { source: 1, id }, &solid(0, 0, 255));
        r.upload_height(id, &ramp);
        let view = MapView::new(center, 10.0, [256.0, 256.0]);
        let params = ShadeParams {
            slope: true,
            slope_alpha: 1.0,
            ..Default::default()
        };
        let plan = plan_frame(&mut r, &view, 1, 19, 1.0);
        let (shade, _) = plan_shade_2d(&view, plan.tile_zoom, &mut r.heights, r.frame);
        assert!(!shade.is_empty());
        r.render_2d(&plan.quads, &shade, &params);
        let p = r.read_pixels().unwrap();
        let c = px(&p, 256, 128, 128);
        // Steepest class is purple (0.55, 0.10, 0.75) -> sRGB ~ (194, 89, 225).
        assert!(
            c[0] > 150 && c[2] > 180 && c[1] < 130,
            "45 degree ramp should be purple, got {c:?}"
        );

        // Same view with flat terrain: no overlay, so the blue basemap shows through.
        r.begin_frame();
        r.upload_height(id, &vec![100.0f32; 256 * 256]);
        let plan = plan_frame(&mut r, &view, 1, 19, 1.0);
        let (shade, _) = plan_shade_2d(&view, plan.tile_zoom, &mut r.heights, r.frame);
        r.render_2d(&plan.quads, &shade, &params);
        let p = r.read_pixels().unwrap();
        assert_eq!(
            px(&p, 256, 128, 128),
            [0, 0, 255, 255],
            "flat ground must be unshaded"
        );
    }

    /// Hillshade: a slope facing the sun is brighter than one facing away.
    #[test]
    fn hillshade_depends_on_aspect() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let id = TileId::from_lnglat(LngLat::new(-92.3, 38.6), 10);
        let b = smf_terrain::tile_bounds_mercator(id);
        let center = smf_geo::mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let texel = texel_meters(10, center.lat);
        // Height rises to the east (~20 degrees), so the surface faces west.
        let ramp: Vec<f32> = (0..256 * 256)
            .map(|i| (i % 256) as f32 * texel * 0.36)
            .collect();
        let render_with_sun = |az: f32| {
            let mut r = MapRenderer::new(&device, &queue);
            r.background = [0.0, 0.0, 0.0, 1.0];
            r.ensure_target([128, 128]);
            r.begin_frame();
            r.upload_tile(TileKey { source: 1, id }, &solid(128, 128, 128));
            r.upload_height(id, &ramp);
            let view = MapView::new(center, 10.0, [128.0, 128.0]);
            let params = ShadeParams {
                hillshade: true,
                hill_strength: 1.0,
                sun_azimuth_deg: az,
                sun_altitude_deg: 35.0,
                ..Default::default()
            };
            let plan = plan_frame(&mut r, &view, 1, 19, 1.0);
            let (shade, _) = plan_shade_2d(&view, plan.tile_zoom, &mut r.heights, r.frame);
            r.render_2d(&plan.quads, &shade, &params);
            let p = r.read_pixels().unwrap();
            px(&p, 128, 64, 64)[0]
        };
        // Surface rises eastward, so it faces west: a sun in the west (270) lights it, east (90) doesn't.
        let lit = render_with_sun(270.0);
        let dark = render_with_sun(90.0);
        assert!(
            lit > dark + 40,
            "west-facing slope: sun W = {lit}, sun E = {dark}"
        );
    }

    /// 3D: a flat tile seen from above shows its imagery; a steep ramp gets the slope class colour,
    /// proving the terrain pipeline, instancing, depth buffer and camera matrices work end to end.
    #[test]
    fn terrain_3d_draws_draped_imagery_and_shading() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let target = LngLat::new(-92.3, 38.6);
        let id = TileId::from_lnglat(target, 10);
        let b = smf_terrain::tile_bounds_mercator(id);
        let center = smf_geo::mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let mut cam = Camera3D {
            target: center,
            target_elev_m: 100.0,
            distance_m: 20_000.0,
            pitch: 89.0f64.to_radians(),
            viewport: [128.0, 128.0],
            ..Camera3D::default()
        };
        cam.heading = 0.0;
        let chunks = [Chunk { id }];

        let mut r = MapRenderer::new(&device, &queue);
        r.ensure_target([128, 128]);
        r.begin_frame();
        r.upload_tile(TileKey { source: 1, id }, &solid(0, 200, 0));
        r.upload_height(id, &vec![100.0f32; 256 * 256]);
        let plan = r.render_3d(&cam, &chunks, 1, &[], &ShadeParams::default());
        assert_eq!(plan.frame.instances.len(), 1);
        let p = r.read_pixels().unwrap();
        assert_eq!(
            px(&p, 128, 64, 64),
            [0, 200, 0, 255],
            "imagery draped on flat terrain"
        );
        // Corner of the frame should still see this tile or sky; the sky colour is the clear colour.
        let sky = px(&p, 128, 0, 0);
        assert!(sky[3] == 255);

        // Steep ramp + slope overlay.
        let texel = texel_meters(10, center.lat);
        let ramp: Vec<f32> = (0..256 * 256).map(|i| (i % 256) as f32 * texel).collect();
        r.begin_frame();
        r.upload_height(id, &ramp);
        let params = ShadeParams {
            slope: true,
            slope_alpha: 1.0,
            ..Default::default()
        };
        let _ = r.render_3d(&cam, &chunks, 1, &[], &params);
        let p = r.read_pixels().unwrap();
        let c = px(&p, 128, 64, 64);
        assert!(
            c[0] > 150 && c[2] > 150,
            "steep slope should be purple-ish in 3D, got {c:?}"
        );
    }

    /// Looking from the side at a mountain: terrain in front must hide terrain behind it (depth test).
    #[test]
    fn terrain_3d_depth_ordering() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let id = TileId::from_lnglat(LngLat::new(-92.3, 38.6), 10);
        let b = smf_terrain::tile_bounds_mercator(id);
        let center = smf_geo::mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        // A tall wall across the middle rows of the tile.
        let mut h = vec![0f32; 256 * 256];
        for y in 120..136 {
            for x in 0..256 {
                h[y * 256 + x] = 3000.0;
            }
        }
        let cam = Camera3D {
            target: center,
            target_elev_m: 0.0,
            distance_m: 40_000.0,
            pitch: 8.0f64.to_radians(),
            heading: 0.0,
            viewport: [128.0, 128.0],
            ..Camera3D::default()
        };
        let mut r = MapRenderer::new(&device, &queue);
        r.ensure_target([128, 128]);
        r.begin_frame();
        r.upload_tile(TileKey { source: 1, id }, &solid(200, 0, 0));
        r.upload_height(id, &h);
        let _ = r.render_3d(&cam, &[Chunk { id }], 1, &[], &ShadeParams::default());
        let p = r.read_pixels().unwrap();
        // Something red (terrain) must be visible, and it must not be the whole frame (sky above).
        let red = (0..128 * 128)
            .filter(|i| p[i * 4] == 200 && p[i * 4 + 1] == 0)
            .count();
        assert!(red > 50, "terrain should be visible, red px = {red}");
        assert!(red < 128 * 128, "sky should be visible above the horizon");
    }

    fn to_lin(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn to_srgb(l: f32) -> f32 {
        let c = if l <= 0.003_130_8 {
            l * 12.92
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        };
        c * 255.0
    }

    /// Blending happens in linear light because the target is sRGB, so the expected value is
    /// computed the same way.
    fn expect_mix(base: [u8; 3], over: [u8; 3], opacity: f32) -> [f32; 3] {
        let m = |i: usize| to_srgb(to_lin(base[i]) * (1.0 - opacity) + to_lin(over[i]) * opacity);
        [m(0), m(1), m(2)]
    }

    fn close(got: [u8; 4], want: [f32; 3], tol: f32) -> bool {
        (0..3).all(|i| (got[i] as f32 - want[i]).abs() <= tol)
    }

    #[test]
    fn overlay_layers_blend_with_opacity_in_2d() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut r = MapRenderer::new(&device, &queue);
        r.ensure_target([128, 128]);
        r.begin_frame();
        let id = TileId::new(2, 1, 1);
        r.upload_tile(TileKey { source: 1, id }, &solid(220, 40, 40)); // basemap
        r.upload_tile(TileKey { source: 2, id }, &solid(30, 60, 240)); // overlay
        let view = MapView::new(LngLat::new(-45.0, 40.0), 2.0, [128.0, 128.0]);
        let base = plan_frame(&mut r, &view, 1, 19, 1.0);
        let mut quads = base.quads.clone();
        for opacity in [0.0f32, 0.5, 1.0] {
            let over = plan_frame(&mut r, &view, 2, 19, opacity);
            let mut all = quads.clone();
            all.extend(over.quads);
            r.render(&all);
            let p = r.read_pixels().unwrap();
            let got = px(&p, 128, 64, 64);
            let want = expect_mix([220, 40, 40], [30, 60, 240], opacity);
            assert!(
                close(got, want, 3.0),
                "opacity {opacity}: got {got:?}, want {want:?}"
            );
        }
        quads.clear();
    }

    #[test]
    fn overlay_layers_drape_on_3d_terrain_and_respect_opacity() {
        let Some((device, queue)) = headless() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let id = TileId::from_lnglat(LngLat::new(-92.3, 38.6), 10);
        let b = smf_terrain::tile_bounds_mercator(id);
        let center = smf_geo::mercator_to_lnglat((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let cam = Camera3D {
            target: center,
            target_elev_m: 100.0,
            distance_m: 20_000.0,
            pitch: 89.0f64.to_radians(),
            viewport: [128.0, 128.0],
            ..Camera3D::default()
        };
        let mut r = MapRenderer::new(&device, &queue);
        r.ensure_target([128, 128]);
        r.begin_frame();
        r.upload_tile(TileKey { source: 1, id }, &solid(0, 200, 0));
        r.upload_tile(TileKey { source: 2, id }, &solid(240, 0, 100));
        r.upload_height(id, &vec![100.0f32; 256 * 256]);
        for opacity in [0.0f32, 0.4, 1.0] {
            let plan = r.render_3d(
                &cam,
                &[Chunk { id }],
                1,
                &[(2, opacity, 19)],
                &ShadeParams::default(),
            );
            assert_eq!(plan.frame.overlays.len(), 1);
            assert_eq!(plan.frame.overlays[0].instances.len(), 1);
            let p = r.read_pixels().unwrap();
            let got = px(&p, 128, 64, 64);
            let want = expect_mix([0, 200, 0], [240, 0, 100], opacity);
            assert!(
                close(got, want, 3.0),
                "opacity {opacity}: got {got:?}, want {want:?}"
            );
        }
        // An overlay with no tile loaded draws nothing and leaves the base intact.
        let plan = r.render_3d(
            &cam,
            &[Chunk { id }],
            1,
            &[(3, 1.0, 19)],
            &ShadeParams::default(),
        );
        assert!(plan.frame.overlays[0].instances.is_empty());
        assert!(
            plan.imagery_requests.iter().any(|(k, _)| k.source == 3),
            "missing overlay tiles are requested"
        );
        let p = r.read_pixels().unwrap();
        assert_eq!(px(&p, 128, 64, 64), [0, 200, 0, 255]);
    }
}
