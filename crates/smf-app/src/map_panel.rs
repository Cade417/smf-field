//! The interactive 2D map: input handling, tile pipeline glue, and overlays
//! (coordinates, scale bar, attribution). The renderer draws offscreen; the
//! resulting texture is shown as an egui image with egui-painted overlays on top.

use directories::ProjectDirs;
use eframe::egui::{self, Align2, Color32, FontId, PointerButton, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::egui_wgpu;
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

#[derive(Clone, Copy, PartialEq)]
enum CoordFormat {
    Dd,
    Dms,
    Utm,
}

pub struct MapPanel {
    sources: Vec<Arc<TileSource>>,
    selected: usize,
    view: MapView,
    renderer: MapRenderer,
    tiles: TileService,
    texture: Option<egui::TextureId>,
    failed: HashMap<TileKey, Instant>,
    velocity: Vec2,
    last_frame: Instant,
    coord_format: CoordFormat,
    render_state: egui_wgpu::RenderState,
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
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
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
                if ui.button("Reset north").clicked() {
                    self.view.angle = 0.0;
                }
                ui.weak(
                    "Drag: pan  |  Wheel: zoom  |  Right-drag: rotate  |  Double-click: zoom in",
                );
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.map_area(ui));
    }

    fn map_area(&mut self, ui: &mut egui::Ui) {
        let ppp = ui.ctx().pixels_per_point();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();
        self.view.size = [(rect.width() * ppp) as f64, (rect.height() * ppp) as f64];

        self.handle_input(ui, &resp, rect, ppp, dt);

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
        self.draw_overlays(ui, &resp, rect, ppp, &src);
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

    fn handle_input(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        rect: Rect,
        ppp: f32,
        dt: f32,
    ) {
        let to_px = |p: Pos2| {
            [
                ((p.x - rect.min.x) * ppp) as f64,
                ((p.y - rect.min.y) * ppp) as f64,
            ]
        };

        if resp.dragged_by(PointerButton::Primary) {
            let d = resp.drag_delta();
            self.view.pan_pixels((d.x * ppp) as f64, (d.y * ppp) as f64);
            // Smoothed release velocity (points per second) for inertia.
            if dt > 0.0 {
                self.velocity = self.velocity * 0.6 + (d / dt) * 0.4;
            }
        } else if resp.dragged_by(PointerButton::Secondary) {
            self.view.angle += resp.drag_delta().x as f64 * 0.006;
        } else if self.velocity.length() > 20.0 {
            let v = self.velocity * dt;
            self.view.pan_pixels((v.x * ppp) as f64, (v.y * ppp) as f64);
            self.velocity *= 0.93f32.powf(dt * 60.0);
            ui.ctx().request_repaint();
        } else {
            self.velocity = Vec2::ZERO;
        }
        if resp.drag_started() {
            self.velocity = Vec2::ZERO;
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
        }
        if resp.double_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.view
                .zoom_at(to_px(p), (self.view.zoom + 1.0).round(), MAX_ZOOM);
        }
    }

    fn draw_overlays(
        &self,
        ui: &egui::Ui,
        resp: &egui::Response,
        rect: Rect,
        ppp: f32,
        src: &TileSource,
    ) {
        let painter = ui.painter_at(rect);
        let font = FontId::proportional(12.0);
        let pill = |text: String, anchor: Align2, pos: Pos2| {
            let galley = painter.layout_no_wrap(text, font.clone(), Color32::BLACK);
            let r = anchor
                .anchor_size(pos, galley.size())
                .expand2(Vec2::new(6.0, 3.0));
            painter.rect_filled(r, 3.0, Color32::from_white_alpha(215));
            painter.galley(r.min + Vec2::new(6.0, 3.0), galley, Color32::BLACK);
        };

        // Attribution is a licence requirement for every provider, so it is always on screen.
        pill(
            src.attribution.clone(),
            Align2::RIGHT_BOTTOM,
            rect.right_bottom() - Vec2::new(10.0, 8.0),
        );

        if let Some(p) = resp.hover_pos() {
            let ll = self.view.screen_to_lnglat([
                ((p.x - rect.min.x) * ppp) as f64,
                ((p.y - rect.min.y) * ppp) as f64,
            ]);
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
        let lat = self.view.center.lat;
        let m_per_pt = meters_per_pixel(lat, self.view.zoom) * ppp as f64;
        let (len_m, label) = nice_scale(m_per_pt * 120.0);
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
        pill(label, Align2::LEFT_BOTTOM, base + Vec2::new(0.0, -8.0));
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
    let metric = if m >= 1000.0 {
        format!("{} km", m / 1000.0)
    } else {
        format!("{m} m")
    };
    let ft = m * 3.280_84;
    let imperial = if ft >= 5280.0 {
        format!("{:.1} mi", ft / 5280.0)
    } else {
        format!("{ft:.0} ft")
    };
    (m, format!("{metric} ({imperial})"))
}

#[cfg(test)]
mod tests {
    use super::nice_scale;

    #[test]
    fn scale_picks_round_numbers() {
        assert_eq!(nice_scale(1234.0).0, 1000.0);
        assert_eq!(nice_scale(2600.0).0, 2000.0);
        assert_eq!(nice_scale(7000.0).0, 5000.0);
        assert!(nice_scale(1000.0).1.starts_with("1 km"));
    }
}
