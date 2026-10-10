//! Fire-history perimeters (NIFC) and snow gauges (SNOTEL): toolbar menu, background
//! fetching for the visible area, drawing, and click-to-read details. Data comes from
//! public services (see `smf-ext`) and is refetched as the view moves.

use crate::map_panel::{MapPanel, ViewMode};
use crate::overlay::{self, Projector};
use directories::ProjectDirs;
use eframe::egui::{self, Rect};
use smf_data::DataError;
use smf_ext::{ExtClient, ExtFeature};
use smf_features::{GeoHit, geometry_hit};
use smf_geo::LngLat;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

/// Below these zooms the view covers too much ground for one request to be useful.
const HISTORY_MIN_ZOOM: f64 = 7.0;
const SNOW_MIN_ZOOM: f64 = 5.0;
/// Snow readings change daily; refetching on a timer keeps a long session current.
const SNOW_REFRESH_SECS: u64 = 3 * 3600;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtKind {
    FireHistory,
    SnowGauges,
}

impl ExtKind {
    const ALL: [ExtKind; 2] = [ExtKind::FireHistory, ExtKind::SnowGauges];

    fn label(self) -> &'static str {
        match self {
            ExtKind::FireHistory => "Fire history (NIFC perimeters)",
            ExtKind::SnowGauges => "Snow gauges (SNOTEL)",
        }
    }

    fn min_zoom(self) -> f64 {
        match self {
            ExtKind::FireHistory => HISTORY_MIN_ZOOM,
            ExtKind::SnowGauges => SNOW_MIN_ZOOM,
        }
    }

    fn credit(self) -> &'static str {
        match self {
            ExtKind::FireHistory => "NIFC",
            ExtKind::SnowGauges => "NRCS SNOTEL",
        }
    }
}

#[derive(Default)]
struct Layer {
    on: bool,
    features: Vec<ExtFeature>,
    /// Area the current features were fetched for, and the zoom at that time.
    fetched: Option<([f64; 4], f64)>,
    fetched_at: Option<Instant>,
    inflight: bool,
}

type Reply = (ExtKind, [f64; 4], f64, Result<Vec<ExtFeature>, DataError>);

pub struct ExtState {
    history: Layer,
    snow: Layer,
    cache_dir: PathBuf,
    pub status: Option<String>,
    rx: Receiver<Reply>,
    tx: Sender<Reply>,
}

impl ExtState {
    pub fn new() -> Self {
        let cache_dir = ProjectDirs::from("org", "ShowMeFire", "Field")
            .map(|d| d.cache_dir().join("ext"))
            .unwrap_or_else(|| std::env::temp_dir().join("smf-field-ext"));
        let (tx, rx) = channel();
        Self {
            history: Layer::default(),
            snow: Layer::default(),
            cache_dir,
            status: None,
            rx,
            tx,
        }
    }

    fn layer(&mut self, k: ExtKind) -> &mut Layer {
        match k {
            ExtKind::FireHistory => &mut self.history,
            ExtKind::SnowGauges => &mut self.snow,
        }
    }

    fn layer_ref(&self, k: ExtKind) -> &Layer {
        match k {
            ExtKind::FireHistory => &self.history,
            ExtKind::SnowGauges => &self.snow,
        }
    }

    pub fn set_on(&mut self, k: ExtKind, on: bool) {
        let l = self.layer(k);
        l.on = on;
        if !on {
            l.features.clear();
            l.fetched = None;
        }
    }
}

/// True when the new view still lies well inside the area already fetched at about the same scale.
pub fn covers(fetched: ([f64; 4], f64), want: [f64; 4], zoom: f64) -> bool {
    let (f, fz) = fetched;
    (zoom - fz).abs() < 1.0
        && want[0] >= f[0]
        && want[1] >= f[1]
        && want[2] <= f[2]
        && want[3] <= f[3]
}

fn grow(b: [f64; 4], k: f64) -> [f64; 4] {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (hx, hy) = ((b[2] - b[0]) / 2.0 * k, (b[3] - b[1]) / 2.0 * k);
    [cx - hx, cy - hy, cx + hx, cy + hy]
}

impl MapPanel {
    pub(crate) fn ext_menu(&mut self, ui: &mut egui::Ui) {
        for k in ExtKind::ALL {
            let mut on = self.ext.layer_ref(k).on;
            if ui.checkbox(&mut on, k.label()).changed() {
                self.ext.set_on(k, on);
                if on && self.view.zoom < k.min_zoom() && self.mode == ViewMode::Flat {
                    self.ext.status = Some(format!(
                        "{} appears when you zoom in past level {:.0}.",
                        k.label(),
                        k.min_zoom()
                    ));
                }
            }
        }
        ui.separator();
        ui.weak("Details: click a perimeter or gauge.");
        if let Some(s) = &self.ext.status {
            ui.separator();
            ui.label(s);
        }
    }

    /// Visible ground area as [west, south, east, north] (2D), or a box around the 3D target.
    fn visible_bbox(&self) -> [f64; 4] {
        match self.mode {
            ViewMode::Flat => {
                let [w, h] = self.view.size;
                let pts =
                    [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]].map(|p| self.view.screen_to_lnglat(p));
                let mut b = [pts[0].lng, pts[0].lat, pts[0].lng, pts[0].lat];
                for p in pts {
                    b = [
                        b[0].min(p.lng),
                        b[1].min(p.lat),
                        b[2].max(p.lng),
                        b[3].max(p.lat),
                    ];
                }
                b
            }
            ViewMode::Terrain => {
                let half_m = (self.cam.distance_m * 3.0).max(2000.0);
                let dlat = half_m / 111_320.0;
                let c = self.cam.target;
                let dlng = dlat / c.lat.to_radians().cos().max(0.05);
                [c.lng - dlng, c.lat - dlat, c.lng + dlng, c.lat + dlat]
            }
        }
    }

    pub(crate) fn poll_ext(&mut self, ctx: &egui::Context) {
        while let Ok((k, bbox, zoom, r)) = self.ext.rx.try_recv() {
            let l = self.ext.layer(k);
            l.inflight = false;
            match r {
                Ok(f) => {
                    l.features = f;
                    l.fetched = Some((bbox, zoom));
                    l.fetched_at = Some(Instant::now());
                    self.ext.status = None;
                }
                Err(e) => {
                    // Keep what is on screen; back off until the view changes enough.
                    l.fetched = Some((bbox, zoom));
                    l.fetched_at = Some(Instant::now());
                    self.ext.status = Some(format!("{}: {e}", k.label()));
                }
            }
        }
        if self.view.size[0] < 200.0 || self.work_offline {
            return;
        }
        let zoom = match self.mode {
            ViewMode::Flat => self.view.zoom,
            ViewMode::Terrain => 12.0,
        };
        for k in ExtKind::ALL {
            let want = self.visible_bbox();
            let cache_dir = self.ext.cache_dir.clone();
            let l = self.ext.layer(k);
            if !l.on || l.inflight || zoom < k.min_zoom() {
                continue;
            }
            let stale = k == ExtKind::SnowGauges
                && l.fetched_at
                    .is_some_and(|t| t.elapsed().as_secs() > SNOW_REFRESH_SECS);
            if !stale && l.fetched.is_some_and(|f| covers(f, want, zoom)) {
                continue;
            }
            // Fetch a bit more than is visible so small pans don't trigger new requests.
            let bbox = grow(want, 1.5);
            l.inflight = true;
            let (tx, ctx) = (self.ext.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let now = smf_data::unix_now();
                let r = ExtClient::new(cache_dir).and_then(|mut c| match k {
                    ExtKind::FireHistory => c.fire_history(bbox, now),
                    ExtKind::SnowGauges => c.snow_gauges(bbox, now),
                });
                let _ = tx.send((k, bbox, zoom, r));
                ctx.request_repaint();
            });
        }
    }

    pub(crate) fn draw_ext(&self, painter: &egui::Painter, pj: &Projector, rect: Rect) {
        let visible = rect.expand(60.0);
        // Perimeters first so gauge markers stay on top.
        for k in ExtKind::ALL {
            let l = self.ext.layer_ref(k);
            if !l.on {
                continue;
            }
            for f in &l.features {
                overlay::draw_data_shape(painter, pj, &f.geometry, &f.style, visible);
            }
        }
    }

    /// Credit line text for the services currently drawn.
    pub(crate) fn ext_credit(&self) -> Option<String> {
        let c: Vec<&str> = ExtKind::ALL
            .into_iter()
            .filter(|k| self.ext.layer_ref(*k).on)
            .map(|k| k.credit())
            .collect();
        (!c.is_empty()).then(|| c.join(" | "))
    }

    /// Details for the topmost fire-history/snow feature under a click (gauges beat outlines
    /// beat interiors); an associated fn so the caller can keep other fields borrowed.
    pub(crate) fn ext_pick(
        ext: &ExtState,
        proj: &dyn Fn(LngLat) -> [f64; 2],
        pt: [f64; 2],
        tol: f64,
    ) -> Option<String> {
        let mut best: Option<(u8, &ExtFeature)> = None;
        for k in [ExtKind::SnowGauges, ExtKind::FireHistory] {
            let l = ext.layer_ref(k);
            if !l.on {
                continue;
            }
            for f in l.features.iter().rev() {
                let rank = match geometry_hit(&f.geometry, proj, pt, tol) {
                    Some(GeoHit::Edge) => 2,
                    Some(GeoHit::Interior) => 1,
                    None => continue,
                };
                if best.is_none_or(|(r, _)| rank > r) {
                    best = Some((rank, f));
                }
            }
        }
        best.map(|(_, f)| format!("{}: {}", f.title, f.lines.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refetch_rules() {
        let fetched = ([-93.0, 37.0, -91.0, 39.0], 9.0);
        // Panning inside the fetched area at the same scale needs nothing.
        assert!(covers(fetched, [-92.5, 37.5, -91.5, 38.5], 9.2));
        // Leaving the area, or zooming a level or more, does.
        assert!(!covers(fetched, [-92.5, 37.5, -90.5, 38.5], 9.2));
        assert!(!covers(fetched, [-92.5, 37.5, -91.5, 38.5], 10.1));
        assert!(!covers(fetched, [-92.5, 37.5, -91.5, 38.5], 7.9));
    }

    #[test]
    fn grow_keeps_the_centre() {
        let g = grow([0.0, 0.0, 2.0, 4.0], 1.5);
        assert_eq!(g, [-0.5, -1.0, 2.5, 5.0]);
    }
}
