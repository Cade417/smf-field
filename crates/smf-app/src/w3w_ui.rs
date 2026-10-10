//! what3words in the app: a search box ("///filled.count.soap"), a three-word readout for
//! the cursor, and a 3 m grid layer. Everything here needs the user's own API key, entered
//! in the app and stored in the per-user config folder. Without a key the controls explain
//! what is missing instead of failing silently.

use crate::map_panel::MapPanel;
use crate::overlay::{self, Projector};
use eframe::egui::{self, Align2, Color32, Key, Pos2, Rect, Stroke};
use smf_features::Editor;
use smf_geo::LngLat;
use smf_w3w::{Line, Square, W3wClient, W3wError};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use web_time::Instant;

/// The 3 m grid is unreadable below this 2D zoom (squares under ~6 px).
const GRID_MIN_ZOOM: f64 = 18.0;
/// In 3D the grid is only drawn when the camera is this close.
const GRID_MAX_CAM_M: f64 = 500.0;
/// Pointer must rest this long before the readout asks the service (each ask is billed).
const HOVER_REST: Duration = Duration::from_millis(600);
/// Hard cap so a huge reply can't stall painting.
const MAX_GRID_LINES: usize = 6000;
const RED: Color32 = Color32::from_rgb(225, 11, 35);

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    #[serde(default)]
    key: String,
}

enum Msg {
    Search(Result<Square, W3wError>),
    Hover(Result<Square, W3wError>),
    Grid([f64; 4], Result<Vec<Line>, W3wError>),
}

pub struct W3wState {
    key: String,
    base: String,
    client: Option<Arc<W3wClient>>,
    pub dialog: bool,
    key_edit: String,
    pub query: String,
    pub found: Option<Square>,
    pub status: Option<(String, bool)>,
    pub grid_on: bool,
    /// Verification hooks (SMF_W3W_SEARCH / SMF_W3W_HOVER): run once on the first frame.
    pub dev_search: Option<String>,
    pub dev_hover: Option<LngLat>,
    grid: Vec<Line>,
    grid_box: Option<[f64; 4]>,
    grid_inflight: bool,
    /// Words for the square under the pointer; reused while the pointer stays inside it.
    pub hover: Option<Square>,
    hover_inflight: bool,
    hover_rest: Option<(Pos2, Instant)>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
}

impl W3wState {
    pub fn new() -> Self {
        let saved: Saved = crate::store::load_json("w3w.json");
        // W3W_API_KEY is for scripted runs (desktop); a key saved in the app wins.
        #[cfg(not(target_arch = "wasm32"))]
        let env_key = std::env::var("W3W_API_KEY").unwrap_or_default();
        #[cfg(target_arch = "wasm32")]
        let env_key = String::new();
        let key = if saved.key.is_empty() {
            env_key
        } else {
            saved.key
        };
        #[cfg(not(target_arch = "wasm32"))]
        let base = std::env::var("SMF_W3W_BASE").unwrap_or_else(|_| smf_w3w::DEFAULT_BASE.into());
        #[cfg(target_arch = "wasm32")]
        let base = smf_w3w::DEFAULT_BASE.to_string();
        let (tx, rx) = channel();
        let mut s = Self {
            key: String::new(),
            base,
            client: None,
            dialog: false,
            key_edit: String::new(),
            query: String::new(),
            found: None,
            status: None,
            grid_on: false,
            dev_search: None,
            dev_hover: None,
            grid: vec![],
            grid_box: None,
            grid_inflight: false,
            hover: None,
            hover_inflight: false,
            hover_rest: None,
            rx,
            tx,
        };
        s.set_key(key, false);
        s
    }

    pub fn has_key(&self) -> bool {
        self.client.is_some()
    }

    fn set_key(&mut self, key: String, persist: bool) {
        self.key = key.trim().to_string();
        self.client = W3wClient::new(&self.base, &self.key).ok().map(Arc::new);
        // A different key may belong to a different plan: forget results fetched with the old one.
        self.grid.clear();
        self.grid_box = None;
        self.hover = None;
        if persist {
            crate::store::save_json(
                "w3w.json",
                &Saved {
                    key: self.key.clone(),
                },
            );
        }
    }

    fn fail(&mut self, e: &W3wError) {
        self.status = Some((e.to_string(), true));
    }

    pub(crate) fn needs_key_message() -> &'static str {
        "Enter your what3words API key first (what3words menu)."
    }
}

fn square_bbox(s: &Square) -> [f64; 4] {
    // ~30 m of context around the 3 m square so the view isn't zoomed to the pixel.
    let pad = 0.0003;
    [
        s.sw.lng - pad,
        s.sw.lat - pad,
        s.ne.lng + pad,
        s.ne.lat + pad,
    ]
}

impl MapPanel {
    /// Search box, what3words menu. Called from the toolbar.
    pub(crate) fn w3w_toolbar(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.w3w.query)
                .hint_text("///what3words")
                .desired_width(140.0),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            self.w3w_search(ui.ctx());
        }
        ui.menu_button("what3words", |ui| {
            if ui
                .checkbox(&mut self.w3w.grid_on, "3 m grid layer")
                .changed()
                && self.w3w.grid_on
            {
                if !self.w3w.has_key() {
                    self.w3w.grid_on = false;
                    self.w3w.status = Some((W3wState::needs_key_message().into(), true));
                } else if self.mode == crate::map_panel::ViewMode::Flat
                    && self.view.zoom < GRID_MIN_ZOOM
                {
                    self.w3w.status = Some((
                        format!("The grid appears when you zoom in past level {GRID_MIN_ZOOM:.0}."),
                        false,
                    ));
                }
            }
            if ui.button("API key...").clicked() {
                self.w3w.key_edit = self.w3w.key.clone();
                self.w3w.dialog = true;
                ui.close();
            }
            ui.add_enabled_ui(self.w3w.found.is_some(), |ui| {
                if ui.button("Add found square as marker").clicked() {
                    self.w3w_add_marker(editor);
                    ui.close();
                }
                if ui.button("Clear found square").clicked() {
                    self.w3w.found = None;
                    ui.close();
                }
            });
        });
        if let Some((msg, err)) = &self.w3w.status {
            let c = if *err {
                Color32::from_rgb(230, 120, 90)
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(c, msg);
        }
    }

    fn w3w_search(&mut self, ctx: &egui::Context) {
        let q = self.w3w.query.clone();
        if q.trim().is_empty() {
            return;
        }
        if !smf_w3w::looks_like_words(&q) {
            self.w3w.status = Some((
                "Type a three-word address like ///filled.count.soap".into(),
                true,
            ));
            return;
        }
        let Some(client) = self.w3w.client.clone() else {
            self.w3w.status = Some((W3wState::needs_key_message().into(), true));
            return;
        };
        if self.work_offline {
            self.w3w.status = Some((
                "what3words needs the internet; turn off Work offline.".into(),
                true,
            ));
            return;
        }
        self.w3w.status = Some(("Searching...".into(), false));
        let (tx, ctx) = (self.w3w.tx.clone(), ctx.clone());
        client.to_square(&q, move |r| {
            let _ = tx.send(Msg::Search(r));
            ctx.request_repaint();
        });
    }

    pub(crate) fn w3w_add_marker(&mut self, editor: &mut Editor) {
        if let Some(sq) = &self.w3w.found {
            let id = editor.add(
                smf_features::Geometry::Marker(sq.center),
                Some(format!("///{}", sq.words)),
            );
            editor.selection = Some(id);
        }
    }

    /// Deliver finished requests and keep the grid and the hover readout fed.
    pub(crate) fn poll_w3w(&mut self, ctx: &egui::Context) {
        if let Some(q) = self.w3w.dev_search.take() {
            self.w3w.query = q;
            self.w3w_search(ctx);
        }
        if let Some(ll) = self.w3w.dev_hover.take()
            && let Some(client) = self.w3w.client.clone()
        {
            let (tx, ctx) = (self.w3w.tx.clone(), ctx.clone());
            client.to_words(ll, move |r| {
                let _ = tx.send(Msg::Hover(r));
                ctx.request_repaint();
            });
        }
        while let Ok(m) = self.w3w.rx.try_recv() {
            match m {
                Msg::Search(Ok(sq)) => {
                    self.w3w.status = None;
                    self.pending_fit = Some(square_bbox(&sq));
                    self.w3w.found = Some(sq);
                }
                Msg::Search(Err(e)) => self.w3w.fail(&e),
                Msg::Hover(r) => {
                    self.w3w.hover_inflight = false;
                    match r {
                        Ok(sq) => self.w3w.hover = Some(sq),
                        Err(e) => self.w3w.fail(&e),
                    }
                }
                Msg::Grid(b, r) => {
                    self.w3w.grid_inflight = false;
                    match r {
                        Ok(mut lines) => {
                            lines.truncate(MAX_GRID_LINES);
                            self.w3w.grid = lines;
                            self.w3w.grid_box = Some(b);
                            self.w3w.status = None;
                        }
                        Err(e) => {
                            // Stop asking: a bad key or plan would otherwise retry every frame.
                            self.w3w.grid_on = false;
                            self.w3w.fail(&e);
                        }
                    }
                }
            }
        }
        self.w3w_grid_tick(ctx);
    }

    fn grid_visible(&self) -> bool {
        self.w3w.grid_on
            && match self.mode {
                crate::map_panel::ViewMode::Flat => self.view.zoom >= GRID_MIN_ZOOM,
                crate::map_panel::ViewMode::Terrain => self.cam.distance_m <= GRID_MAX_CAM_M,
            }
    }

    /// Box to fetch: the visible area plus a margin, kept inside the API's size limit.
    fn grid_wanted_box(&self) -> [f64; 4] {
        let (c, half_m) = match self.mode {
            crate::map_panel::ViewMode::Flat => {
                let [w, h] = self.view.size;
                let m_px = smf_geo::meters_per_pixel(self.view.center.lat, self.view.zoom);
                (self.view.center, 0.5 * w.hypot(h) * m_px)
            }
            crate::map_panel::ViewMode::Terrain => (self.cam.target, self.cam.distance_m * 1.5),
        };
        let half_m = (half_m * 1.2).min(smf_w3w::MAX_GRID_DIAGONAL_M * 0.45);
        let dlat = half_m / 111_320.0;
        let dlng = dlat / c.lat.to_radians().cos().max(0.05);
        smf_w3w::clamp_grid_box([c.lng - dlng, c.lat - dlat, c.lng + dlng, c.lat + dlat])
    }

    fn w3w_grid_tick(&mut self, ctx: &egui::Context) {
        if !self.grid_visible() || self.w3w.grid_inflight || self.work_offline {
            return;
        }
        let Some(client) = self.w3w.client.clone() else {
            return;
        };
        let c = match self.mode {
            crate::map_panel::ViewMode::Flat => self.view.center,
            crate::map_panel::ViewMode::Terrain => self.cam.target,
        };
        // Before the first layout the viewport has no size, which would request a box of a few
        // centimetres and then never refresh it.
        if self.view.size[0] < 200.0 {
            return;
        }
        let b = self.grid_wanted_box();
        // Re-fetch once the view centre leaves the middle half of the last box, or the window
        // grew so the last box no longer covers it (the clamped maximum never counts as growth).
        if let Some(old) = self.w3w.grid_box {
            let (mx, my) = ((old[2] - old[0]) * 0.25, (old[3] - old[1]) * 0.25);
            let inside = c.lng > old[0] + mx
                && c.lng < old[2] - mx
                && c.lat > old[1] + my
                && c.lat < old[3] - my;
            let grew = (b[2] - b[0]) > (old[2] - old[0]) * 1.05;
            if inside && !grew {
                return;
            }
        }
        self.w3w.grid_inflight = true;
        let (tx, ctx) = (self.w3w.tx.clone(), ctx.clone());
        client.grid(b, move |r| {
            let _ = tx.send(Msg::Grid(b, r));
            ctx.request_repaint();
        });
    }

    /// Ask for the words under a resting pointer (only when the readout shows them).
    pub(crate) fn w3w_hover_tick(
        &mut self,
        hover: Option<Pos2>,
        at: Option<LngLat>,
        ctx: &egui::Context,
    ) {
        let (Some(h), Some(ll)) = (hover, at) else {
            self.w3w.hover_rest = None;
            return;
        };
        if !self.w3w.has_key() || self.work_offline {
            return;
        }
        if self.w3w.hover.as_ref().is_some_and(|s| s.contains(ll)) {
            return;
        }
        match self.w3w.hover_rest {
            Some((p, _)) if (p - h).length() < 6.0 => {}
            _ => {
                self.w3w.hover_rest = Some((h, Instant::now()));
                ctx.request_repaint_after(HOVER_REST);
                return;
            }
        }
        let rested = self
            .w3w
            .hover_rest
            .map(|(_, t)| t.elapsed())
            .unwrap_or_default();
        if rested < HOVER_REST {
            ctx.request_repaint_after(HOVER_REST - rested);
        } else if !self.w3w.hover_inflight
            && let Some(client) = self.w3w.client.clone()
        {
            self.w3w.hover_inflight = true;
            let (tx, ctx) = (self.w3w.tx.clone(), ctx.clone());
            client.to_words(ll, move |r| {
                let _ = tx.send(Msg::Hover(r));
                ctx.request_repaint();
            });
        }
    }

    /// Text for the coordinate readout in what3words mode.
    pub(crate) fn w3w_readout(&self, ll: LngLat) -> String {
        if !self.w3w.has_key() {
            return "///  (add a what3words API key)".into();
        }
        match &self.w3w.hover {
            Some(s) if s.contains(ll) => format!("///{}", s.words),
            _ => "///...".into(),
        }
    }

    pub(crate) fn draw_w3w(&self, painter: &egui::Painter, pj: &Projector, rect: Rect) {
        if self.grid_visible() {
            let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(225, 11, 35, 150));
            let visible = rect.expand(40.0);
            for l in &self.w3w.grid {
                for seg in pj.polyline(l) {
                    // Both ends can be off-screen while the line still crosses the view.
                    if Rect::from_points(&seg).intersects(visible) {
                        painter.add(egui::Shape::line(seg, stroke));
                    }
                }
            }
        }
        if let Some(sq) = &self.w3w.found {
            let ring = sq.ring();
            for seg in pj.polyline(&ring) {
                painter.add(egui::Shape::line(
                    seg.clone(),
                    Stroke::new(5.0, Color32::WHITE),
                ));
                painter.add(egui::Shape::line(seg, Stroke::new(2.5, RED)));
            }
            if let Some(p) = pj.pos(sq.center) {
                painter.circle_filled(p, 3.5, RED);
                overlay::label(
                    painter,
                    p + egui::vec2(0.0, -14.0),
                    Align2::CENTER_BOTTOM,
                    &format!("///{}", sq.words),
                    13.0,
                    RED,
                );
            }
        }
    }

    pub(crate) fn w3w_window(&mut self, ctx: &egui::Context) {
        if !self.w3w.dialog {
            return;
        }
        let mut open = true;
        let mut save = false;
        egui::Window::new("what3words API key")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(
                    "what3words only works with your own API key. The convert and grid \
                     features need a paid plan; the free plan does not include them.",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.w3w.key_edit)
                        .password(true)
                        .hint_text("API key")
                        .desired_width(320.0),
                );
                ui.weak("Stored in this app's settings folder on this computer, unencrypted. Sent only to what3words.");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Remove key").clicked() {
                        self.w3w.key_edit.clear();
                        save = true;
                    }
                });
            });
        if save {
            let k = std::mem::take(&mut self.w3w.key_edit);
            self.w3w.set_key(k, true);
            self.w3w.status = None;
            open = false;
        }
        self.w3w.dialog = open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_saved_reloaded_and_removable() {
        let dir = crate::store::use_temp_root("w3w");
        let path = dir.join("config").join("w3w.json");
        let mut a = W3wState::new();
        assert!(!a.has_key() || std::env::var_os("W3W_API_KEY").is_some());
        a.set_key("  secret-key  ".into(), true);
        assert!(a.has_key());
        // A new run finds the key saved by the dialog (trimmed), with no environment variable involved.
        let b = W3wState::new();
        assert!(b.has_key());
        assert_eq!(b.key, "secret-key");
        // Removing the key empties the file, so the next run has none from disk.
        let mut c = W3wState::new();
        c.set_key(String::new(), true);
        assert!(!c.has_key());
        let saved: Saved = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.key.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn square_box_pads_the_square() {
        let sq = Square {
            words: "a.b.c".into(),
            center: LngLat::new(1.0, 2.0),
            sw: LngLat::new(0.99998, 1.99998),
            ne: LngLat::new(1.00002, 2.00002),
            nearest_place: String::new(),
            country: String::new(),
        };
        let b = square_bbox(&sq);
        assert!(b[0] < sq.sw.lng && b[2] > sq.ne.lng && b[1] < sq.sw.lat && b[3] > sq.ne.lat);
    }
}
