use crate::HeightField;
use glam::{DMat4, DVec3, DVec4};
use smf_geo::{LngLat, lnglat_to_mercator, mercator_to_lnglat};

/// Orbit camera around a ground target. Heading 0 looks north; pitch is the
/// angle below the horizon (90 deg = straight down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera3D {
    pub target: LngLat,
    /// Real metres above sea level at the target.
    pub target_elev_m: f64,
    /// Real metres from the eye to the target.
    pub distance_m: f64,
    pub heading: f64,
    pub pitch: f64,
    pub fov_y: f64,
    /// Viewport in physical pixels.
    pub viewport: [f64; 2],
}

impl Default for Camera3D {
    fn default() -> Self {
        Self {
            target: LngLat::new(-92.5, 38.5),
            target_elev_m: 0.0,
            distance_m: 20_000.0,
            heading: 0.0,
            pitch: 55f64.to_radians(),
            fov_y: 45f64.to_radians(),
            viewport: [1280.0, 800.0],
        }
    }
}

pub const MIN_PITCH: f64 = 5.0 * std::f64::consts::PI / 180.0;
pub const MAX_PITCH: f64 = 90.0 * std::f64::consts::PI / 180.0;
pub const MIN_DISTANCE_M: f64 = 30.0;
/// Keep the eye at least this far above the ground (real metres).
const MIN_CLEARANCE_M: f64 = 25.0;

impl Camera3D {
    /// Mercator scale factor: scene metres per real metre at the target latitude.
    pub fn k(&self) -> f64 {
        1.0 / self.target.lat.to_radians().cos()
    }

    pub fn aspect(&self) -> f64 {
        self.viewport[0] / self.viewport[1].max(1.0)
    }

    /// Focal length in pixels (used to convert world error to screen error).
    pub fn focal_px(&self) -> f64 {
        self.viewport[1] / (2.0 * (self.fov_y / 2.0).tan())
    }

    fn target_scene(&self) -> DVec3 {
        let (x, y) = lnglat_to_mercator(self.target);
        DVec3::new(x, y, self.target_elev_m * self.k())
    }

    /// Unit view direction in scene axes (x east, y north, z up).
    pub fn dir(&self) -> DVec3 {
        let (sh, ch) = self.heading.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        DVec3::new(sh * cp, ch * cp, -sp)
    }

    /// Eye position in scene (Mercator) coordinates.
    pub fn eye(&self) -> DVec3 {
        self.target_scene() - self.dir() * (self.distance_m * self.k())
    }

    pub fn eye_lnglat(&self) -> LngLat {
        let e = self.eye();
        mercator_to_lnglat(e.x, e.y)
    }

    pub fn near(&self) -> f64 {
        (self.distance_m * self.k() * 0.01).max(1.0)
    }

    /// View-projection for coordinates given *relative to the eye* (reversed-Z, infinite far).
    pub fn view_proj_rel(&self) -> DMat4 {
        let view = DMat4::look_to_rh(DVec3::ZERO, self.dir(), DVec3::Z);
        let proj = DMat4::perspective_infinite_reverse_rh(self.fov_y, self.aspect(), self.near());
        proj * view
    }

    /// Frustum planes (ax+by+cz+d >= 0 is inside) for eye-relative coordinates.
    pub fn frustum_planes(&self) -> [DVec4; 5] {
        let m = self.view_proj_rel();
        let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
        [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r3 - r2]
    }

    /// Screen position (physical px) of a point, or None when behind the camera.
    pub fn project(&self, p: LngLat, elev_m: f64) -> Option<[f64; 2]> {
        let (x, y) = lnglat_to_mercator(p);
        let rel = DVec3::new(x, y, elev_m * self.k()) - self.eye();
        let c = self.view_proj_rel() * DVec4::new(rel.x, rel.y, rel.z, 1.0);
        if c.w <= 0.0 {
            return None;
        }
        let (nx, ny) = (c.x / c.w, c.y / c.w);
        Some([
            (nx * 0.5 + 0.5) * self.viewport[0],
            (0.5 - ny * 0.5) * self.viewport[1],
        ])
    }

    /// Ray through a pixel in scene coordinates: (origin, unit direction).
    pub fn ray(&self, px: [f64; 2]) -> (DVec3, DVec3) {
        let (nx, ny) = (
            px[0] / self.viewport[0] * 2.0 - 1.0,
            1.0 - px[1] / self.viewport[1] * 2.0,
        );
        let th = (self.fov_y / 2.0).tan();
        let dir = self.dir();
        let right = dir.cross(DVec3::Z).normalize();
        let up = right.cross(dir);
        let d = (dir + right * (nx * th * self.aspect()) + up * (ny * th)).normalize();
        (self.eye(), d)
    }

    /// Where the pixel's ray first meets the terrain, found by marching then bisecting.
    pub fn pick_ground(&self, px: [f64; 2], hf: &HeightField) -> Option<(LngLat, f32)> {
        let (o, d) = self.ray(px);
        if d.z >= -1e-6 {
            // Looking at or above the horizon: only hits terrain far away; intersect z = target plane.
            return None;
        }
        let k = self.k();
        let ground = |t: f64| -> Option<(f64, LngLat, f32)> {
            let p = o + d * t;
            let ll = mercator_to_lnglat(p.x, p.y);
            let h = hf.sample(ll)?;
            Some((p.z - h as f64 * k, ll, h))
        };
        let max_t = (o.z / -d.z) * 4.0 + self.distance_m * k * 2.0; // generous: terrain may be above sea level
        let mut t = self.near();
        let mut step = (self.distance_m * k * 0.01).max(2.0);
        let mut prev_t = t;
        while t < max_t {
            match ground(t) {
                Some((clear, _, _)) if clear <= 0.0 => {
                    // Bisect between the last point above ground and this one below it.
                    let (mut lo, mut hi) = (prev_t, t);
                    for _ in 0..24 {
                        let mid = (lo + hi) / 2.0;
                        match ground(mid) {
                            Some((c, _, _)) if c > 0.0 => lo = mid,
                            _ => hi = mid,
                        }
                    }
                    let (_, ll, h) = ground(hi)?;
                    return Some((ll, h));
                }
                Some(_) => {}
                None => return None, // ran into terrain we haven't loaded yet
            }
            prev_t = t;
            t += step;
            step *= 1.03;
        }
        None
    }

    /// Push the eye up/in until it clears the ground, so you never end up inside a hill.
    pub fn enforce_clearance(&mut self, hf: &HeightField) {
        for _ in 0..40 {
            let e = self.eye();
            let ll = mercator_to_lnglat(e.x, e.y);
            let Some(h) = hf.sample(ll) else { return };
            if e.z / self.k() >= h as f64 + MIN_CLEARANCE_M || self.distance_m <= MIN_DISTANCE_M {
                return;
            }
            self.distance_m = (self.distance_m * 0.9).max(MIN_DISTANCE_M);
        }
    }

    /// Move the target along the ground by a screen-space drag (dx, dy in physical px).
    pub fn pan_pixels(&mut self, dx: f64, dy: f64) {
        let m_per_px = 2.0 * self.distance_m * (self.fov_y / 2.0).tan() / self.viewport[1];
        let (sh, ch) = self.heading.sin_cos();
        // Drag right moves the map right => target moves left in view space.
        let (right, fwd) = (-dx * m_per_px, dy * m_per_px / self.pitch.sin().max(0.15));
        let east = right * ch + fwd * sh;
        let north = -right * sh + fwd * ch;
        let (x, y) = lnglat_to_mercator(self.target);
        self.target = mercator_to_lnglat(x + east * self.k(), y + north * self.k());
    }

    pub fn orbit(&mut self, d_heading: f64, d_pitch: f64) {
        self.heading = (self.heading + d_heading).rem_euclid(std::f64::consts::TAU);
        self.pitch = (self.pitch + d_pitch).clamp(MIN_PITCH, MAX_PITCH);
    }

    pub fn dolly(&mut self, factor: f64) {
        self.distance_m = (self.distance_m * factor).clamp(MIN_DISTANCE_M, 6_000_000.0);
    }

    /// Distance at which a top-down view shows the same ground as a 2D map at `zoom`.
    pub fn distance_for_zoom(&self, zoom: f64) -> f64 {
        let mpp = smf_geo::meters_per_pixel(self.target.lat, zoom);
        self.viewport[1] * mpp / 2.0 / (self.fov_y / 2.0).tan()
    }

    pub fn zoom_for_distance(&self) -> f64 {
        let mpp = 2.0 * self.distance_m * (self.fov_y / 2.0).tan() / self.viewport[1];
        (smf_geo::EARTH_CIRCUMFERENCE_M * self.target.lat.to_radians().cos() / (256.0 * mpp)).log2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeightTile, TERRAIN_MAX_ZOOM};
    use smf_geo::TileId;

    fn cam() -> Camera3D {
        Camera3D {
            target: LngLat::new(-92.3, 38.6),
            target_elev_m: 200.0,
            distance_m: 5000.0,
            ..Camera3D::default()
        }
    }

    #[test]
    fn target_projects_to_screen_center() {
        let c = cam();
        let s = c.project(c.target, c.target_elev_m).unwrap();
        assert!(
            (s[0] - 640.0).abs() < 1e-6 && (s[1] - 400.0).abs() < 1e-6,
            "{s:?}"
        );
    }

    #[test]
    fn north_is_up_at_heading_zero() {
        let c = cam();
        let north = LngLat::new(c.target.lng, c.target.lat + 0.01);
        let east = LngLat::new(c.target.lng + 0.01, c.target.lat);
        let (t, n, e) = (
            c.project(c.target, 200.0).unwrap(),
            c.project(north, 200.0).unwrap(),
            c.project(east, 200.0).unwrap(),
        );
        assert!(n[1] < t[1], "north should be higher on screen");
        assert!(e[0] > t[0], "east should be to the right");
    }

    #[test]
    fn heading_rotates_the_view() {
        let mut c = cam();
        c.heading = std::f64::consts::FRAC_PI_2; // looking east: north is to the left
        let north = LngLat::new(c.target.lng, c.target.lat + 0.01);
        let t = c.project(c.target, 200.0).unwrap();
        let n = c.project(north, 200.0).unwrap();
        assert!(n[0] < t[0]);
    }

    #[test]
    fn points_behind_camera_are_rejected() {
        let c = cam();
        let eye = c.eye_lnglat();
        let behind = LngLat::new(eye.lng, eye.lat - 0.2); // heading north, so south is behind
        assert!(c.project(behind, 0.0).is_none());
    }

    #[test]
    fn ray_through_center_hits_target() {
        let c = cam();
        let (o, d) = c.ray([640.0, 400.0]);
        let t = (c.distance_m * c.k()).abs();
        let p = o + d * t;
        let tgt = c.target_scene();
        assert!((p - tgt).length() < 1e-3, "{p:?} vs {tgt:?}");
    }

    fn plateau(h: f32) -> HeightField {
        let mut hf = HeightField::default();
        let id = TileId::from_lnglat(cam().target, 8);
        hf.insert(
            id,
            HeightTile {
                heights: vec![h; 256 * 256],
                min: h,
                max: h,
            },
        );
        let _ = TERRAIN_MAX_ZOOM;
        hf
    }

    #[test]
    fn pick_ground_finds_flat_plateau() {
        let c = cam();
        let hf = plateau(200.0);
        let (ll, h) = c.pick_ground([640.0, 400.0], &hf).unwrap();
        assert!((h - 200.0).abs() < 1e-3);
        assert!(
            smf_geo::distance_m(ll, c.target) < 5.0,
            "{}",
            smf_geo::distance_m(ll, c.target)
        );
    }

    #[test]
    fn clearance_pulls_camera_out_of_a_hill() {
        let mut c = cam();
        c.pitch = 6f64.to_radians();
        c.distance_m = 3000.0;
        c.target_elev_m = 0.0;
        let hf = plateau(150.0); // ground well above the eye
        c.enforce_clearance(&hf);
        let e = c.eye();
        assert!(e.z / c.k() >= 150.0 + 24.0 || c.distance_m <= MIN_DISTANCE_M);
    }

    #[test]
    fn dragging_right_moves_the_map_right() {
        let mut c = cam();
        let before = c.project(c.target, 200.0).unwrap();
        let t0 = c.target;
        c.pan_pixels(100.0, 0.0);
        // The old target should now appear further right on screen than before.
        let after = c.project(t0, 200.0).unwrap();
        assert!(after[0] > before[0] + 50.0, "{before:?} -> {after:?}");
    }

    #[test]
    fn zoom_distance_roundtrip() {
        let mut c = cam();
        c.distance_m = c.distance_for_zoom(12.0);
        assert!((c.zoom_for_distance() - 12.0).abs() < 1e-9);
    }
}
