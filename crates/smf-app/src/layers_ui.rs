//! Layer stack (basemap + overlays), custom layers, offline areas and the download
//! dialog. These are `MapPanel` methods split into their own file to keep map_panel.rs
//! focused on rendering and input.

use crate::layers::{AddLayerInput, DownloadInput, LayerKind, human_bytes};
use crate::map_panel::MapPanel;
use eframe::egui::{self, RichText};
use smf_features::Editor;
use smf_geo::LngLat;
use smf_tiles::{Backend, Download, DownloadError, TileSource, estimate};
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayLayer {
    /// Index into `MapPanel::sources`.
    pub source: usize,
    pub opacity: f32,
    pub visible: bool,
}

pub struct ActiveDownload {
    pub label: String,
    pub dl: Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaMode {
    View,
    Selection,
    AllFeatures,
}

pub struct DownloadUi {
    pub open: bool,
    pub source: usize,
    pub area: AreaMode,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub terrain: bool,
    pub name: String,
    pub error: Option<String>,
}

impl Default for DownloadUi {
    fn default() -> Self {
        Self {
            open: false,
            source: 0,
            area: AreaMode::View,
            min_zoom: 8,
            max_zoom: 13,
            terrain: true,
            name: String::new(),
            error: None,
        }
    }
}

fn is_local(s: &TileSource) -> bool {
    matches!(s.backend, Backend::Mbtiles { .. } | Backend::Geotiff { .. })
}

/// Bounding box of every vertex of the given features, padded slightly so a single point has area.
fn feature_bounds(editor: &Editor, only_selected: bool) -> Option<[f64; 4]> {
    let mut b: Option<[f64; 4]> = None;
    for f in &editor.project.features {
        if only_selected && editor.selection != Some(f.id) {
            continue;
        }
        for p in f.geometry.vertices() {
            let e = b.get_or_insert([p.lng, p.lat, p.lng, p.lat]);
            *e = [
                e[0].min(p.lng),
                e[1].min(p.lat),
                e[2].max(p.lng),
                e[3].max(p.lat),
            ];
        }
    }
    b.map(|b| [b[0] - 0.002, b[1] - 0.002, b[2] + 0.002, b[3] + 0.002])
}

impl MapPanel {
    /// Load saved custom layers, then find and mount offline areas. Called once at startup.
    pub(crate) fn init_layers(&mut self) {
        for s in self.store.load() {
            match self.tiles.register_source(&s) {
                Ok(()) => self.sources.push(std::sync::Arc::new(s)),
                Err(e) => tracing::warn!("custom layer \"{}\" skipped: {e}", s.name),
            }
        }
        self.rescan_offline();
    }

    /// Verification hook: download `bounds` for the current basemap (+ terrain) without the dialog.
    pub(crate) fn dev_download(
        &mut self,
        bounds: [f64; 4],
        min_zoom: u8,
        max_zoom: u8,
        name: &str,
    ) {
        let input = DownloadInput {
            source: (*self.sources[self.selected]).clone(),
            terrain: Some((*self.terrain_src).clone()),
            bounds,
            min_zoom,
            max_zoom,
            name: name.to_owned(),
            dir: self.offline_dir.clone(),
        };
        for spec in input.specs() {
            let label = spec.out.display().to_string();
            match smf_tiles::start_download(spec) {
                Ok(dl) => self.downloads.push(ActiveDownload { label, dl }),
                Err(e) => tracing::error!("SMF_DOWNLOAD refused: {e}"),
            }
        }
    }

    /// Verification hook: add a local MBTiles/GeoTIFF as an overlay at `opacity`.
    pub(crate) fn dev_add_layer(&mut self, path: &str, opacity: f32) {
        let kind = if path.to_ascii_lowercase().ends_with(".mbtiles") {
            LayerKind::Mbtiles
        } else {
            LayerKind::Geotiff
        };
        let input = AddLayerInput {
            kind,
            path: path.to_owned(),
            overlay: true,
            ..Default::default()
        };
        let ids: Vec<String> = self.sources.iter().map(|s| s.id.clone()).collect();
        match input.build(&ids).and_then(|s| self.add_custom(s)) {
            Ok(()) => {
                if let Some(o) = self.overlays.last_mut() {
                    o.opacity = opacity;
                }
            }
            Err(e) => tracing::error!("SMF_LAYER failed: {e}"),
        }
    }

    pub(crate) fn set_work_offline(&mut self, offline: bool) {
        self.work_offline = offline;
        self.tiles.set_offline(offline);
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    fn persist_custom(&self) {
        let custom: Vec<TileSource> = self
            .sources
            .iter()
            .filter(|s| s.id.starts_with("custom-") && !self.hidden.contains(&s.id))
            .map(|s| (**s).clone())
            .collect();
        if let Err(e) = self.store.save(&custom) {
            tracing::warn!("could not save custom layers: {e}");
        }
    }

    fn add_custom(&mut self, src: TileSource) -> Result<(), String> {
        self.tiles.register_source(&src)?;
        let overlay = src.overlay;
        self.sources.push(std::sync::Arc::new(src));
        let idx = self.sources.len() - 1;
        if overlay {
            self.overlays.push(OverlayLayer {
                source: idx,
                opacity: 0.8,
                visible: true,
            });
        } else {
            self.selected = idx;
        }
        self.persist_custom();
        Ok(())
    }

    /// Re-read the offline folder and mount every area for its source.
    pub(crate) fn rescan_offline(&mut self) {
        self.areas = smf_tiles::scan_offline_dir(&self.offline_dir);
        self.tiles.unmount_all();
        for a in &self.areas {
            if let (Some(id), Ok(m)) = (&a.source_id, a.open()) {
                self.tiles.mount_offline(id, m);
            }
        }
    }

    /// When the last running download finishes, re-mount offline areas so new tiles are used at once.
    pub(crate) fn poll_downloads(&mut self) {
        let running = self.downloads.iter().any(|d| !d.dl.is_finished());
        if self.was_downloading && !running {
            self.rescan_offline();
        }
        self.was_downloading = running;
    }

    /// Current viewport as (min_lng, min_lat, max_lng, max_lat).
    pub(crate) fn view_bounds(&self) -> [f64; 4] {
        let s = self.view.size;
        let corners: Vec<LngLat> = [[0.0, 0.0], [s[0], 0.0], [s[0], s[1]], [0.0, s[1]]]
            .iter()
            .map(|c| self.view.screen_to_lnglat(*c))
            .collect();
        let lat = |p: &LngLat| p.lat.clamp(-85.0, 85.0);
        [
            corners
                .iter()
                .map(|p| p.lng)
                .fold(f64::MAX, f64::min)
                .max(-180.0),
            corners.iter().map(lat).fold(f64::MAX, f64::min),
            corners
                .iter()
                .map(|p| p.lng)
                .fold(f64::MIN, f64::max)
                .min(180.0),
            corners.iter().map(lat).fold(f64::MIN, f64::max),
        ]
    }

    pub(crate) fn layers_menu(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Overlays").strong());
        let mut remove = None;
        let mut swap = None;
        let n = self.overlays.len();
        for i in 0..n {
            let name = self.sources[self.overlays[i].source].name.clone();
            ui.horizontal(|ui| {
                let ov = &mut self.overlays[i];
                ui.checkbox(&mut ov.visible, &name);
                ui.add(egui::Slider::new(&mut ov.opacity, 0.0..=1.0).show_value(false))
                    .on_hover_text("Opacity");
                if ui
                    .add_enabled(i > 0, egui::Button::new("▲").small())
                    .clicked()
                {
                    swap = Some((i, i - 1));
                }
                if ui
                    .add_enabled(i + 1 < n, egui::Button::new("▼").small())
                    .clicked()
                {
                    swap = Some((i, i + 1));
                }
                if ui
                    .small_button("✕")
                    .on_hover_text("Remove from map")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
        }
        if let Some((a, b)) = swap {
            self.overlays.swap(a, b);
        }
        if let Some(i) = remove {
            self.overlays.remove(i);
        }
        if self.overlays.is_empty() {
            ui.weak("No overlays");
        }
        let addable: Vec<(usize, String)> = self
            .sources
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                s.overlay
                    && !self.hidden.contains(&s.id)
                    && !self.overlays.iter().any(|o| o.source == *i)
            })
            .map(|(i, s)| (i, s.name.clone()))
            .collect();
        if !addable.is_empty() {
            ui.menu_button("Add overlay", |ui| {
                for (i, name) in addable {
                    if ui.button(name).clicked() {
                        self.overlays.push(OverlayLayer {
                            source: i,
                            opacity: 0.8,
                            visible: true,
                        });
                        ui.close();
                    }
                }
            });
        }
        ui.separator();
        if ui.button("Add custom layer…").clicked() {
            self.add_input = Some(AddLayerInput::default());
            self.add_error = None;
            ui.close();
        }
        let customs: Vec<(usize, String)> = self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| s.id.starts_with("custom-") && !self.hidden.contains(&s.id))
            .map(|(i, s)| (i, s.name.clone()))
            .collect();
        if !customs.is_empty() {
            ui.menu_button("Remove custom layer", |ui| {
                for (i, name) in customs {
                    if ui.button(name).clicked() {
                        self.hidden.insert(self.sources[i].id.clone());
                        self.overlays.retain(|o| o.source != i);
                        if self.selected == i {
                            self.selected = self
                                .sources
                                .iter()
                                .position(|s| !s.overlay && !self.hidden.contains(&s.id))
                                .unwrap_or(0);
                        }
                        self.persist_custom();
                        ui.close();
                    }
                }
            });
        }
    }

    pub(crate) fn offline_menu(&mut self, ui: &mut egui::Ui) {
        let mut offline = self.work_offline;
        if ui
            .checkbox(&mut offline, "Work offline (no network)")
            .changed()
        {
            self.work_offline = offline;
            self.tiles.set_offline(offline);
        }
        if ui.button("Download area…").clicked() {
            self.dl.open = true;
            self.dl.error = None;
            if self.dl.name.is_empty() {
                self.dl.name = "offline-area".into();
            }
            // Start from what is on screen: a few zoom levels deeper than the current view.
            let z = self.view.zoom.round().clamp(0.0, 20.0) as u8;
            self.dl.min_zoom = z.saturating_sub(1);
            self.dl.max_zoom = (z + 3).min(20);
            ui.close();
        }
        ui.separator();
        ui.label(RichText::new("Offline areas").strong());
        let mut show = None;
        let mut delete = None;
        for (i, a) in self.areas.iter().enumerate() {
            ui.horizontal(|ui| {
                let zooms = a
                    .zooms
                    .map(|(lo, hi)| format!("z{lo}-{hi}"))
                    .unwrap_or_default();
                ui.label(format!(
                    "{}  ({} tiles, {}, {})",
                    a.name,
                    a.tiles,
                    human_bytes(a.bytes),
                    zooms
                ));
                if a.bounds.is_some() && ui.small_button("Show").clicked() {
                    show = a.bounds;
                }
                if ui
                    .small_button("Delete")
                    .on_hover_text("Delete this file from disk")
                    .clicked()
                {
                    delete = Some(i);
                }
            });
            if a.source_id.is_none() {
                ui.weak("  (not made by this app: not mounted)");
            }
        }
        if self.areas.is_empty() {
            ui.weak("None yet");
        }
        if let Some(b) = show {
            self.pending_fit = Some(b);
        }
        if let Some(i) = delete {
            let path = self.areas[i].path.clone();
            self.tiles.unmount_all();
            let res = std::fs::remove_file(&path);
            for ext in ["mbtiles-wal", "mbtiles-shm"] {
                let _ = std::fs::remove_file(path.with_extension(ext));
            }
            match res {
                Ok(()) => self.notify(format!("Deleted {}", path.display())),
                Err(e) => self.notify(format!("Could not delete {}: {e}", path.display())),
            }
            self.rescan_offline();
        }
        if ui.button("Open offline folder").clicked() {
            let _ = std::fs::create_dir_all(&self.offline_dir);
            #[cfg(windows)]
            let _ = std::process::Command::new("explorer")
                .arg(&self.offline_dir)
                .spawn();
            ui.close();
        }
    }

    /// Floating windows: add-layer dialog, download dialog, download progress.
    pub(crate) fn layer_windows(&mut self, ctx: &egui::Context, editor: &Editor) {
        self.add_layer_window(ctx);
        self.download_window(ctx, editor);
        self.progress_window(ctx);
        if let Some((msg, t)) = &self.notice
            && t.elapsed().as_secs() > 8
        {
            let _ = msg;
            self.notice = None;
        }
    }

    fn add_layer_window(&mut self, ctx: &egui::Context) {
        let Some(mut input) = self.add_input.take() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("Add custom layer").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            egui::ComboBox::from_label("Type").selected_text(input.kind.label()).show_ui(ui, |ui| {
                for k in LayerKind::ALL {
                    ui.selectable_value(&mut input.kind, k, k.label());
                }
            });
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut input.name);
            });
            match input.kind {
                LayerKind::Xyz => {
                    ui.label("URL template ({z} {x} {y}, or {-y} for TMS)");
                    ui.add(egui::TextEdit::singleline(&mut input.url).desired_width(380.0).hint_text("https://example.org/tiles/{z}/{x}/{y}.png"));
                }
                LayerKind::Wms => {
                    ui.label("WMS service URL");
                    ui.add(egui::TextEdit::singleline(&mut input.url).desired_width(380.0).hint_text("https://gis.example.gov/arcgis/services/x/MapServer/WMSServer"));
                    ui.horizontal(|ui| {
                        ui.label("Layers");
                        ui.text_edit_singleline(&mut input.wms_layers);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Format");
                        ui.text_edit_singleline(&mut input.wms_format);
                        egui::ComboBox::from_id_salt("wmsver").selected_text(input.wms_version.clone()).show_ui(ui, |ui| {
                            for v in ["1.1.1", "1.3.0"] {
                                ui.selectable_value(&mut input.wms_version, v.to_owned(), v);
                            }
                        });
                        ui.checkbox(&mut input.wms_transparent, "Transparent");
                    });
                }
                LayerKind::Mbtiles | LayerKind::Geotiff => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut input.path).desired_width(300.0));
                        if ui.button("Browse…").clicked() {
                            let (label, exts): (&str, &[&str]) =
                                if input.kind == LayerKind::Mbtiles { ("MBTiles", &["mbtiles"]) } else { ("GeoTIFF", &["tif", "tiff"]) };
                            if let Some(p) = rfd::FileDialog::new().add_filter(label, exts).pick_file() {
                                input.path = p.display().to_string();
                            }
                        }
                    });
                    if input.kind == LayerKind::Geotiff {
                        ui.weak("North-up GeoTIFF in WGS84, Web Mercator or UTM; up to 64 megapixels.");
                    }
                }
            }
            if matches!(input.kind, LayerKind::Xyz | LayerKind::Wms) {
                ui.horizontal(|ui| {
                    ui.label("Max zoom");
                    ui.add(egui::Slider::new(&mut input.max_zoom, 1..=22));
                });
            }
            ui.horizontal(|ui| {
                ui.label("Attribution");
                ui.text_edit_singleline(&mut input.attribution);
            });
            ui.checkbox(&mut input.overlay, "Draw on top of the basemap (overlay)");
            if matches!(input.kind, LayerKind::Xyz | LayerKind::Wms) {
                ui.checkbox(&mut input.offline_allowed, "Allow downloading for offline use")
                    .on_hover_text("Only tick this if the provider's terms permit bulk download.");
            }
            if let Some(e) = &self.add_error {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
            }
            ui.horizontal(|ui| {
                if ui.button("Add layer").clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if submit {
            let ids: Vec<String> = self.sources.iter().map(|s| s.id.clone()).collect();
            match input.build(&ids).and_then(|s| {
                let name = s.name.clone();
                self.add_custom(s).map(|()| name)
            }) {
                Ok(name) => {
                    self.notify(format!("Added layer \"{name}\""));
                    return; // dialog closes
                }
                Err(e) => self.add_error = Some(e),
            }
        }
        if open && !cancel {
            self.add_input = Some(input);
        }
    }

    fn download_window(&mut self, ctx: &egui::Context, editor: &Editor) {
        if !self.dl.open {
            return;
        }
        let candidates: Vec<(usize, String)> = self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| s.offline_allowed && !is_local(s) && !self.hidden.contains(&s.id))
            .map(|(i, s)| (i, s.name.clone()))
            .collect();
        let restricted: Vec<String> = self
            .sources
            .iter()
            .filter(|s| !s.offline_allowed && !is_local(s) && !s.overlay)
            .map(|s| s.name.clone())
            .collect();
        if !candidates.iter().any(|(i, _)| *i == self.dl.source) {
            self.dl.source = candidates.first().map_or(0, |c| c.0);
        }
        let mut open = true;
        let mut start = false;
        let bounds = match self.dl.area {
            AreaMode::View => Some(self.view_bounds()),
            AreaMode::Selection => feature_bounds(editor, true),
            AreaMode::AllFeatures => feature_bounds(editor, false),
        };
        egui::Window::new("Download area for offline use")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                if candidates.is_empty() {
                    ui.label("No downloadable basemaps.");
                    return;
                }
                let cur = candidates
                    .iter()
                    .find(|c| c.0 == self.dl.source)
                    .map(|c| c.1.clone())
                    .unwrap_or_default();
                egui::ComboBox::from_label("Source")
                    .selected_text(cur)
                    .show_ui(ui, |ui| {
                        for (i, name) in &candidates {
                            ui.selectable_value(&mut self.dl.source, *i, name);
                        }
                    });
                if !restricted.is_empty() {
                    ui.weak(format!(
                        "Not offered (terms forbid bulk download): {}",
                        restricted.join(", ")
                    ));
                }
                ui.horizontal(|ui| {
                    ui.label("Area");
                    ui.selectable_value(&mut self.dl.area, AreaMode::View, "Visible map");
                    ui.selectable_value(&mut self.dl.area, AreaMode::Selection, "Selected feature");
                    ui.selectable_value(&mut self.dl.area, AreaMode::AllFeatures, "All features");
                });
                let max_src = self.sources[self.dl.source].max_zoom;
                self.dl.max_zoom = self.dl.max_zoom.min(max_src);
                self.dl.min_zoom = self.dl.min_zoom.min(self.dl.max_zoom);
                ui.add(egui::Slider::new(&mut self.dl.min_zoom, 0..=max_src).text("min zoom"));
                ui.add(egui::Slider::new(&mut self.dl.max_zoom, 0..=max_src).text("max zoom"));
                if self.dl.min_zoom > self.dl.max_zoom {
                    self.dl.max_zoom = self.dl.min_zoom;
                }
                ui.checkbox(
                    &mut self.dl.terrain,
                    "Include terrain (3D, hillshade, elevation up to z14)",
                );
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.dl.name);
                });
                match bounds {
                    None => {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 160, 60),
                            "Nothing to download: draw or select a feature first.",
                        );
                    }
                    Some(b) => {
                        let src = &self.sources[self.dl.source];
                        let mut e = estimate(src, b, self.dl.min_zoom, self.dl.max_zoom);
                        if self.dl.terrain {
                            let t = estimate(
                                &self.terrain_src,
                                b,
                                self.dl.min_zoom.min(14),
                                self.dl.max_zoom.min(14),
                            );
                            e.tiles += t.tiles;
                            e.approx_bytes += t.approx_bytes;
                        }
                        ui.label(format!(
                            "About {} tiles, roughly {} (an estimate).",
                            e.tiles,
                            human_bytes(e.approx_bytes)
                        ));
                        if e.tiles > smf_tiles::DEFAULT_MAX_TILES {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 90, 90),
                                "Too many tiles: shrink the area or lower the max zoom.",
                            );
                        }
                        ui.weak(format!("Saved to {}", self.offline_dir.display()));
                        if ui
                            .add_enabled(
                                e.tiles <= smf_tiles::DEFAULT_MAX_TILES,
                                egui::Button::new("Start download"),
                            )
                            .clicked()
                        {
                            start = true;
                        }
                    }
                }
                if let Some(e) = &self.dl.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
                }
            });
        if !open {
            self.dl.open = false;
        }
        if start && let Some(b) = bounds {
            let input = DownloadInput {
                source: (*self.sources[self.dl.source]).clone(),
                terrain: self.dl.terrain.then(|| (*self.terrain_src).clone()),
                bounds: b,
                min_zoom: self.dl.min_zoom,
                max_zoom: self.dl.max_zoom,
                name: self.dl.name.clone(),
                dir: self.offline_dir.clone(),
            };
            let mut started: Vec<ActiveDownload> = vec![];
            let mut err: Option<DownloadError> = None;
            for spec in input.specs() {
                let label = spec
                    .out
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match smf_tiles::start_download(spec) {
                    Ok(dl) => started.push(ActiveDownload { label, dl }),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            match err {
                None => {
                    self.downloads.extend(started);
                    self.dl.open = false;
                    self.dl.error = None;
                }
                Some(e) => {
                    // Don't leave half a batch running when the other half was refused.
                    for d in &started {
                        d.dl.cancel();
                    }
                    self.dl.error = Some(e.to_string());
                }
            }
        }
    }

    fn progress_window(&mut self, ctx: &egui::Context) {
        if self.downloads.is_empty() {
            return;
        }
        let mut clear = false;
        egui::Window::new("Offline downloads")
            .collapsible(true)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -40.0])
            .show(ctx, |ui| {
                for d in &self.downloads {
                    let p = &d.dl.progress;
                    let (done, total) = (
                        p.done.load(Ordering::Relaxed),
                        p.total.load(Ordering::Relaxed).max(1),
                    );
                    ui.label(&d.label);
                    ui.add(egui::ProgressBar::new(done as f32 / total as f32).show_percentage());
                    let failed = p.failed.load(Ordering::Relaxed);
                    ui.weak(format!(
                        "{} downloaded, {} skipped, {} failed, {}",
                        p.downloaded.load(Ordering::Relaxed),
                        p.skipped.load(Ordering::Relaxed),
                        failed,
                        human_bytes(p.bytes.load(Ordering::Relaxed))
                    ));
                    if failed > 0
                        && let Some(e) = p.last_error.lock().unwrap().as_ref()
                    {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 130, 90),
                            format!("Last error: {e}"),
                        );
                    }
                    ui.horizontal(|ui| {
                        if d.dl.is_finished() {
                            ui.label(if p.cancelled.load(Ordering::Relaxed) {
                                "Cancelled"
                            } else {
                                "Done"
                            });
                        } else if p.paused.load(Ordering::Relaxed) {
                            if ui.button("Resume").clicked() {
                                d.dl.resume();
                            }
                        } else if ui.button("Pause").clicked() {
                            d.dl.pause();
                        }
                        if !d.dl.is_finished() && ui.button("Cancel").clicked() {
                            d.dl.cancel();
                        }
                    });
                    ui.separator();
                }
                if self.downloads.iter().all(|d| d.dl.is_finished())
                    && ui.button("Dismiss").clicked()
                {
                    clear = true;
                }
            });
        if clear {
            self.downloads.clear();
        }
        // Keep the progress bars moving while a download runs.
        if self.downloads.iter().any(|d| !d.dl.is_finished()) {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
