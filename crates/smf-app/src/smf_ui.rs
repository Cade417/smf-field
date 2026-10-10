//! Built-in Show Me Fire data: live fire layers, raster conditions, the 72-hour forecast,
//! legends and the point-conditions readout. These are `MapPanel` methods in their own file.

use crate::layers_ui::OverlayLayer;
use crate::map_panel::MapPanel;
use crate::overlay::Projector;
use crossbeam_channel::{Receiver, Sender, unbounded};
use directories::ProjectDirs;
use eframe::egui::{self, Align2, Color32, Pos2, Rect, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use smf_data::{
    Command, DataCache, DataFeature, DataLayerKind, ForecastRun, RasterLayer, Refresher, SmfClient,
    Update, default_base, links, parse_catalog, parse_conditions, parse_forecast_run,
    snapshot_features, summary, unix_now,
};
use smf_features::{Editor, GeoHit, geometry_hit};
use smf_geo::LngLat;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Raster overlays opened by default the first time: the headline Show Me Fire product.
const DEFAULT_RASTER: (&str, f32) = ("fire_danger", 0.45);
const COND_REST: Duration = Duration::from_millis(800);

#[derive(Default, Serialize, Deserialize, Clone)]
struct Prefs {
    initialized: bool,
    vectors: Vec<String>,
    rasters: Vec<(String, f32)>,
}

pub struct LayerView {
    pub features: Vec<DataFeature>,
    pub fetched_unix: Option<u64>,
    pub from_cache: bool,
    pub error: Option<String>,
    pub enabled: bool,
}

pub struct ForecastUi {
    pub run: Option<ForecastRun>,
    pub status: String,
    pub on: bool,
    pub var_idx: usize,
    pub lead: u32,
    pub playing: bool,
    last_step: Instant,
    /// (variable index, lead) currently shown, to avoid re-registering every frame.
    applied: Option<(usize, u32)>,
    overlay_source: Option<usize>,
}

pub struct SmfState {
    pub base: String,
    client: Option<SmfClient>,
    refresher: Option<Refresher>,
    pub layers: HashMap<DataLayerKind, LayerView>,
    pub catalog: Vec<RasterLayer>,
    pub catalog_status: String,
    catalog_rx: Receiver<Result<(Vec<RasterLayer>, String), String>>,
    catalog_tx: Sender<Result<(Vec<RasterLayer>, String), String>>,
    forecast_rx: Receiver<Result<Option<ForecastRun>, String>>,
    forecast_tx: Sender<Result<Option<ForecastRun>, String>>,
    pub forecast: ForecastUi,
    pub info: Option<DataFeature>,
    prefs: Prefs,
    prefs_path: PathBuf,
    cache_dir: PathBuf,
    pending_rasters: Vec<(String, f32)>,
    cond_rx: Receiver<(LngLat, Result<String, String>)>,
    cond_tx: Sender<(LngLat, Result<String, String>)>,
    cond_inflight: bool,
    cond_rest: Option<(Pos2, Instant)>,
    cond_last: Option<LngLat>,
    pub conditions: Option<String>,
    pub feedback: Option<(String, Instant)>,
    /// Verification hook (SMF_INFO): open the popup for the first feature of this layer once it loads.
    pub dev_info: Option<DataLayerKind>,
    /// Verification hook (SMF_FORECAST=<lead hours>): switch the forecast on at that lead once a run is known.
    pub dev_forecast: Option<u32>,
}

impl SmfState {
    pub fn new(ctx: &egui::Context) -> Self {
        let dirs = ProjectDirs::from("org", "ShowMeFire", "Field");
        let cache_dir = dirs
            .as_ref()
            .map(|d| d.cache_dir().join("smf-data"))
            .unwrap_or_else(|| std::env::temp_dir().join("smf-field-data"));
        let prefs_path = dirs
            .as_ref()
            .map(|d| d.config_dir().join("smf_prefs.json"))
            .unwrap_or_else(|| std::env::temp_dir().join("smf-field-prefs.json"));
        let mut prefs: Prefs = std::fs::read_to_string(&prefs_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if !prefs.initialized {
            // First run: show the incident picture and the fire-danger raster without any setup.
            prefs = Prefs {
                initialized: true,
                vectors: DataLayerKind::ALL
                    .iter()
                    .filter(|k| k.default_on())
                    .map(|k| k.id().to_owned())
                    .collect(),
                rasters: vec![(DEFAULT_RASTER.0.to_owned(), DEFAULT_RASTER.1)],
            };
        }
        let base = default_base();
        let client = SmfClient::new(&base).ok();
        let ctx2 = ctx.clone();
        let refresher = client.clone().map(|c| {
            Refresher::spawn(
                c,
                DataCache::new(cache_dir.clone()),
                move || ctx2.request_repaint(),
                None,
            )
        });
        let mut layers = HashMap::new();
        for k in DataLayerKind::ALL {
            let enabled = prefs.vectors.iter().any(|v| v == k.id());
            layers.insert(
                k,
                LayerView {
                    features: vec![],
                    fetched_unix: None,
                    from_cache: false,
                    error: None,
                    enabled,
                },
            );
            if let Some(r) = &refresher {
                r.set_enabled(k, enabled);
            }
        }
        let (catalog_tx, catalog_rx) = unbounded();
        let (forecast_tx, forecast_rx) = unbounded();
        let (cond_tx, cond_rx) = unbounded();
        let mut s = Self {
            base,
            client,
            refresher,
            layers,
            catalog: vec![],
            catalog_status: "loading layer list…".into(),
            catalog_rx,
            catalog_tx,
            forecast_rx,
            forecast_tx,
            forecast: ForecastUi {
                run: None,
                status: "checking for a forecast…".into(),
                on: false,
                var_idx: 0,
                lead: 0,
                playing: false,
                last_step: Instant::now(),
                applied: None,
                overlay_source: None,
            },
            info: None,
            pending_rasters: prefs.rasters.clone(),
            prefs,
            prefs_path,
            cache_dir,
            cond_rx,
            cond_tx,
            cond_inflight: false,
            cond_rest: None,
            cond_last: None,
            conditions: None,
            feedback: None,
            dev_info: None,
            dev_forecast: None,
        };
        s.load_catalog(ctx);
        s.check_forecast(ctx);
        s
    }

    fn save_prefs(&mut self) {
        self.prefs.vectors = DataLayerKind::ALL
            .iter()
            .filter(|k| self.layers[k].enabled)
            .map(|k| k.id().to_owned())
            .collect();
        if let Some(d) = self.prefs_path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(b) = serde_json::to_vec_pretty(&self.prefs) {
            let _ = std::fs::write(&self.prefs_path, b);
        }
    }

    /// Show the last catalog immediately (works offline), then refresh it from the server.
    fn load_catalog(&mut self, ctx: &egui::Context) {
        let cache = self.cache_dir.join("catalog.json");
        if let Some(text) = std::fs::read_to_string(&cache).ok()
            && let Ok(c) = parse_catalog(&text)
        {
            self.catalog = c;
            self.catalog_status = "from the last session".into();
        }
        let Some(client) = self.client.clone() else {
            self.catalog_status = "invalid API address".into();
            return;
        };
        let (tx, ctx) = (self.catalog_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = client
                .get_text("/api/conditions/layers")
                .map_err(|e| e.to_string())
                .and_then(|t| parse_catalog(&t).map_err(|e| e.to_string()).map(|c| (c, t)));
            let _ = tx.send(r);
            ctx.request_repaint();
        });
    }

    fn check_forecast(&mut self, ctx: &egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let (tx, ctx) = (self.forecast_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = match client.get("/api/forecast/v1/runs/latest", None) {
                Ok(smf_data::Fetched::Body { text, .. }) => parse_forecast_run(&text)
                    .map(Some)
                    .map_err(|e| e.to_string()),
                Ok(_) => Ok(None),
                // 404 means no public run right now; that is normal, not an error.
                Err(smf_data::DataError::Status(404)) => Ok(None),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
    }

    pub fn set_vector(&mut self, kind: DataLayerKind, on: bool) {
        if let Some(l) = self.layers.get_mut(&kind) {
            l.enabled = on;
        }
        if let Some(r) = &self.refresher {
            r.set_enabled(kind, on);
        }
        self.save_prefs();
    }

    pub fn refresh_now(&self) {
        if let Some(r) = &self.refresher {
            r.send(Command::RefreshNow);
        }
    }

    pub fn set_offline(&self, offline: bool) {
        if let Some(r) = &self.refresher {
            r.send(Command::SetOffline(offline));
        }
    }
}

fn ago(unix: Option<u64>) -> String {
    let Some(t) = unix else { return "never".into() };
    let s = unix_now().saturating_sub(t);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{:.1} h ago", s as f64 / 3600.0),
        _ => format!("{} days ago", s / 86_400),
    }
}

fn color(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

impl MapPanel {
    /// Called once per frame before drawing: apply background results and drive the forecast player.
    pub(crate) fn poll_smf(&mut self, ctx: &egui::Context) {
        while let Some(u) = self.smf.refresher.as_ref().and_then(Refresher::try_recv) {
            match u {
                Update::Loaded {
                    kind,
                    parsed,
                    fetched_unix,
                    from_cache,
                } => {
                    if let Some(l) = self.smf.layers.get_mut(&kind) {
                        l.features = parsed.features;
                        l.fetched_unix = Some(fetched_unix);
                        l.from_cache = from_cache;
                        l.error = None;
                    }
                }
                Update::Unchanged { kind, fetched_unix } => {
                    if let Some(l) = self.smf.layers.get_mut(&kind) {
                        l.fetched_unix = Some(fetched_unix);
                        l.from_cache = false;
                        l.error = None;
                    }
                }
                Update::Failed { kind, error } => {
                    if let Some(l) = self.smf.layers.get_mut(&kind) {
                        l.error = Some(error);
                    }
                }
            }
        }
        while let Ok(r) = self.smf.catalog_rx.try_recv() {
            match r {
                Ok((cat, text)) => {
                    let _ = std::fs::create_dir_all(&self.smf.cache_dir);
                    let _ = std::fs::write(self.smf.cache_dir.join("catalog.json"), text);
                    self.smf.catalog = cat;
                    self.smf.catalog_status = format!("{} layers", self.smf.catalog.len());
                }
                Err(e) => self.smf.catalog_status = format!("offline ({e})"),
            }
        }
        // Raster layers saved from last time (or the first-run default) turn on once the catalog is known.
        if !self.smf.catalog.is_empty() && !self.smf.pending_rasters.is_empty() {
            for (id, opacity) in std::mem::take(&mut self.smf.pending_rasters) {
                if let Some(layer) = self
                    .smf
                    .catalog
                    .iter()
                    .find(|l| l.id == id && l.available)
                    .cloned()
                {
                    self.set_smf_raster(&layer, true, opacity);
                }
            }
        }
        while let Ok(r) = self.smf.forecast_rx.try_recv() {
            self.smf.forecast.status = match &r {
                Ok(Some(run)) => format!("run {} ({} h)", run.cycle_time, run.horizon_hours),
                Ok(None) => "no forecast run is published right now".into(),
                Err(e) => format!("could not check ({e})"),
            };
            if let Ok(run) = r {
                self.smf.forecast.run = run;
                if let (Some(lead), Some(_)) =
                    (self.smf.dev_forecast.take(), &self.smf.forecast.run)
                {
                    self.smf.forecast.on = true;
                    self.smf.forecast.lead = lead;
                }
            }
        }
        while let Ok((at, r)) = self.smf.cond_rx.try_recv() {
            self.smf.cond_inflight = false;
            self.smf.conditions = r.ok().and_then(|t| parse_conditions(&t).ok()).map(|c| {
                format!(
                    "{}  ({:.3}, {:.3})",
                    c.summary(&self.smf.catalog),
                    at.lat,
                    at.lng
                )
            });
        }
        if let Some(k) = self.smf.dev_info
            && let Some(f) = self.smf.layers[&k].features.first().cloned()
        {
            self.smf.info = Some(f);
            self.smf.dev_info = None;
        }
        self.drive_forecast(ctx);
        if self
            .smf
            .feedback
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed().as_secs() > 8)
        {
            self.smf.feedback = None;
        }
    }

    fn smf_raster_overlay(&self, id: &str) -> Option<usize> {
        let sid = format!("smf-raster-{id}");
        self.overlays
            .iter()
            .position(|o| self.sources[o.source].id == sid)
    }

    /// Turn a Show Me Fire raster layer on (as an overlay) or off.
    pub(crate) fn set_smf_raster(&mut self, layer: &RasterLayer, on: bool, opacity: f32) {
        let existing = self.smf_raster_overlay(&layer.id);
        match (on, existing) {
            (true, None) => {
                let mut src = layer.tile_source(&self.smf.base);
                // The catalog's observation time changes the URL (cache-busting), so refresh the source in place when it does.
                if let Some(i) = self.sources.iter().position(|s| s.id == src.id) {
                    src.id = self.sources[i].id.clone();
                    self.sources[i] = std::sync::Arc::new(src);
                    self.overlays.push(OverlayLayer {
                        source: i,
                        opacity,
                        visible: true,
                    });
                } else {
                    self.sources.push(std::sync::Arc::new(src));
                    self.overlays.push(OverlayLayer {
                        source: self.sources.len() - 1,
                        opacity,
                        visible: true,
                    });
                }
            }
            (false, Some(i)) => {
                self.overlays.remove(i);
            }
            _ => {}
        }
        // Remember the choice (opacity is stored too, so it comes back the way it was left).
        self.smf.prefs.rasters = self
            .smf
            .catalog
            .iter()
            .filter_map(|l| {
                self.smf_raster_overlay(&l.id)
                    .map(|i| (l.id.clone(), self.overlays[i].opacity))
            })
            .collect();
        self.smf.save_prefs();
    }

    fn drive_forecast(&mut self, ctx: &egui::Context) {
        let f = &mut self.smf.forecast;
        let Some(run) = f.run.clone() else { return };
        if !f.on {
            // Switched off: drop the overlay.
            if let Some(src) = f.overlay_source.take() {
                self.overlays.retain(|o| o.source != src);
                self.smf.forecast.applied = None;
            }
            return;
        }
        if f.playing && f.last_step.elapsed() > Duration::from_millis(900) {
            f.lead = if f.lead >= run.horizon_hours {
                0
            } else {
                f.lead + 1
            };
            f.last_step = Instant::now();
            ctx.request_repaint_after(Duration::from_millis(900));
        }
        let Some(layer) = run
            .layers
            .get(f.var_idx.min(run.layers.len().saturating_sub(1)))
        else {
            return;
        };
        if f.applied == Some((f.var_idx, f.lead)) {
            return;
        }
        let src = run.tile_source(layer, f.lead, &self.smf.base);
        let (var_idx, lead) = (f.var_idx, f.lead);
        let idx = match self.sources.iter().position(|s| s.id == src.id) {
            Some(i) => i,
            None => {
                self.sources.push(std::sync::Arc::new(src));
                self.sources.len() - 1
            }
        };
        let previous = self.smf.forecast.overlay_source;
        match previous.and_then(|p| self.overlays.iter().position(|o| o.source == p)) {
            Some(oi) => self.overlays[oi].source = idx,
            None => self.overlays.push(OverlayLayer {
                source: idx,
                opacity: 0.7,
                visible: true,
            }),
        }
        self.smf.forecast.overlay_source = Some(idx);
        self.smf.forecast.applied = Some((var_idx, lead));
    }

    pub(crate) fn smf_menu(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        ui.label(RichText::new("Fire data").strong());
        let mut toggle: Option<(DataLayerKind, bool)> = None;
        let mut snapshot: Option<DataLayerKind> = None;
        for kind in DataLayerKind::ALL {
            let l = &self.smf.layers[&kind];
            let mut on = l.enabled;
            ui.horizontal(|ui| {
                if ui.checkbox(&mut on, kind.label()).changed() {
                    toggle = Some((kind, on));
                }
                if l.enabled {
                    let status = match (&l.error, l.fetched_unix) {
                        (Some(e), _) => RichText::new(format!("{} ({e})", l.features.len()))
                            .color(Color32::from_rgb(230, 130, 90)),
                        (None, _) => RichText::new(format!(
                            "{}  {}{}",
                            l.features.len(),
                            ago(l.fetched_unix),
                            if l.from_cache { " (saved copy)" } else { "" }
                        ))
                        .weak(),
                    };
                    ui.label(status);
                    if !l.features.is_empty()
                        && ui
                            .small_button("Copy to map")
                            .on_hover_text(
                                "Add these as editable features (works offline, prints, exports)",
                            )
                            .clicked()
                    {
                        snapshot = Some(kind);
                    }
                }
            });
        }
        if let Some((k, on)) = toggle {
            self.smf.set_vector(k, on);
        }
        if let Some(k) = snapshot {
            self.snapshot_layer(k, editor);
        }
        if ui.button("Refresh now").clicked() {
            self.smf.refresh_now();
            self.smf.load_catalog(ui.ctx());
            self.smf.check_forecast(ui.ctx());
        }
        ui.separator();
        ui.label(RichText::new("Conditions").strong());
        ui.weak(format!("Layer list: {}", self.smf.catalog_status));
        let catalog = self.smf.catalog.clone();
        for group in ["rtma", "station_fill"] {
            let title = if group == "rtma" {
                "Weather analysis (RTMA, hourly)"
            } else {
                "Station analysis"
            };
            let members: Vec<&RasterLayer> = catalog.iter().filter(|l| l.group == group).collect();
            if members.is_empty() {
                continue;
            }
            ui.label(RichText::new(title).weak());
            for l in members {
                let idx = self.smf_raster_overlay(&l.id);
                let mut on = idx.is_some();
                ui.horizontal(|ui| {
                    if !l.available {
                        ui.add_enabled(
                            false,
                            egui::Checkbox::new(&mut on, format!("{} (unavailable)", l.label)),
                        );
                        return;
                    }
                    if ui.checkbox(&mut on, &l.label).changed() {
                        self.set_smf_raster(l, on, 0.6);
                    }
                    if let Some(i) = self.smf_raster_overlay(&l.id) {
                        let mut op = self.overlays[i].opacity;
                        if ui
                            .add(egui::Slider::new(&mut op, 0.05..=1.0).show_value(false))
                            .changed()
                        {
                            self.overlays[i].opacity = op;
                        }
                    }
                });
            }
        }
        ui.separator();
        ui.label(RichText::new("72-hour forecast").strong());
        self.forecast_controls(ui);
        ui.separator();
        ui.weak(format!("Server: {}", self.smf.base));
    }

    fn forecast_controls(&mut self, ui: &mut egui::Ui) {
        let f = &mut self.smf.forecast;
        let Some(run) = f.run.clone() else {
            ui.weak(&f.status);
            return;
        };
        if run.layers.is_empty() {
            ui.weak("This run has no layers that can be stepped through.");
            return;
        }
        ui.checkbox(&mut f.on, "Show forecast");
        ui.weak(&f.status);
        if f.on {
            let cur = run.layers[f.var_idx.min(run.layers.len() - 1)]
                .variable
                .replace('_', " ");
            egui::ComboBox::from_label("Variable")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (i, l) in run.layers.iter().enumerate() {
                        ui.selectable_value(&mut f.var_idx, i, l.variable.replace('_', " "));
                    }
                });
            ui.add(egui::Slider::new(&mut f.lead, 0..=run.horizon_hours).text("hours ahead"));
            ui.label(run.valid_time_label(f.lead));
            if ui
                .button(if f.playing { "Pause" } else { "Play" })
                .clicked()
            {
                f.playing = !f.playing;
                f.last_step = Instant::now();
            }
        }
    }

    fn snapshot_layer(&mut self, kind: DataLayerKind, editor: &mut Editor) {
        let date = smf_print::utc_date();
        let (folder, feats) = snapshot_features(&self.smf.layers[&kind].features, kind, &date);
        if feats.is_empty() {
            return;
        }
        let fid = editor.project.add_folder(&folder);
        let n = feats.len();
        let feats = feats.into_iter().map(|mut f| {
            f.folder = Some(fid);
            f
        });
        editor.add_features(feats.collect());
        self.smf.feedback = Some((
            format!(
                "Copied {n} {} into the map (folder \"{folder}\")",
                kind.label().to_lowercase()
            ),
            Instant::now(),
        ));
    }

    /// Live fire data drawn under the user's own markup.
    pub(crate) fn draw_smf_data(&self, painter: &egui::Painter, pj: &Projector, rect: Rect) {
        // Footprints and zones first, points last so incidents stay on top.
        let order = [
            DataLayerKind::BurnBans,
            DataLayerKind::WeatherAlerts,
            DataLayerKind::IncidentShapes,
            DataLayerKind::Detections,
            DataLayerKind::Events,
            DataLayerKind::Incidents,
        ];
        let visible = rect.expand(120.0);
        for kind in order {
            let Some(l) = self.smf.layers.get(&kind).filter(|l| l.enabled) else {
                continue;
            };
            for f in &l.features {
                crate::overlay::draw_data_feature(painter, pj, f, visible);
            }
        }
    }

    /// Topmost live-data feature under the pointer (physical px), if any.
    pub(crate) fn smf_pick(
        smf: &SmfState,
        proj: &dyn Fn(LngLat) -> [f64; 2],
        pt: [f64; 2],
        tol: f64,
    ) -> Option<DataFeature> {
        // Points beat outlines beat interiors; within a class, later layers win.
        let order = [
            DataLayerKind::Incidents,
            DataLayerKind::Events,
            DataLayerKind::IncidentShapes,
            DataLayerKind::Detections,
            DataLayerKind::BurnBans,
            DataLayerKind::WeatherAlerts,
        ];
        let mut interior: Option<&DataFeature> = None;
        for kind in order {
            let Some(l) = smf.layers.get(&kind).filter(|l| l.enabled) else {
                continue;
            };
            for f in l.features.iter().rev() {
                match geometry_hit(&f.geometry, proj, pt, tol) {
                    Some(GeoHit::Edge) => return Some(f.clone()),
                    Some(GeoHit::Interior) if interior.is_none() => interior = Some(f),
                    _ => {}
                }
            }
        }
        interior.cloned()
    }

    pub(crate) fn smf_windows(&mut self, ctx: &egui::Context, editor: &mut Editor) {
        let Some(f) = self.smf.info.clone() else {
            return;
        };
        let mut open = true;
        let mut copy = false;
        egui::Window::new(&f.title)
            .id(egui::Id::new("smf-info"))
            .open(&mut open)
            .default_width(340.0)
            .show(ctx, |ui| {
                egui::Grid::new("smf-info-grid")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        for (k, v) in summary(&f) {
                            ui.label(RichText::new(k).weak());
                            ui.label(v);
                            ui.end_row();
                        }
                    });
                ui.separator();
                for (label, url) in links(&f, &self.smf.base) {
                    if ui.link(&label).on_hover_text(&url).clicked() {
                        let _ = open::that(&url);
                    }
                }
                if ui
                    .button("Copy to map")
                    .on_hover_text("Add this as an editable feature")
                    .clicked()
                {
                    copy = true;
                }
            });
        if copy {
            let (_, feats) =
                snapshot_features(std::slice::from_ref(&f), f.kind, &smf_print::utc_date());
            editor.add_features(feats);
        }
        if !open {
            self.smf.info = None;
        }
        if let Some((msg, _)) = &self.smf.feedback {
            let _ = msg; // shown in the status bar
        }
    }

    /// Fire danger etc. at the point the pointer rests on, shown in the status bar.
    pub(crate) fn smf_conditions_tick(
        &mut self,
        hover: Option<Pos2>,
        at: Option<LngLat>,
        ctx: &egui::Context,
    ) {
        let any_raster = self
            .overlays
            .iter()
            .any(|o| o.visible && self.sources[o.source].id.starts_with("smf-raster-"));
        let (Some(h), Some(ll), true) = (hover, at, any_raster) else {
            self.smf.cond_rest = None;
            return;
        };
        match self.smf.cond_rest {
            Some((p, _)) if (p - h).length() < 6.0 => {}
            _ => {
                self.smf.cond_rest = Some((h, Instant::now()));
                return;
            }
        }
        let rested = self
            .smf
            .cond_rest
            .map(|(_, t)| t.elapsed())
            .unwrap_or_default();
        let moved = self
            .smf
            .cond_last
            .is_none_or(|l| (l.lat - ll.lat).abs() > 0.003 || (l.lng - ll.lng).abs() > 0.003);
        if rested < COND_REST {
            ctx.request_repaint_after(COND_REST - rested);
        } else if moved && !self.smf.cond_inflight && !self.work_offline {
            self.query_conditions(ll, ctx);
        }
    }

    /// Ask the server for conditions at a point; the result lands in `smf.conditions`.
    pub(crate) fn query_conditions(&mut self, ll: LngLat, ctx: &egui::Context) {
        let Some(client) = self.smf.client.clone() else {
            return;
        };
        self.smf.cond_inflight = true;
        self.smf.cond_last = Some(ll);
        let (tx, ctx2) = (self.smf.cond_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = client
                .get_text(&format!(
                    "/api/conditions/at?lat={:.5}&lon={:.5}",
                    ll.lat, ll.lng
                ))
                .map_err(|e| e.to_string());
            let _ = tx.send((ll, r));
            ctx2.request_repaint();
        });
    }

    /// Colour keys for every visible Show Me Fire raster, drawn in the map's top-left corner.
    pub(crate) fn draw_smf_legends(&self, painter: &egui::Painter, rect: Rect) {
        let mut y = rect.top() + 8.0;
        let font = egui::FontId::proportional(11.0);
        for o in self
            .overlays
            .iter()
            .filter(|o| o.visible && o.opacity > 0.0)
        {
            let id = &self.sources[o.source].id;
            let Some(layer) = id
                .strip_prefix("smf-raster-")
                .and_then(|lid| self.smf.catalog.iter().find(|l| l.id == lid))
            else {
                continue;
            };
            if layer.legend.is_empty() {
                continue;
            }
            let categorical = layer.legend.iter().all(|e| e.label.is_some());
            let (w, h) = (if categorical { 330.0 } else { 220.0 }, 38.0);
            let box_rect = Rect::from_min_size(Pos2::new(rect.left() + 8.0, y), Vec2::new(w, h));
            painter.rect_filled(box_rect, 4.0, Color32::from_white_alpha(225));
            let title = if layer.units.is_empty() || categorical {
                layer.label.clone()
            } else {
                format!("{} ({})", layer.label, layer.units)
            };
            painter.text(
                box_rect.left_top() + Vec2::new(6.0, 4.0),
                Align2::LEFT_TOP,
                title,
                font.clone(),
                Color32::BLACK,
            );
            let n = layer.legend.len() as f32;
            let bar = Rect::from_min_size(
                box_rect.left_top() + Vec2::new(6.0, 19.0),
                Vec2::new(w - 12.0, 8.0),
            );
            for (i, e) in layer.legend.iter().enumerate() {
                let seg = Rect::from_min_size(
                    Pos2::new(bar.left() + bar.width() * i as f32 / n, bar.top()),
                    Vec2::new(bar.width() / n + 0.5, bar.height()),
                );
                painter.rect_filled(seg, 0.0, color(e.color));
                if categorical {
                    painter.text(
                        Pos2::new(seg.center().x, bar.bottom() + 1.0),
                        Align2::CENTER_TOP,
                        e.label.clone().unwrap_or_default(),
                        egui::FontId::proportional(9.0),
                        Color32::BLACK,
                    );
                }
            }
            if !categorical {
                painter.text(
                    Pos2::new(bar.left(), bar.bottom() + 1.0),
                    Align2::LEFT_TOP,
                    format!("{}", layer.legend.first().map_or(0.0, |e| e.value)),
                    egui::FontId::proportional(9.0),
                    Color32::BLACK,
                );
                painter.text(
                    Pos2::new(bar.right(), bar.bottom() + 1.0),
                    Align2::RIGHT_TOP,
                    format!("{}", layer.legend.last().map_or(0.0, |e| e.value)),
                    egui::FontId::proportional(9.0),
                    Color32::BLACK,
                );
            }
            painter.rect_stroke(
                box_rect,
                4.0,
                Stroke::new(0.5, Color32::from_gray(120)),
                egui::StrokeKind::Outside,
            );
            y += h + 6.0;
        }
    }

    /// Live data as plain features for printing: markers keep a short label (the county), nothing else.
    pub(crate) fn smf_print_features(&self) -> Vec<smf_features::Feature> {
        let mut out = Vec::new();
        for kind in DataLayerKind::ALL {
            let Some(l) = self.smf.layers.get(&kind).filter(|l| l.enabled) else {
                continue;
            };
            let (_, mut feats) = snapshot_features(&l.features, kind, "");
            for f in &mut feats {
                f.notes.clear();
                // The full title is too long to print beside a marker.
                f.name = f
                    .name
                    .split(',')
                    .nth(1)
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default();
                if kind != DataLayerKind::Incidents {
                    f.name.clear();
                }
            }
            out.extend(feats);
        }
        out
    }

    /// One legend line per visible data layer, e.g. "Fire incidents (7)".
    pub(crate) fn smf_print_legend(&self) -> Vec<(String, usize, [u8; 3])> {
        DataLayerKind::ALL
            .iter()
            .filter_map(|k| {
                let l = self
                    .smf
                    .layers
                    .get(k)
                    .filter(|l| l.enabled && !l.features.is_empty())?;
                // The first feature's colour stands for the layer (colours vary with confidence).
                Some((
                    k.label().to_owned(),
                    l.features.len(),
                    l.features[0].style.color,
                ))
            })
            .collect()
    }
}
