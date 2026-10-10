//! The interactive 2D map: input handling, tile pipeline glue, tools, and overlays.
//! The renderer draws offscreen; the texture is shown as an egui image with
//! egui-painted features and chrome on top.

use crate::{overlay, panels};
use directories::ProjectDirs;
use eframe::egui::{self, Align2, Color32, Key, PointerButton, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::egui_wgpu;
use smf_features::{Editor, Geometry, Hit, Tool, pick, snap};
use smf_geo::{LngLat, MapView, meters_per_pixel};
use smf_render::{MapRenderer, TileKey, plan_frame};
use smf_tiles::{TileEvent, TileService, TileSource, default_sources};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_ZOOM: f64 = 22.0;
/// Don't re-request a failed tile for a while so a dead server isn't hammered every frame.
const RETRY_AFTER: Duration = Duration::from_secs(15);
const MAX_UPLOADS_PER_FRAME: usize = 8;
const MAX_REQUESTS_PER_FRAME: usize = 160;
/// Pointer tolerance for picking/snapping, in egui points.
const PICK_TOL: f64 = 7.0;
const FREEHAND_STEP: f32 = 4.0;

#[derive(Clone, Copy, PartialEq)]
enum CoordFormat {
    Dd,
    Dms,
    Utm,
}

/// What a left-drag is currently doing, decided when the drag starts.
#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Pan,
    Vertex { id: u64, index: usize },
    Freehand,
}

pub struct MapPanel {
    sources: Vec<Arc<TileSource>>,
    selected: usize,
    pub view: MapView,
    renderer: MapRenderer,
    tiles: TileService,
    texture: Option<egui::TextureId>,
    failed: HashMap<TileKey, Instant>,
    velocity: Vec2,
    last_frame: Instant,
    coord_format: CoordFormat,
    render_state: egui_wgpu::RenderState,
    drag: Option<DragMode>,
    /// Set by the host to fit a bounding box on the next frame once the viewport size is known.
    pub pending_fit: Option<[f64; 4]>,
}

impl MapPanel {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rs = cc
            .wgpu_render_state
            .clone()
            .expect("wgpu render state (renderer must be wgpu)");
        let cache_dir = ProjectDirs::from("org", "ShowMeFire", "Field")
            .map(|d| d.cache_dir().join("tiles"))
            .unwrap_or_else(|| std::env::temp_dir().join("smf-field-tiles"));
        tracing::info!("tile cache: {}", cache_dir.display());
        let ctx = cc.egui_ctx.clone();
        let tiles = TileService::new(cache_dir, move || ctx.request_repaint());
        Self {
            sources: default_sources().into_iter().map(Arc::new).collect(),
            selected: 0,
            // Missouri: the home turf for Show Me Fire data.
            view: MapView::new(LngLat::new(-92.5, 38.5), 7.0, [1.0, 1.0]),
            renderer: MapRenderer::new(&rs.device, &rs.queue),
            tiles,
            texture: None,
            failed: HashMap::new(),
            velocity: Vec2::ZERO,
            last_frame: Instant::now(),
            coord_format: CoordFormat::Dd,
            render_state: rs,
            drag: None,
            pending_fit: None,
        }
    }

    pub fn basemap_id(&self) -> &str {
        &self.sources[self.selected].id
    }

    pub fn set_basemap(&mut self, id: &str) {
        if let Some(i) = self.sources.iter().position(|s| s.id == id) {
            self.selected = i;
        }
    }

    /// Fit a (min_lng, min_lat, max_lng, max_lat) box into the viewport with a margin.
    pub fn fit_bounds(&mut self, b: [f64; 4]) {
        let center = LngLat::new((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let (x0, y0) = smf_geo::lnglat_to_world(LngLat::new(b[0], b[3]), 0.0);
        let (x1, y1) = smf_geo::lnglat_to_world(LngLat::new(b[2], b[1]), 0.0);
        let (w, h) = ((x1 - x0).max(1e-9), (y1 - y0).max(1e-9));
        let avail = [self.view.size[0] * 0.8, self.view.size[1] * 0.8];
        let z = (avail[0] / w).min(avail[1] / h).log2();
        self.view.center = center;
        self.view.zoom = z.clamp(0.0, 17.0);
    }

    pub fn show(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Basemap");
                egui::ComboBox::from_id_salt("basemap")
                    .selected_text(&self.sources[self.selected].name)
                    .show_ui(ui, |ui| {
                        for (i, s) in self.sources.iter().enumerate() {
                            ui.selectable_value(&mut self.selected, i, &s.name);
                        }
                    });
                ui.separator();
                ui.label("Coords");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Dd, "DD");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Dms, "DMS");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Utm, "UTM");
                ui.separator();
                if ui.button("North up").clicked() {
                    self.view.angle = 0.0;
                }
            });
        });

        let mut zoom_to = None;
        egui::Panel::right("side")
            .default_size(290.0)
            .show(ui, |ui| {
                let a = panels::side_panel(ui, editor);
                zoom_to = a.zoom_to;
            });
        if let Some(id) = zoom_to
            && let Some(f) = editor.project.feature(id)
        {
            let v = f.geometry.vertices();
            let mut b = [v[0].lng, v[0].lat, v[0].lng, v[0].lat];
            for p in &v {
                b = [
                    b[0].min(p.lng),
                    b[1].min(p.lat),
                    b[2].max(p.lng),
                    b[3].max(p.lat),
                ];
            }
            // A single point (marker) would fit at max zoom; pad it to ~1 km.
            if (b[2] - b[0]).abs() < 1e-5 && (b[3] - b[1]).abs() < 1e-5 {
                b = [b[0] - 0.005, b[1] - 0.005, b[2] + 0.005, b[3] + 0.005];
            }
            self.pending_fit = Some(b);
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(tool_hint(editor));
                if let Some(m) = overlay::measure_summary(editor) {
                    ui.separator();
                    ui.label(egui::RichText::new(m).strong());
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.map_area(ui, editor));
    }

    fn map_area(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        let ppp = ui.ctx().pixels_per_point();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();
        self.view.size = [(rect.width() * ppp) as f64, (rect.height() * ppp) as f64];
        if let Some(b) = self.pending_fit.take() {
            self.fit_bounds(b);
        }

        self.handle_keys(ui, editor);
        self.handle_input(ui, &resp, rect, ppp, dt, editor);

        // Tile pipeline: bump generation, ingest finished tiles, plan, request, render.
        self.tiles.begin_frame();
        self.renderer.begin_frame();
        let mut uploads = 0;
        while uploads < MAX_UPLOADS_PER_FRAME {
            match self.tiles.try_recv() {
                Some(TileEvent::Ready { source, id, rgba }) => {
                    if let Some(si) = self.sources.iter().position(|s| s.id == source) {
                        self.renderer.upload_tile(
                            TileKey {
                                source: si as u16,
                                id,
                            },
                            &rgba,
                        );
                        uploads += 1;
                    }
                }
                Some(TileEvent::Failed { source, id }) => {
                    if let Some(si) = self.sources.iter().position(|s| s.id == source) {
                        self.failed.insert(
                            TileKey {
                                source: si as u16,
                                id,
                            },
                            Instant::now(),
                        );
                    }
                }
                None => break,
            }
        }
        if uploads == MAX_UPLOADS_PER_FRAME {
            ui.ctx().request_repaint(); // more tiles waiting in the channel
        }
        let src = self.sources[self.selected].clone();
        let plan = plan_frame(
            &mut self.renderer,
            &self.view,
            self.selected as u16,
            src.max_zoom,
        );
        for (key, prio) in plan.requests.iter().take(MAX_REQUESTS_PER_FRAME) {
            if self
                .failed
                .get(key)
                .is_some_and(|t| t.elapsed() < RETRY_AFTER)
            {
                continue;
            }
            self.tiles.request(&src, key.id, *prio);
        }

        let size = [
            self.view.size[0].max(1.0) as u32,
            self.view.size[1].max(1.0) as u32,
        ];
        if self.renderer.ensure_target(size) {
            self.register_texture();
        }
        self.renderer.render(&plan.quads);
        if let Some(id) = self.texture {
            ui.painter().image(
                id,
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        let painter = ui.painter_at(rect);
        let pj = overlay::Projector {
            view: &self.view,
            origin: rect.min,
            ppp,
        };
        overlay::draw_project(&painter, &pj, editor);
        let cursor_ll = resp
            .hover_pos()
            .map(|p| self.screen_to_lnglat(p, rect, ppp));
        overlay::draw_drafts(&painter, &pj, editor, cursor_ll);
        self.draw_chrome(ui, &resp, rect, ppp, &src);
    }

    fn screen_to_lnglat(&self, p: Pos2, rect: Rect, ppp: f32) -> LngLat {
        self.view.screen_to_lnglat([
            ((p.x - rect.min.x) * ppp) as f64,
            ((p.y - rect.min.y) * ppp) as f64,
        ])
    }

    /// (Re)register the offscreen target with egui; called whenever it is recreated on resize.
    fn register_texture(&mut self) {
        let Some(view) = self.renderer.target_view() else {
            return;
        };
        let mut r = self.render_state.renderer.write();
        if let Some(old) = self.texture.take() {
            r.free_texture(&old);
        }
        self.texture = Some(r.register_native_texture(
            &self.render_state.device,
            view,
            egui_wgpu::wgpu::FilterMode::Linear,
        ));
    }

    fn handle_keys(&mut self, ui: &egui::Ui, editor: &mut Editor) {
        if ui.ctx().egui_wants_keyboard_input() {
            return; // typing in a text box
        }
        let (undo, redo, del, enter, esc, back, tool) = ui.input(|i| {
            let ctrl = i.modifiers.command;
            let tool = [
                (Key::V, Tool::Select),
                (Key::M, Tool::Marker),
                (Key::L, Tool::Line),
                (Key::P, Tool::Polygon),
                (Key::R, Tool::RangeRing),
                (Key::T, Tool::Label),
                (Key::D, Tool::Measure),
                (Key::F, Tool::Freehand),
            ]
            .into_iter()
            .find(|(k, _)| !ctrl && i.key_pressed(*k))
            .map(|(_, t)| t);
            (
                ctrl && !i.modifiers.shift && i.key_pressed(Key::Z),
                ctrl && (i.key_pressed(Key::Y) || (i.modifiers.shift && i.key_pressed(Key::Z))),
                i.key_pressed(Key::Delete),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::Backspace),
                tool,
            )
        });
        if undo {
            editor.undo();
        }
        if redo {
            editor.redo();
        }
        if del {
            editor.delete_selected();
        }
        if enter {
            editor.finish();
        }
        if esc {
            editor.cancel();
            editor.selection = None;
        }
        if back {
            editor.draft.pop();
            editor.measure.pop();
        }
        if let Some(t) = tool {
            editor.cancel();
            editor.tool = t;
        }
    }

    fn handle_input(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        rect: Rect,
        ppp: f32,
        dt: f32,
        editor: &mut Editor,
    ) {
        let to_px = |p: Pos2| {
            [
                ((p.x - rect.min.x) * ppp) as f64,
                ((p.y - rect.min.y) * ppp) as f64,
            ]
        };
        let view = self.view;
        let proj = move |p: LngLat| view.lnglat_to_screen(p);
        let tol = PICK_TOL * ppp as f64;
        let shift = ui.input(|i| i.modifiers.shift);

        // Decide what a left-drag does the moment it starts.
        if resp.drag_started_by(PointerButton::Primary) {
            self.velocity = Vec2::ZERO;
            let start = ui.input(|i| i.pointer.press_origin()).map(to_px);
            self.drag = Some(DragMode::Pan);
            if let Some(pt) = start {
                if editor.tool == Tool::Freehand {
                    editor.draft.clear();
                    editor.freehand_point(self.view.screen_to_lnglat(pt));
                    self.drag = Some(DragMode::Freehand);
                } else if editor.tool == Tool::Select
                    && let Some(hit) = pick(&editor.project, editor.selection, &proj, pt, tol)
                    && let Some(f) = editor.project.feature(hit.id)
                {
                    let movable_body = matches!(
                        f.geometry,
                        Geometry::Marker(_) | Geometry::Label(_) | Geometry::RangeRing { .. }
                    );
                    let index = hit.vertex.or(movable_body.then_some(0));
                    if let Some(index) = index {
                        editor.selection = Some(hit.id);
                        editor.begin_edit(hit.id);
                        self.drag = Some(DragMode::Vertex { id: hit.id, index });
                    }
                }
            }
        }

        match self.drag {
            Some(DragMode::Vertex { id, index }) if resp.dragged_by(PointerButton::Primary) => {
                if let Some(p) = resp.interact_pointer_pos() {
                    let mut ll = self.screen_to_lnglat(p, rect, ppp);
                    if !shift && let Some(s) = snap_excluding(editor, id, &proj, to_px(p), tol) {
                        ll = s;
                    }
                    editor.drag_vertex(id, index, ll);
                }
            }
            Some(DragMode::Freehand) if resp.dragged_by(PointerButton::Primary) => {
                if let Some(p) = resp.interact_pointer_pos() {
                    let last = editor.draft.last().map(|l| self.view.lnglat_to_screen(*l));
                    let cur = to_px(p);
                    if last.is_none_or(|l| {
                        ((cur[0] - l[0]).hypot(cur[1] - l[1])) as f32 > FREEHAND_STEP * ppp
                    }) {
                        editor.freehand_point(self.view.screen_to_lnglat(cur));
                    }
                }
            }
            Some(DragMode::Pan) if resp.dragged_by(PointerButton::Primary) => {
                let d = resp.drag_delta();
                self.view.pan_pixels((d.x * ppp) as f64, (d.y * ppp) as f64);
                // Smoothed release velocity (points per second) for inertia.
                if dt > 0.0 {
                    self.velocity = self.velocity * 0.6 + (d / dt) * 0.4;
                }
            }
            _ => {}
        }
        if resp.drag_stopped_by(PointerButton::Primary) {
            match self.drag.take() {
                Some(DragMode::Vertex { .. }) => editor.end_edit(),
                Some(DragMode::Freehand) => editor.finish(),
                _ => {}
            }
        }

        if resp.dragged_by(PointerButton::Secondary) {
            self.view.angle += resp.drag_delta().x as f64 * 0.006;
        } else if !resp.dragged() && self.velocity.length() > 20.0 {
            let v = self.velocity * dt;
            self.view.pan_pixels((v.x * ppp) as f64, (v.y * ppp) as f64);
            self.velocity *= 0.93f32.powf(dt * 60.0);
            ui.ctx().request_repaint();
        } else if !resp.dragged() {
            self.velocity = Vec2::ZERO;
        }

        // Clicks: select / place points / finish shapes.
        if resp.double_clicked() && matches!(editor.tool, Tool::Line | Tool::Polygon) {
            editor.draft.pop(); // the second click of the double-click added a duplicate
            editor.finish();
        } else if resp.clicked_by(PointerButton::Primary)
            && let Some(p) = resp.interact_pointer_pos()
        {
            let pt = to_px(p);
            if editor.tool == Tool::Select {
                match pick(&editor.project, editor.selection, &proj, pt, tol) {
                    Some(Hit { id, .. }) => editor.selection = Some(id),
                    None => editor.selection = None,
                }
            } else {
                let mut ll = self.screen_to_lnglat(p, rect, ppp);
                if !shift && let Some(s) = snap(&editor.project, &proj, pt, tol) {
                    ll = s;
                }
                editor.click(ll);
            }
        } else if resp.double_clicked()
            && editor.tool == Tool::Select
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.view
                .zoom_at(to_px(p), (self.view.zoom + 1.0).round(), MAX_ZOOM);
        } else if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            // Right-click a selected vertex to delete it.
            if let Some(Hit {
                id,
                vertex: Some(i),
            }) = pick(&editor.project, editor.selection, &proj, to_px(p), tol)
            {
                editor.delete_vertex(id, i);
            }
        }

        if resp.hovered() {
            let (scroll, pinch) = ui
                .ctx()
                .input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let anchor = resp
                .hover_pos()
                .map(to_px)
                .unwrap_or([self.view.size[0] / 2.0, self.view.size[1] / 2.0]);
            let dz = scroll as f64 * 0.004 + (pinch as f64).log2();
            if dz.abs() > 1e-6 {
                self.view.zoom_at(anchor, self.view.zoom + dz, MAX_ZOOM);
                ui.ctx().request_repaint();
            }
            ui.ctx().set_cursor_icon(match editor.tool {
                Tool::Select => egui::CursorIcon::Default,
                _ => egui::CursorIcon::Crosshair,
            });
        }
    }

    fn draw_chrome(
        &self,
        ui: &egui::Ui,
        resp: &egui::Response,
        rect: Rect,
        ppp: f32,
        src: &TileSource,
    ) {
        let painter = ui.painter_at(rect);
        let pill = |text: String, anchor: Align2, pos: Pos2| {
            overlay::label(&painter, pos, anchor, &text, 12.0, Color32::BLACK)
        };

        // Attribution is a licence requirement for every provider, so it is always on screen.
        pill(
            src.attribution.clone(),
            Align2::RIGHT_BOTTOM,
            rect.right_bottom() - Vec2::new(10.0, 8.0),
        );

        if let Some(p) = resp.hover_pos() {
            let ll = self.screen_to_lnglat(p, rect, ppp);
            let text = match self.coord_format {
                CoordFormat::Dd => smf_geo::format_dd(ll),
                CoordFormat::Dms => smf_geo::format_dms(ll),
                CoordFormat::Utm => {
                    smf_geo::format_utm(ll).unwrap_or_else(|| "UTM n/a at this latitude".into())
                }
            };
            pill(
                text,
                Align2::LEFT_BOTTOM,
                rect.left_bottom() + Vec2::new(10.0, -8.0),
            );
        }

        // Scale bar: pick a round ground distance that is roughly 120 points wide.
        let m_per_pt = meters_per_pixel(self.view.center.lat, self.view.zoom) * ppp as f64;
        let (len_m, text) = nice_scale(m_per_pt * 120.0);
        let width = (len_m / m_per_pt) as f32;
        let base = rect.left_bottom() + Vec2::new(14.0, -42.0);
        let stroke = Stroke::new(2.5, Color32::BLACK);
        painter.line_segment(
            [base, base + Vec2::new(width, 0.0)],
            Stroke::new(5.0, Color32::from_white_alpha(200)),
        );
        painter.line_segment([base, base + Vec2::new(width, 0.0)], stroke);
        painter.line_segment(
            [base - Vec2::new(0.0, 5.0), base + Vec2::new(0.0, 5.0)],
            stroke,
        );
        painter.line_segment(
            [base + Vec2::new(width, -5.0), base + Vec2::new(width, 5.0)],
            stroke,
        );
        pill(text, Align2::LEFT_BOTTOM, base + Vec2::new(0.0, -8.0));
    }
}

/// Snap to other features' vertices only; snapping a dragged vertex to itself would pin it in place.
fn snap_excluding(
    editor: &Editor,
    id: u64,
    proj: &dyn Fn(LngLat) -> [f64; 2],
    pt: [f64; 2],
    tol: f64,
) -> Option<LngLat> {
    let mut p = editor.project.clone();
    p.features.retain(|f| f.id != id);
    snap(&p, proj, pt, tol)
}

fn tool_hint(editor: &Editor) -> String {
    match editor.tool {
        Tool::Select => "Select (V): click to select, drag handles to edit, Delete removes, right-click a vertex to delete it".into(),
        Tool::Marker => "Marker (M): click to place".into(),
        Tool::Line => format!("Line (L): click points, double-click or Enter to finish ({} pts)", editor.draft.len()),
        Tool::Polygon => format!("Polygon (P): click points, double-click or Enter to finish ({} pts)", editor.draft.len()),
        Tool::RangeRing => "Range rings (R): click to center rings".into(),
        Tool::Label => "Label (T): click to place text".into(),
        Tool::Measure => "Measure (D): click points; Esc clears, Backspace undoes a point".into(),
        Tool::Freehand => "Freehand (F): press and drag to draw a line".into(),
    }
}

/// Round `meters` down to 1/2/5 x 10^n and format as "x km (y mi)" / "x m (y ft)".
fn nice_scale(meters: f64) -> (f64, String) {
    let pow = 10f64.powf(meters.log10().floor());
    let n = meters / pow;
    let m = pow
        * if n >= 5.0 {
            5.0
        } else if n >= 2.0 {
            2.0
        } else {
            1.0
        };
    (m, smf_geo::format_distance(m))
}

#[cfg(test)]
mod tests {
    use super::nice_scale;

    #[test]
    fn scale_picks_round_numbers() {
        assert_eq!(nice_scale(1234.0).0, 1000.0);
        assert_eq!(nice_scale(2600.0).0, 2000.0);
        assert_eq!(nice_scale(7000.0).0, 5000.0);
        assert!(nice_scale(1000.0).1.starts_with("1.00 km"));
    }
}
