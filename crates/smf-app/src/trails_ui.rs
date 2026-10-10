//! "Snap to trails": while drawing a line or polygon, each click is pulled onto the nearest
//! OpenStreetMap trail and the segment from the previous point follows the trail network
//! instead of cutting straight across. Trails are fetched for the visible area on demand.

use crate::ext_ui::{covers, grow};
use crate::map_panel::{MapPanel, ViewMode};
use eframe::egui;
use smf_features::{Editor, Tool};
use smf_geo::LngLat;
use smf_route::{TrailGraph, Way, fetch_trails};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Trails are only fetched this close: a wider view would pull far too many paths.
const MIN_ZOOM: f64 = 12.0;
/// Overpass is a shared public service; never ask for more than about this much ground.
const MAX_SPAN_DEG: f64 = 0.35;
const RETRY: std::time::Duration = std::time::Duration::from_secs(20);

type Reply = ([f64; 4], f64, Result<Vec<Way>, String>);

pub struct TrailState {
    pub on: bool,
    graph: Option<Arc<TrailGraph>>,
    fetched: Option<([f64; 4], f64)>,
    inflight: bool,
    /// After a failed fetch, wait before asking again instead of hammering a busy server.
    retry_at: Option<std::time::Instant>,
    pub status: Option<String>,
    /// Verification hook (SMF_TRAIL_ROUTE): draw a line through these points once trails load.
    pub dev_route: Option<Vec<LngLat>>,
    rx: Receiver<Reply>,
    tx: Sender<Reply>,
}

impl TrailState {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            on: false,
            graph: None,
            fetched: None,
            inflight: false,
            retry_at: None,
            status: None,
            dev_route: None,
            rx,
            tx,
        }
    }

    pub fn way_count(&self) -> usize {
        self.graph.as_ref().map_or(0, |g| g.way_count())
    }
}

/// Clamp a wanted box about its centre to the span limit.
fn limit_span(b: [f64; 4]) -> [f64; 4] {
    let k = (MAX_SPAN_DEG / (b[2] - b[0]).max(MAX_SPAN_DEG / 100.0))
        .min(MAX_SPAN_DEG / (b[3] - b[1]).max(MAX_SPAN_DEG / 100.0))
        .min(1.0);
    grow(b, k)
}

impl MapPanel {
    pub(crate) fn trails_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.trails.on, "Snap to trails (OpenStreetMap)")
            .changed()
            && self.trails.on
        {
            self.trails.status = Some(format!(
                "Zoom in past level {MIN_ZOOM:.0} to load trails, then draw lines or polygons."
            ));
        }
        if self.trails.on {
            ui.weak(match self.trails.way_count() {
                0 => "No trails loaded for this area yet.".to_string(),
                n => format!("{n} trail segments loaded."),
            });
        }
        if let Some(s) = &self.trails.status {
            ui.label(s);
        }
    }

    pub(crate) fn poll_trails(&mut self, ctx: &egui::Context) {
        while let Ok((bbox, zoom, r)) = self.trails.rx.try_recv() {
            self.trails.inflight = false;
            match r {
                Ok(ways) => {
                    self.trails.fetched = Some((bbox, zoom));
                    tracing::info!("trails: {} ways loaded", ways.len());
                    self.trails.status = None;
                    self.trails.graph = Some(Arc::new(TrailGraph::new(ways)));
                }
                // Keep any earlier graph; the user can still draw freehand-style.
                Err(e) => {
                    tracing::warn!("trails: {e}");
                    self.trails.status = Some(format!("{e}; retrying shortly."));
                    self.trails.retry_at = Some(std::time::Instant::now() + RETRY);
                }
            }
        }
        if let Some(t) = self.trails.retry_at {
            if t > std::time::Instant::now() {
                ctx.request_repaint_after(t - std::time::Instant::now());
                return;
            }
            self.trails.retry_at = None;
        }
        if !self.trails.on
            || self.trails.inflight
            || self.work_offline
            || self.view.size[0] < 200.0
            || self.mode != ViewMode::Flat
            || self.view.zoom < MIN_ZOOM
        {
            return;
        }
        let [w, h] = self.view.size;
        let corners = [[0.0, 0.0], [w, h]].map(|p| self.view.screen_to_lnglat(p));
        let want = [
            corners[0].lng.min(corners[1].lng),
            corners[0].lat.min(corners[1].lat),
            corners[0].lng.max(corners[1].lng),
            corners[0].lat.max(corners[1].lat),
        ];
        if self
            .trails
            .fetched
            .is_some_and(|f| covers(f, want, self.view.zoom))
        {
            return;
        }
        let bbox = limit_span(grow(want, 1.3));
        self.trails.inflight = true;
        let (tx, ctx, zoom) = (self.trails.tx.clone(), ctx.clone(), self.view.zoom);
        let send = move |r: Result<Vec<Way>, String>| {
            let _ = tx.send((bbox, zoom, r));
            ctx.request_repaint();
        };
        // SMF_TRAILS_FILE (development only): read an Overpass JSON reply from disk instead of the network.
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(f) = std::env::var("SMF_TRAILS_FILE") {
            send(
                std::fs::read_to_string(&f)
                    .map_err(|e| e.to_string())
                    .and_then(|t| smf_route::parse_overpass(&t)),
            );
            return;
        }
        fetch_trails(bbox, send);
    }

    /// A drawing click: with trail snapping on, the point (and the path from the previous
    /// point) follows trails; otherwise, or when no trail is close, it is the plain click.
    pub(crate) fn click_snapped(&mut self, editor: &mut Editor, ll: LngLat, tol_m: f64) {
        let drawing = matches!(editor.tool, Tool::Line | Tool::Polygon);
        let graph = self
            .trails
            .graph
            .clone()
            .filter(|_| self.trails.on && drawing);
        let Some(g) = graph else {
            editor.click(ll);
            return;
        };
        let routed = editor
            .draft
            .last()
            .copied()
            .and_then(|prev| g.route(prev, ll, tol_m));
        match routed {
            Some(r) => {
                for p in r.pts {
                    // Skip repeats (a double-click finishes the shape at the same spot).
                    if editor.draft.last() != Some(&p) {
                        editor.click(p);
                    }
                }
            }
            None => editor.click(g.snap(ll, tol_m).map_or(ll, |s| s.pos)),
        }
    }

    pub(crate) fn trails_dev(&mut self, editor: &mut Editor) {
        if self.trails.graph.is_some()
            && let Some(pts) = self.trails.dev_route.take()
        {
            editor.tool = Tool::Line;
            for p in pts {
                self.click_snapped(editor, p, 30.0);
            }
        }
    }

    pub(crate) fn trails_credit(&self) -> Option<&'static str> {
        self.trails.on.then_some("OpenStreetMap contributors")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_is_limited_but_small_boxes_untouched() {
        let small = [0.0, 0.0, 0.1, 0.1];
        assert_eq!(limit_span(small), small);
        let big = limit_span([0.0, 0.0, 1.0, 0.5]);
        assert!((big[2] - big[0]) <= MAX_SPAN_DEG + 1e-9);
        assert!((big[3] - big[1]) <= MAX_SPAN_DEG + 1e-9);
        assert!(((big[0] + big[2]) / 2.0 - 0.5).abs() < 1e-9);
    }
}
