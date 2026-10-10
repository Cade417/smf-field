//! The interactive map: 2D and 3D views, input handling, tile pipeline glue, tools,
//! and overlays. The renderer draws offscreen; the texture is shown as an egui
//! image with egui-painted features and chrome on top.

use crate::earth_ui::EarthUi;
use crate::ext_ui::ExtState;
use crate::gps_ui::GpsState;
use crate::layers::LayerStore;
use crate::layers_ui::{ActiveDownload, DownloadUi, OverlayLayer};
use crate::live_ui::LiveState;
use crate::print_ui::PrintUi;
use crate::smf_ui::SmfState;
use crate::team_ui::TeamState;
use crate::trails_ui::TrailState;
use crate::w3w_ui::W3wState;
use crate::{overlay, panels};
use directories::ProjectDirs;
use eframe::egui::{self, Align2, Color32, Key, PointerButton, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::egui_wgpu;
use smf_features::{Editor, Geometry, Hit, Tool, pick, snap};
use smf_geo::{LngLat, MapView, TileId, meters_per_pixel};
use smf_render::{MapRenderer, ShadeParams, TileKey, plan_frame};
use smf_terrain::{Camera3D, HeightField, decode_terrarium, select_chunks};
use smf_tiles::{OfflineArea, TileEvent, TileService, TileSource, basemaps, terrain_source};
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
/// Keeps terrain failures separate from basemap failures in the retry-backoff map.
const TERRAIN_FAIL_SOURCE: u16 = u16::MAX;
/// Mesh cells larger than this many pixels get subdivided.
const SPLIT_PX: f64 = 10.0;
const MAX_CHUNKS: usize = 350;

#[derive(Clone, Copy, PartialEq)]
enum CoordFormat {
    Dd,
    Dms,
    Utm,
    W3w,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Flat,
    Terrain,
}

/// What a left-drag is currently doing, decided when the drag starts.
#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Pan,
    Vertex { id: u64, index: usize },
    Freehand,
}

/// Mode-aware conversion between screen pixels and geography for picking and tools.
struct Proj<'a> {
    mode: ViewMode,
    view: MapView,
    cam: Camera3D,
    hf: &'a HeightField,
}

impl Proj<'_> {
    fn project(&self, ll: LngLat) -> [f64; 2] {
        match self.mode {
            ViewMode::Flat => self.view.lnglat_to_screen(ll),
            ViewMode::Terrain => {
                let e = self.hf.sample(ll).unwrap_or(0.0) as f64;
                // Far off-screen so behind-camera points never match a pick.
                self.cam.project(ll, e).unwrap_or([-1e9, -1e9])
            }
        }
    }

    /// Geographic point under a pixel; in 3D this is where the ray meets loaded terrain.
    fn unproject(&self, px: [f64; 2]) -> Option<LngLat> {
        match self.mode {
            ViewMode::Flat => Some(self.view.screen_to_lnglat(px)),
            ViewMode::Terrain => self.cam.pick_ground(px, self.hf).map(|(l, _)| l),
        }
    }
}

pub struct MapPanel {
    pub(crate) sources: Vec<Arc<TileSource>>,
    pub(crate) selected: usize,
    pub(crate) terrain_src: Arc<TileSource>,
    pub view: MapView,
    pub cam: Camera3D,
    pub mode: ViewMode,
    pub shade2d: ShadeParams,
    pub shade3d: ShadeParams,
    renderer: MapRenderer,
    pub(crate) tiles: TileService,
    texture: Option<egui::TextureId>,
    failed: HashMap<TileKey, Instant>,
    pub(crate) heights: HeightField,
    velocity: Vec2,
    last_frame: Instant,
    coord_format: CoordFormat,
    pub(crate) render_state: egui_wgpu::RenderState,
    drag: Option<DragMode>,
    /// Set by the host to fit a bounding box on the next frame once the viewport size is known.
    pub pending_fit: Option<[f64; 4]>,
    cursor_elev: Option<f32>,
    /// Slope degrees and the compass bearing it faces, under the pointer.
    cursor_slope: Option<(f64, f64)>,
    /// Draw-call stats shown in the status bar (3D).
    last_chunks: usize,
    pub(crate) overlays: Vec<OverlayLayer>,
    /// Custom layers removed this session (indices into `sources` must stay stable).
    pub(crate) hidden: std::collections::HashSet<String>,
    pub(crate) store: LayerStore,
    pub(crate) add_input: Option<crate::layers::AddLayerInput>,
    pub(crate) add_error: Option<String>,
    pub(crate) dl: DownloadUi,
    pub(crate) areas: Vec<OfflineArea>,
    pub(crate) offline_dir: std::path::PathBuf,
    pub(crate) work_offline: bool,
    pub(crate) downloads: Vec<ActiveDownload>,
    pub(crate) was_downloading: bool,
    pub(crate) notice: Option<(String, Instant)>,
    pub(crate) print: PrintUi,
    pub(crate) smf: SmfState,
    pub(crate) w3w: W3wState,
    pub(crate) ext: ExtState,
    pub(crate) gps: GpsState,
    pub(crate) live: LiveState,
    pub(crate) team: TeamState,
    /// Nearest vertex of the selected feature while navigating (set each frame for drawing).
    pub(crate) gps_target: Option<(LngLat, String)>,
    pub(crate) earth: EarthUi,
    pub(crate) trails: TrailState,
    /// SMF_COND verification hook: query conditions here on the first frame.
    pub dev_conditions: Option<LngLat>,
    pub(crate) cache_dir: std::path::PathBuf,
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
        let tiles = TileService::new(cache_dir.clone(), move || ctx.request_repaint());
        let center = LngLat::new(-92.5, 38.5);
        let dirs = ProjectDirs::from("org", "ShowMeFire", "Field");
        let offline_dir = dirs
            .as_ref()
            .map(|d| d.data_local_dir().join("offline"))
            .unwrap_or_else(|| std::env::temp_dir().join("smf-field-offline"));
        let store = LayerStore::new(
            dirs.as_ref()
                .map(|d| d.config_dir().join("layers.json"))
                .unwrap_or_else(|| std::env::temp_dir().join("smf-field-layers.json")),
        );
        let mut panel = Self {
            sources: basemaps().into_iter().map(Arc::new).collect(),
            selected: 0,
            terrain_src: Arc::new(terrain_source()),
            // Missouri: the home turf for Show Me Fire data.
            view: MapView::new(center, 7.0, [1.0, 1.0]),
            cam: Camera3D {
                target: center,
                ..Camera3D::default()
            },
            mode: ViewMode::Flat,
            shade2d: ShadeParams::default(),
            // Without lighting, 3D terrain reads as a flat picture, so shade by default.
            shade3d: ShadeParams {
                hillshade: true,
                hill_strength: 0.75,
                ..ShadeParams::default()
            },
            renderer: MapRenderer::new(&rs.device, &rs.queue),
            tiles,
            texture: None,
            failed: HashMap::new(),
            heights: HeightField::default(),
            velocity: Vec2::ZERO,
            last_frame: Instant::now(),
            coord_format: CoordFormat::Dd,
            render_state: rs,
            drag: None,
            pending_fit: None,
            cursor_elev: None,
            cursor_slope: None,
            last_chunks: 0,
            overlays: vec![],
            hidden: Default::default(),
            store,
            add_input: None,
            add_error: None,
            dl: DownloadUi::default(),
            areas: vec![],
            offline_dir,
            work_offline: false,
            downloads: vec![],
            was_downloading: false,
            notice: None,
            print: PrintUi::default(),
            smf: SmfState::new(&cc.egui_ctx),
            w3w: W3wState::new(),
            ext: ExtState::new(),
            gps: GpsState::new(),
            live: LiveState::new(),
            team: TeamState::new(),
            gps_target: None,
            earth: EarthUi::default(),
            trails: TrailState::new(),
            dev_conditions: None,
            cache_dir,
        };
        panel.init_layers();
        panel
    }

    pub fn basemap_id(&self) -> &str {
        &self.sources[self.selected].id
    }

    pub fn set_basemap(&mut self, id: &str) {
        if let Some(i) = self.sources.iter().position(|s| s.id == id) {
            self.selected = i;
        }
    }

    /// Switch 2D <-> 3D keeping the same place and (roughly) the same scale and direction.
    pub fn set_mode(&mut self, mode: ViewMode) {
        if mode == self.mode {
            return;
        }
        self.velocity = Vec2::ZERO;
        match mode {
            ViewMode::Terrain => {
                self.cam.viewport = self.view.size;
                self.cam.target = self.view.center;
                self.cam.target_elev_m =
                    self.heights.sample(self.view.center).unwrap_or(0.0) as f64;
                self.cam.distance_m = self.cam.distance_for_zoom(self.view.zoom);
                self.cam.heading = (-self.view.angle).rem_euclid(std::f64::consts::TAU);
                self.cam.pitch = 55f64.to_radians();
            }
            ViewMode::Flat => {
                self.view.center = self.cam.target;
                self.view.zoom = self.cam.zoom_for_distance().clamp(0.0, MAX_ZOOM);
                self.view.angle = -self.cam.heading;
            }
        }
        self.mode = mode;
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
        self.view.zoom = z.clamp(0.0, 19.0);
        // Same framing for the 3D camera.
        self.cam.target = center;
        self.cam.target_elev_m = self.heights.sample(center).unwrap_or(0.0) as f64;
        self.cam.distance_m = self.cam.distance_for_zoom(self.view.zoom) * 1.1;
    }

    fn active_shade(&mut self) -> &mut ShadeParams {
        match self.mode {
            ViewMode::Flat => &mut self.shade2d,
            ViewMode::Terrain => &mut self.shade3d,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        self.poll_downloads();
        self.poll_smf(ui.ctx());
        self.poll_w3w(ui.ctx());
        self.poll_ext(ui.ctx());
        self.poll_gps(ui.ctx());
        self.poll_live(ui.ctx());
        self.team_tick(ui.ctx(), editor);
        self.poll_trails(ui.ctx());
        self.trails_dev(editor);
        if let Some(ll) = self.dev_conditions.take() {
            self.query_conditions(ll, ui.ctx());
        }
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Basemap");
                egui::ComboBox::from_id_salt("basemap")
                    .selected_text(&self.sources[self.selected].name)
                    .show_ui(ui, |ui| {
                        for (i, s) in self.sources.iter().enumerate() {
                            if !s.overlay && !self.hidden.contains(&s.id) {
                                ui.selectable_value(&mut self.selected, i, &s.name);
                            }
                        }
                    });
                ui.separator();
                let mut want = self.mode;
                ui.selectable_value(&mut want, ViewMode::Flat, "2D");
                ui.selectable_value(&mut want, ViewMode::Terrain, "3D");
                self.set_mode(want);
                ui.menu_button("Show Me Fire", |ui| self.smf_menu(ui, editor));
                ui.menu_button("Layers", |ui| self.layers_menu(ui));
                ui.menu_button("Fire & snow", |ui| self.ext_menu(ui));
                ui.menu_button("GPS", |ui| self.gps_menu(ui, editor));
                ui.menu_button("Tracking", |ui| self.live_menu(ui));
                ui.menu_button("Team", |ui| self.team_menu(ui, editor));
                ui.menu_button("Offline", |ui| self.offline_menu(ui));
                ui.menu_button("Shading", |ui| self.shading_menu(ui));
                self.w3w_toolbar(ui, editor);
                ui.separator();
                ui.label("Coords");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Dd, "DD");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Dms, "DMS");
                ui.selectable_value(&mut self.coord_format, CoordFormat::Utm, "UTM");
                ui.selectable_value(&mut self.coord_format, CoordFormat::W3w, "///");
                ui.separator();
                if ui.button("North up").clicked() {
                    self.view.angle = 0.0;
                    self.cam.heading = 0.0;
                }
            });
        });

        let mut action = panels::PanelAction {
            zoom_to: None,
            profile_tiles: vec![],
        };
        egui::Panel::right("side")
            .default_size(290.0)
            .show(ui, |ui| {
                action = panels::side_panel(ui, editor, &self.heights);
            });
        for id in std::mem::take(&mut action.profile_tiles) {
            self.request_terrain_tile(id, -5.0);
        }
        if let Some(id) = action.zoom_to
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

        let gps_text = self.gps_status_text(editor);
        // Vertical gain along the measured line, once its elevation tiles are loaded.
        let measure_gain = if editor.measure.len() >= 2 {
            let path = editor.measure.clone();
            for id in smf_terrain::profile_tiles(&path, 12) {
                if !self.heights.contains(id) {
                    self.request_terrain_tile(id, -5.0);
                }
            }
            let p = smf_terrain::elevation_profile(&self.heights, &path, 160);
            (p.coverage > 0.0).then(|| {
                format!(
                    "Gain +{:.0} m / Loss -{:.0} m{}",
                    p.gain_m,
                    p.loss_m,
                    if p.coverage < 1.0 { " (loading)" } else { "" }
                )
            })
        } else {
            None
        };
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(t) = &gps_text {
                    ui.label(
                        egui::RichText::new(t)
                            .strong()
                            .color(Color32::from_rgb(90, 160, 255)),
                    );
                }
                if gps_text.is_some() {
                    ui.separator();
                }
                ui.label(tool_hint(editor));
                if let Some(m) = overlay::measure_summary(editor) {
                    ui.separator();
                    ui.label(egui::RichText::new(m).strong());
                }
                if let Some(g) = &measure_gain {
                    ui.separator();
                    ui.label(egui::RichText::new(g).strong());
                }
                if self.work_offline {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("OFFLINE")
                            .strong()
                            .color(Color32::from_rgb(230, 160, 60)),
                    );
                }
                if let Some((msg, _)) = &self.smf.feedback {
                    ui.separator();
                    ui.label(msg);
                }
                if let Some(c) = &self.smf.conditions {
                    ui.separator();
                    ui.add(egui::Label::new(c).truncate());
                }
                if let Some((msg, _)) = &self.notice {
                    ui.separator();
                    ui.label(msg);
                }
                if let Some(e) = self.cursor_elev {
                    ui.separator();
                    ui.label(format!("Elev {e:.0} m ({:.0} ft)", e * 3.280_84));
                }
                if let Some((deg, aspect)) = self.cursor_slope {
                    ui.separator();
                    ui.label(format!("Slope {deg:.0}° facing {}", compass(aspect)));
                }
                if self.mode == ViewMode::Terrain {
                    ui.separator();
                    ui.weak(format!(
                        "3D: {} chunks | pitch {:.0}° | heading {:.0}° | {} m out",
                        self.last_chunks,
                        self.cam.pitch.to_degrees(),
                        self.cam.heading.to_degrees(),
                        self.cam.distance_m as i64
                    ));
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.map_area(ui, editor));
        self.layer_windows(ui.ctx(), editor);
        self.print_window(ui.ctx(), editor);
        self.smf_windows(ui.ctx(), editor);
        self.w3w_window(ui.ctx());
        self.live_window(ui.ctx());
        self.earth_window(ui.ctx());
    }

    fn shading_menu(&mut self, ui: &mut egui::Ui) {
        let mode = self.mode;
        let p = self.active_shade();
        ui.checkbox(&mut p.hillshade, "Hillshade");
        ui.add_enabled(
            p.hillshade,
            egui::Slider::new(&mut p.hill_strength, 0.1..=1.0).text("strength"),
        );
        ui.checkbox(&mut p.slope, "Slope angle shading");
        ui.add_enabled(
            p.slope,
            egui::Slider::new(&mut p.slope_alpha, 0.1..=1.0).text("opacity"),
        );
        ui.add_enabled_ui(p.slope, |ui| {
            ui.radio_value(
                &mut p.slope_fire_preset,
                false,
                "Degrees: 27 / 30 / 35 / 40",
            );
            ui.radio_value(
                &mut p.slope_fire_preset,
                true,
                "Fire grade: 20 / 40 / 60 / 80 %",
            );
        });
        ui.separator();
        ui.add(egui::Slider::new(&mut p.sun_azimuth_deg, 0.0..=360.0).text("sun azimuth"));
        ui.add(egui::Slider::new(&mut p.sun_altitude_deg, 5.0..=80.0).text("sun altitude"));
        if mode == ViewMode::Terrain {
            ui.add(egui::Slider::new(&mut p.exaggeration, 1.0..=4.0).text("vertical exaggeration"));
        }
        ui.weak("Terrain: Mapzen / USGS 3DEP / SRTM");
    }

    fn map_area(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        let ppp = ui.ctx().pixels_per_point();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();
        self.view.size = [(rect.width() * ppp) as f64, (rect.height() * ppp) as f64];
        self.cam.viewport = self.view.size;
        if let Some(b) = self.pending_fit.take() {
            self.fit_bounds(b);
        }

        self.handle_keys(ui, editor);
        self.handle_input(ui, &resp, rect, ppp, dt, editor);

        // Tile pipeline: bump generation, ingest finished tiles, plan, request, render.
        self.tiles.begin_frame();
        self.renderer.begin_frame();
        self.ingest_tiles(ui.ctx());

        let src = self.sources[self.selected].clone();
        let size = [
            self.view.size[0].max(1.0) as u32,
            self.view.size[1].max(1.0) as u32,
        ];
        if self.renderer.ensure_target(size) {
            self.register_texture();
        }
        match self.mode {
            ViewMode::Flat => self.render_flat(),
            ViewMode::Terrain => self.render_terrain(),
        }
        if let Some(id) = self.texture {
            ui.painter().image(
                id,
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        let painter = ui.painter_at(rect);
        let pj = match self.mode {
            ViewMode::Flat => overlay::Projector::Flat {
                view: &self.view,
                origin: rect.min,
                ppp,
            },
            ViewMode::Terrain => overlay::Projector::Terrain {
                cam: &self.cam,
                hf: &self.heights,
                origin: rect.min,
                ppp,
            },
        };
        self.draw_smf_data(&painter, &pj, rect);
        self.draw_ext(&painter, &pj, rect);
        self.draw_live(&painter, &pj, rect);
        self.draw_team(&painter, &pj, rect);
        self.draw_gps(&painter, &pj);
        self.draw_w3w(&painter, &pj, rect);
        overlay::draw_project(&painter, &pj, editor);
        let proj = self.proj();
        let cursor_ll = resp
            .hover_pos()
            .and_then(|p| proj.unproject(to_px(p, rect, ppp)));
        overlay::draw_drafts(&painter, &pj, editor, cursor_ll);
        self.cursor_elev = cursor_ll.and_then(|ll| self.heights.sample(ll));
        self.cursor_slope = cursor_ll.and_then(|ll| self.heights.slope_aspect(ll));
        if self.mode == ViewMode::Flat
            && let Some(ll) = cursor_ll
        {
            // Hovering is enough to fetch the elevation tile for the readout.
            let z = (self.view.zoom.round() as u8).clamp(6, 13);
            self.request_terrain_tile(TileId::from_lnglat(ll, z), -1.0);
        }
        self.draw_chrome(ui, cursor_ll, rect, ppp, &src);
        self.draw_smf_legends(&painter, rect);
        self.smf_conditions_tick(resp.hover_pos(), cursor_ll, ui.ctx());
        if self.coord_format == CoordFormat::W3w {
            self.w3w_hover_tick(resp.hover_pos(), cursor_ll, ui.ctx());
        }
    }

    fn proj(&self) -> Proj<'_> {
        Proj {
            mode: self.mode,
            view: self.view,
            cam: self.cam,
            hf: &self.heights,
        }
    }

    /// Drain finished tiles into the GPU pools, within a per-frame budget.
    fn ingest_tiles(&mut self, ctx: &egui::Context) {
        let mut uploads = 0;
        while uploads < MAX_UPLOADS_PER_FRAME {
            match self.tiles.try_recv() {
                Some(TileEvent::Ready { source, id, rgba }) => {
                    if source == self.terrain_src.id {
                        let t = decode_terrarium(&rgba);
                        self.renderer.upload_height(id, &t.heights);
                        self.heights.insert(id, t);
                        uploads += 1;
                    } else if let Some(si) = self.sources.iter().position(|s| s.id == source) {
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
                    let si = if source == self.terrain_src.id {
                        Some(TERRAIN_FAIL_SOURCE)
                    } else {
                        self.sources
                            .iter()
                            .position(|s| s.id == source)
                            .map(|i| i as u16)
                    };
                    if let Some(si) = si {
                        self.failed
                            .insert(TileKey { source: si, id }, Instant::now());
                    }
                }
                None => break,
            }
        }
        if uploads == MAX_UPLOADS_PER_FRAME {
            ctx.request_repaint(); // more tiles waiting in the channel
        }
    }

    /// Request an elevation tile (or re-upload it from the CPU copy if the GPU evicted it).
    fn request_terrain_tile(&mut self, id: TileId, prio: f32) {
        if let Some(t) = self.heights.get(id) {
            if !self.renderer.has_height(id) {
                let t = t.clone();
                self.renderer.upload_height(id, &t.heights);
            }
            return;
        }
        let key = TileKey {
            source: TERRAIN_FAIL_SOURCE,
            id,
        };
        if self
            .failed
            .get(&key)
            .is_some_and(|t| t.elapsed() < RETRY_AFTER)
        {
            return;
        }
        let src = self.terrain_src.clone();
        self.tiles.request(&src, id, prio);
    }

    /// Queue tile requests; each key carries its source index so overlays fetch from their own source.
    fn request_imagery(&mut self, reqs: &[(TileKey, f32)]) {
        for (key, prio) in reqs.iter().take(MAX_REQUESTS_PER_FRAME) {
            if self
                .failed
                .get(key)
                .is_some_and(|t| t.elapsed() < RETRY_AFTER)
            {
                continue;
            }
            if let Some(src) = self.sources.get(key.source as usize).cloned() {
                self.tiles.request(&src, key.id, *prio);
            }
        }
    }

    /// Visible overlays as (source index, opacity, source max zoom).
    fn active_overlays(&self) -> Vec<(usize, f32, u8)> {
        self.overlays
            .iter()
            .filter(|o| {
                o.visible && o.opacity > 0.0 && !self.hidden.contains(&self.sources[o.source].id)
            })
            .map(|o| (o.source, o.opacity, self.sources[o.source].max_zoom))
            .collect()
    }

    fn render_flat(&mut self) {
        let base = self.sources[self.selected].clone();
        let plan = plan_frame(
            &mut self.renderer,
            &self.view,
            self.selected as u16,
            base.max_zoom,
            1.0,
        );
        let (mut quads, mut reqs, zoom) = (plan.quads, plan.requests, plan.tile_zoom);
        for (idx, opacity, max_zoom) in self.active_overlays() {
            let p = plan_frame(
                &mut self.renderer,
                &self.view,
                idx as u16,
                max_zoom,
                opacity,
            );
            quads.extend(p.quads);
            reqs.extend(p.requests);
        }
        self.request_imagery(&reqs);
        let params = self.shade2d;
        let verts = if params.hillshade || params.slope {
            let (v, reqs) = self.renderer.plan_shade(&self.view, zoom);
            for (k, p) in reqs.iter().take(80) {
                self.request_terrain_tile(k.id, *p);
            }
            v
        } else {
            vec![]
        };
        self.renderer.render_2d(&quads, &verts, &params);
    }

    fn render_terrain(&mut self) {
        let base_max = self.sources[self.selected].max_zoom;
        // Follow the loaded ground under the target, then keep the eye out of the hillside.
        if let Some(h) = self.heights.sample(self.cam.target) {
            self.cam.target_elev_m += (h as f64 - self.cam.target_elev_m) * 0.25;
        }
        self.cam.enforce_clearance(&self.heights);
        let chunks = select_chunks(&self.cam, &self.heights, base_max, SPLIT_PX, MAX_CHUNKS);
        self.last_chunks = chunks.len();
        let params = self.shade3d;
        let overlays: Vec<(u16, f32, u8)> = self
            .active_overlays()
            .into_iter()
            .map(|(i, o, z)| (i as u16, o, z))
            .collect();
        let plan =
            self.renderer
                .render_3d(&self.cam, &chunks, self.selected as u16, &overlays, &params);
        self.request_imagery(&plan.imagery_requests);
        for (k, p) in plan.height_requests.iter().take(80) {
            self.request_terrain_tile(k.id, *p);
        }
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
        let hf = &self.heights;
        let proj = Proj {
            mode: self.mode,
            view: self.view,
            cam: self.cam,
            hf,
        };
        let project = |p: LngLat| proj.project(p);
        let tol = PICK_TOL * ppp as f64;
        let shift = ui.input(|i| i.modifiers.shift);
        let terrain = self.mode == ViewMode::Terrain;

        // Decide what a left-drag does the moment it starts.
        if resp.drag_started_by(PointerButton::Primary) {
            self.velocity = Vec2::ZERO;
            let start = ui
                .input(|i| i.pointer.press_origin())
                .map(|p| to_px(p, rect, ppp));
            self.drag = Some(DragMode::Pan);
            if let Some(pt) = start {
                if editor.tool == Tool::Freehand {
                    if let Some(ll) = proj.unproject(pt) {
                        editor.draft.clear();
                        editor.freehand_point(ll);
                        self.drag = Some(DragMode::Freehand);
                    }
                } else if editor.tool == Tool::Select
                    && let Some(hit) = pick(&editor.project, editor.selection, &project, pt, tol)
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
                if let Some(p) = resp.interact_pointer_pos()
                    && let Some(mut ll) = proj.unproject(to_px(p, rect, ppp))
                {
                    if !shift
                        && let Some(s) =
                            snap_excluding(editor, id, &project, to_px(p, rect, ppp), tol)
                    {
                        ll = s;
                    }
                    editor.drag_vertex(id, index, ll);
                }
            }
            Some(DragMode::Freehand) if resp.dragged_by(PointerButton::Primary) => {
                if let Some(p) = resp.interact_pointer_pos() {
                    let cur = to_px(p, rect, ppp);
                    let last = editor.draft.last().map(|l| project(*l));
                    if last.is_none_or(|l| {
                        ((cur[0] - l[0]).hypot(cur[1] - l[1])) as f32 > FREEHAND_STEP * ppp
                    }) && let Some(ll) = proj.unproject(cur)
                    {
                        editor.freehand_point(ll);
                    }
                }
            }
            Some(DragMode::Pan) if resp.dragged_by(PointerButton::Primary) => {
                let d = resp.drag_delta();
                if terrain {
                    self.cam.pan_pixels((d.x * ppp) as f64, (d.y * ppp) as f64);
                } else {
                    self.view.pan_pixels((d.x * ppp) as f64, (d.y * ppp) as f64);
                    // Smoothed release velocity (points per second) for inertia.
                    if dt > 0.0 {
                        self.velocity = self.velocity * 0.6 + (d / dt) * 0.4;
                    }
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
            let d = resp.drag_delta();
            if terrain {
                self.cam.orbit(d.x as f64 * 0.005, d.y as f64 * 0.004);
            } else {
                self.view.angle += d.x as f64 * 0.006;
            }
        } else if !terrain && !resp.dragged() && self.velocity.length() > 20.0 {
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
            let pt = to_px(p, rect, ppp);
            if editor.tool == Tool::Select {
                match pick(&editor.project, editor.selection, &project, pt, tol) {
                    Some(Hit { id, .. }) => editor.selection = Some(id),
                    None => {
                        editor.selection = None;
                        // Nothing of the user's under the pointer: offer live Show Me Fire data instead.
                        self.smf.info = Self::smf_pick(&self.smf, &project, pt, tol);
                        if self.smf.info.is_none()
                            && let Some(t) = Self::team_pick(&self.team, &project, pt, tol)
                                .or_else(|| Self::live_pick(&self.live, &project, pt, tol))
                                .or_else(|| Self::ext_pick(&self.ext, &project, pt, tol))
                        {
                            self.notice = Some((t, Instant::now()));
                        }
                    }
                }
            } else if let Some(mut ll) = proj.unproject(pt) {
                if !shift && let Some(s) = snap(&editor.project, &project, pt, tol) {
                    ll = s;
                }
                let tol_m =
                    (meters_per_pixel(self.view.center.lat, self.view.zoom) * tol * 1.5).max(12.0);
                self.click_snapped(editor, ll, if terrain { 30.0 } else { tol_m });
            }
        } else if resp.double_clicked()
            && editor.tool == Tool::Select
            && let Some(p) = resp.interact_pointer_pos()
        {
            let pt = to_px(p, rect, ppp);
            if terrain {
                // Fly to the clicked ground point.
                if let Some(ll) = proj.unproject(pt) {
                    self.cam.target = ll;
                    self.cam.target_elev_m = self.heights.sample(ll).unwrap_or(0.0) as f64;
                    self.cam.dolly(0.5);
                }
            } else {
                self.view
                    .zoom_at(pt, (self.view.zoom + 1.0).round(), MAX_ZOOM);
            }
        } else if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            // Right-click a selected vertex to delete it.
            if let Some(Hit {
                id,
                vertex: Some(i),
            }) = pick(
                &editor.project,
                editor.selection,
                &project,
                to_px(p, rect, ppp),
                tol,
            ) {
                editor.delete_vertex(id, i);
            }
        }

        if resp.hovered() {
            let (scroll, pinch) = ui
                .ctx()
                .input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let dz = scroll as f64 * 0.004 + (pinch as f64).log2();
            if dz.abs() > 1e-6 {
                if terrain {
                    self.cam.dolly((-dz).exp());
                } else {
                    let anchor = resp
                        .hover_pos()
                        .map(|p| to_px(p, rect, ppp))
                        .unwrap_or([self.view.size[0] / 2.0, self.view.size[1] / 2.0]);
                    self.view.zoom_at(anchor, self.view.zoom + dz, MAX_ZOOM);
                }
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
        cursor: Option<LngLat>,
        rect: Rect,
        ppp: f32,
        src: &TileSource,
    ) {
        let painter = ui.painter_at(rect);
        let pill = |text: String, anchor: Align2, pos: Pos2| {
            overlay::label(&painter, pos, anchor, &text, 12.0, Color32::BLACK)
        };

        // Attribution is a licence requirement for every provider, so it is always on screen.
        let shade = match self.mode {
            ViewMode::Flat => &self.shade2d,
            ViewMode::Terrain => &self.shade3d,
        };
        let uses_terrain = self.mode == ViewMode::Terrain
            || shade.hillshade
            || shade.slope
            || self.cursor_elev.is_some();
        let mut attribution = if uses_terrain {
            format!("{} | {}", src.attribution, self.terrain_src.attribution)
        } else {
            src.attribution.clone()
        };
        if self.coord_format == CoordFormat::W3w || self.w3w.grid_on || self.w3w.found.is_some() {
            attribution.push_str(" | what3words");
        }
        if let Some(c) = self.live_credit() {
            attribution.push_str(" | ");
            attribution.push_str(c);
        }
        if let Some(c) = self.trails_credit() {
            attribution.push_str(" | ");
            attribution.push_str(c);
        }
        if let Some(c) = self.ext_credit() {
            attribution.push_str(" | ");
            attribution.push_str(&c);
        }
        pill(
            attribution,
            Align2::RIGHT_BOTTOM,
            rect.right_bottom() - Vec2::new(10.0, 8.0),
        );

        if let Some(ll) = cursor {
            let text = match self.coord_format {
                CoordFormat::Dd => smf_geo::format_dd(ll),
                CoordFormat::Dms => smf_geo::format_dms(ll),
                CoordFormat::Utm => {
                    smf_geo::format_utm(ll).unwrap_or_else(|| "UTM n/a at this latitude".into())
                }
                CoordFormat::W3w => self.w3w_readout(ll),
            };
            pill(
                text,
                Align2::LEFT_BOTTOM,
                rect.left_bottom() + Vec2::new(10.0, -8.0),
            );
        }

        if self.mode == ViewMode::Flat {
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
}

/// 8-point compass name for a bearing in degrees.
fn compass(bearing: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    NAMES[((bearing.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

fn to_px(p: Pos2, rect: Rect, ppp: f32) -> [f64; 2] {
    [
        ((p.x - rect.min.x) * ppp) as f64,
        ((p.y - rect.min.y) * ppp) as f64,
    ]
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
    use super::{compass, nice_scale};

    #[test]
    fn compass_names() {
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(359.0), "N");
        assert_eq!(compass(44.0), "NE");
        assert_eq!(compass(225.0), "SW");
        assert_eq!(compass(270.0), "W");
        assert_eq!(compass(-90.0), "W");
    }

    #[test]
    fn scale_picks_round_numbers() {
        assert_eq!(nice_scale(1234.0).0, 1000.0);
        assert_eq!(nice_scale(2600.0).0, 2000.0);
        assert_eq!(nice_scale(7000.0).0, 5000.0);
        assert!(nice_scale(1000.0).1.starts_with("1.00 km"));
    }
}
