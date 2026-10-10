//! GPU texture-array cache for tiles, shared by imagery (RGBA8) and elevation (R32F).

use crate::TileKey;
use std::collections::HashMap;

pub const TILE_PX: u32 = 256;
/// 256 is the default max array layers, so this works on every backend including WebGL2.
pub const POOL_LAYERS: u32 = 256;

/// A tile resolved to a texture layer, possibly via an ancestor: the wanted tile
/// occupies sub-rectangle (u0, v0)..(u0+scale, v0+scale) of that layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    pub layer: u32,
    pub u0: f32,
    pub v0: f32,
    pub scale: f32,
    /// Zoom of the tile actually resident (the wanted zoom minus the ancestor distance).
    pub zoom: u8,
}

impl Resolved {
    pub fn is_exact(&self) -> bool {
        self.scale == 1.0
    }
}

pub struct TilePool {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    layers: HashMap<TileKey, u32>,
    owner: Vec<Option<TileKey>>,
    used: Vec<u64>,
    bytes_per_texel: u32,
}

impl TilePool {
    pub fn new(
        device: &wgpu::Device,
        label: &str,
        format: wgpu::TextureFormat,
        bytes_per_texel: u32,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: TILE_PX,
                height: TILE_PX,
                depth_or_array_layers: POOL_LAYERS,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        Self {
            texture,
            view,
            layers: HashMap::new(),
            owner: vec![None; POOL_LAYERS as usize],
            used: vec![0; POOL_LAYERS as usize],
            bytes_per_texel,
        }
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.layers.contains_key(key)
    }

    /// Layer for a resident tile; marks it used this frame so it isn't evicted.
    pub fn touch(&mut self, key: &TileKey, frame: u64) -> Option<u32> {
        let l = *self.layers.get(key)?;
        self.used[l as usize] = frame;
        Some(l)
    }

    pub fn upload(&mut self, queue: &wgpu::Queue, key: TileKey, data: &[u8], frame: u64) {
        assert_eq!(
            data.len(),
            (TILE_PX * TILE_PX * self.bytes_per_texel) as usize
        );
        let layer = match self.layers.get(&key) {
            Some(&l) => l,
            None => {
                // Evict the least recently used layer (never-used layers have frame 0).
                let l = (0..POOL_LAYERS as usize)
                    .min_by_key(|&i| self.used[i])
                    .unwrap();
                if let Some(old) = self.owner[l].take() {
                    self.layers.remove(&old);
                }
                self.owner[l] = Some(key);
                self.layers.insert(key, l as u32);
                l as u32
            }
        };
        self.used[layer as usize] = frame;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: 0,
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TILE_PX * self.bytes_per_texel),
                rows_per_image: Some(TILE_PX),
            },
            wgpu::Extent3d {
                width: TILE_PX,
                height: TILE_PX,
                depth_or_array_layers: 1,
            },
        );
    }

    /// The tile itself if resident, else its nearest resident ancestor with the sub-rectangle to use.
    pub fn resolve(&mut self, key: &TileKey, frame: u64) -> Option<Resolved> {
        let mut cur = key.id;
        loop {
            if let Some(layer) = self.touch(
                &TileKey {
                    source: key.source,
                    id: cur,
                },
                frame,
            ) {
                let shift = key.id.z - cur.z;
                let span = 1u32 << shift;
                return Some(Resolved {
                    layer,
                    u0: (key.id.x - (cur.x << shift)) as f32 / span as f32,
                    v0: (key.id.y - (cur.y << shift)) as f32 / span as f32,
                    scale: 1.0 / span as f32,
                    zoom: cur.z,
                });
            }
            cur = cur.parent()?;
        }
    }
}
