//! 2D map camera: world <-> screen transforms and visible-tile selection.
//! All f64 on the CPU; callers hand the GPU already-projected screen coordinates,
//! which sidesteps f32 precision loss at high zoom.

use crate::{LngLat, MAX_LAT, TILE_SIZE, lnglat_to_world, world_to_lnglat};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapView {
    pub center: LngLat,
    /// Fractional zoom; 256 px tiles at zoom 0 cover the world.
    pub zoom: f64,
    /// Clockwise rotation of the map in radians (0 = north up).
    pub angle: f64,
    /// Viewport size in physical pixels.
    pub size: [f64; 2],
}

/// A tile as laid out for the current view. `x` is unwrapped so tiles past the
/// antimeridian keep their screen position; use `wrap_x` for the tile id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleTile {
    pub z: u8,
    pub x: i64,
    pub y: u32,
}

fn rot(v: [f64; 2], a: f64) -> [f64; 2] {
    let (s, c) = a.sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

impl MapView {
    pub fn new(center: LngLat, zoom: f64, size: [f64; 2]) -> Self {
        Self {
            center,
            zoom,
            angle: 0.0,
            size,
        }
    }

    fn center_world(&self) -> (f64, f64) {
        lnglat_to_world(self.center, self.zoom)
    }

    pub fn lnglat_to_screen(&self, p: LngLat) -> [f64; 2] {
        let (cx, cy) = self.center_world();
        let (wx, wy) = lnglat_to_world(p, self.zoom);
        let d = rot([wx - cx, wy - cy], self.angle);
        [self.size[0] / 2.0 + d[0], self.size[1] / 2.0 + d[1]]
    }

    pub fn screen_to_lnglat(&self, s: [f64; 2]) -> LngLat {
        let (cx, cy) = self.center_world();
        let d = rot(
            [s[0] - self.size[0] / 2.0, s[1] - self.size[1] / 2.0],
            -self.angle,
        );
        world_to_lnglat(cx + d[0], cy + d[1], self.zoom)
    }

    /// Drag the map by a screen-space delta.
    pub fn pan_pixels(&mut self, dx: f64, dy: f64) {
        let (cx, cy) = self.center_world();
        let d = rot([dx, dy], -self.angle);
        let mut c = world_to_lnglat(cx - d[0], cy - d[1], self.zoom);
        c.lat = c.lat.clamp(-MAX_LAT, MAX_LAT);
        c.lng = (c.lng + 180.0).rem_euclid(360.0) - 180.0;
        self.center = c;
    }

    /// Change zoom keeping the geographic point under `anchor` (screen px) fixed.
    pub fn zoom_at(&mut self, anchor: [f64; 2], new_zoom: f64, max_zoom: f64) {
        let new_zoom = new_zoom.clamp(0.0, max_zoom);
        let before = self.screen_to_lnglat(anchor);
        self.zoom = new_zoom;
        let after = self.lnglat_to_screen(before);
        self.pan_pixels(anchor[0] - after[0], anchor[1] - after[1]);
    }

    /// Screen corners (NW, NE, SE, SW before rotation) of tile (x, y) at integer zoom `tz`.
    pub fn tile_corners(&self, tz: u8, x: i64, y: u32) -> [[f64; 2]; 4] {
        let k = 2f64.powf(self.zoom - tz as f64); // screen px per tile px
        let (cx, cy) = self.center_world();
        let t = TILE_SIZE;
        let corner = |tx: f64, ty: f64| {
            let wx = tx * t * k;
            let wy = ty * t * k;
            let d = rot([wx - cx, wy - cy], self.angle);
            [self.size[0] / 2.0 + d[0], self.size[1] / 2.0 + d[1]]
        };
        let (x, y) = (x as f64, y as f64);
        [
            corner(x, y),
            corner(x + 1.0, y),
            corner(x + 1.0, y + 1.0),
            corner(x, y + 1.0),
        ]
    }

    /// Tiles at zoom `tz` intersecting the viewport.
    pub fn visible_tiles(&self, tz: u8) -> Vec<VisibleTile> {
        let k = 2f64.powf(self.zoom - tz as f64);
        let (cx, cy) = self.center_world();
        // Un-rotate the four screen corners into world space and take the bbox.
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for s in [
            [0.0, 0.0],
            [self.size[0], 0.0],
            [self.size[0], self.size[1]],
            [0.0, self.size[1]],
        ] {
            let d = rot(
                [s[0] - self.size[0] / 2.0, s[1] - self.size[1] / 2.0],
                -self.angle,
            );
            let (wx, wy) = (cx + d[0], cy + d[1]);
            min_x = min_x.min(wx);
            max_x = max_x.max(wx);
            min_y = min_y.min(wy);
            max_y = max_y.max(wy);
        }
        let tile_px = TILE_SIZE * k;
        let n = 1i64 << tz;
        let x0 = (min_x / tile_px).floor() as i64;
        let x1 = (max_x / tile_px).floor() as i64;
        let y0 = ((min_y / tile_px).floor() as i64).max(0);
        let y1 = ((max_y / tile_px).floor() as i64).min(n - 1);
        let mut out = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                out.push(VisibleTile {
                    z: tz,
                    x,
                    y: y as u32,
                });
            }
        }
        out
    }

    /// Screen-space distance from the viewport centre to a tile centre (for load priority).
    pub fn tile_distance(&self, tz: u8, x: i64, y: u32) -> f64 {
        let c = self.tile_corners(tz, x, y);
        let mx = (c[0][0] + c[2][0]) / 2.0 - self.size[0] / 2.0;
        let my = (c[0][1] + c[2][1]) / 2.0 - self.size[1] / 2.0;
        mx.hypot(my)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> MapView {
        MapView {
            center: LngLat::new(-92.3, 38.5),
            zoom: 9.3,
            angle: 0.4,
            size: [1000.0, 700.0],
        }
    }

    #[test]
    fn center_maps_to_screen_center() {
        let v = view();
        let s = v.lnglat_to_screen(v.center);
        assert!((s[0] - 500.0).abs() < 1e-6 && (s[1] - 350.0).abs() < 1e-6);
    }

    #[test]
    fn screen_roundtrip_with_rotation() {
        let v = view();
        let p = v.screen_to_lnglat([123.0, 456.0]);
        let s = v.lnglat_to_screen(p);
        assert!((s[0] - 123.0).abs() < 1e-6 && (s[1] - 456.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut v = view();
        let anchor = [800.0, 100.0];
        let before = v.screen_to_lnglat(anchor);
        v.zoom_at(anchor, 12.1, 19.0);
        let after = v.screen_to_lnglat(anchor);
        assert!((before.lng - after.lng).abs() < 1e-7 && (before.lat - after.lat).abs() < 1e-7);
    }

    #[test]
    fn pan_moves_map_with_cursor() {
        let mut v = view();
        let p = v.screen_to_lnglat([300.0, 300.0]);
        v.pan_pixels(40.0, -25.0);
        let s = v.lnglat_to_screen(p);
        assert!((s[0] - 340.0).abs() < 1e-6 && (s[1] - 275.0).abs() < 1e-6);
    }

    #[test]
    fn visible_tiles_cover_every_screen_pixel() {
        let v = view();
        let tz = 9;
        let tiles = v.visible_tiles(tz);
        // Every sampled screen point must land in one of the returned tiles.
        for sx in (0..=1000).step_by(50) {
            for sy in (0..=700).step_by(50) {
                let (sx, sy) = (sx as f64, sy as f64);
                let p = v.screen_to_lnglat([sx, sy]);
                let (wx, wy) = lnglat_to_world(p, tz as f64);
                let (tx, ty) = (
                    (wx / TILE_SIZE).floor() as i64,
                    (wy / TILE_SIZE).floor() as u32,
                );
                assert!(
                    tiles.iter().any(|t| t.x == tx && t.y == ty),
                    "missing {tx},{ty} for {sx},{sy}"
                );
            }
        }
    }

    #[test]
    fn tile_corners_match_lnglat_projection() {
        let v = MapView::new(LngLat::new(0.0, 0.0), 2.0, [512.0, 512.0]);
        // Tile (2,2) at z2 has NW corner at lng 0, lat 0 == view centre.
        let c = v.tile_corners(2, 2, 2);
        assert!((c[0][0] - 256.0).abs() < 1e-6 && (c[0][1] - 256.0).abs() < 1e-6);
        assert!((c[2][0] - 512.0).abs() < 1e-6);
    }
}
