//! Print-to-PDF and GeoTIFF export dialogs. The job runs on a background thread with its own
//! renderer and tile service so the live map stays responsive and no tile events get mixed up.

use crate::layers::human_bytes;
use crate::map_panel::MapPanel;
use eframe::egui::{self, RichText};
use smf_features::Editor;
use smf_geo::LngLat;
use smf_print::{
    GeoTiffRegion, GridKind, LegendOverlay, Paper, PrintProjection, PrintSpec, RenderLayers,
};
use smf_render::{MapRenderer, ShadeParams};
use smf_tiles::TileService;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrintKind {
    Pdf,
    GeoTiff,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PrintArea {
    View,
    Selection,
    AllFeatures,
}

pub enum JobState {
    Running(f32),
    Done {
        path: PathBuf,
        warnings: Vec<String>,
        missing: usize,
    },
    Failed(String),
    Cancelled,
}

pub struct JobHandle {
    pub state: Arc<Mutex<JobState>>,
    pub cancel: Arc<AtomicBool>,
}

pub struct PrintUi {
    pub open: bool,
    pub kind: PrintKind,
    pub spec: PrintSpec,
    pub area: PrintArea,
    pub hillshade: bool,
    pub declination_on: bool,
    pub declination: f64,
    pub geotiff_zoom: u8,
    pub error: Option<String>,
    pub job: Option<JobHandle>,
}

impl Default for PrintUi {
    fn default() -> Self {
        Self {
            open: false,
            kind: PrintKind::Pdf,
            spec: PrintSpec {
                dpi: 200,
                ..PrintSpec::default()
            },
            area: PrintArea::View,
            hillshade: false,
            declination_on: false,
            declination: 0.0,
            geotiff_zoom: 14,
            error: None,
            job: None,
        }
    }
}

fn bounds_of(editor: &Editor, area: PrintArea) -> Option<[f64; 4]> {
    let mut b: Option<[f64; 4]> = None;
    for f in &editor.project.features {
        if area == PrintArea::Selection && editor.selection != Some(f.id) {
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
    b.map(|b| [b[0] - 0.001, b[1] - 0.001, b[2] + 0.001, b[3] + 0.001])
}

fn atomic_write(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("pdf.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

impl MapPanel {
    pub(crate) fn open_print(&mut self, kind: PrintKind) {
        self.print.open = true;
        self.print.kind = kind;
        self.print.error = None;
        // Start from what the user is looking at.
        let c = if self.mode == crate::map_panel::ViewMode::Flat {
            self.view.center
        } else {
            self.cam.target
        };
        self.print.spec.center = c;
        let z = self.view.zoom.round().clamp(1.0, 18.0) as u8;
        self.print.geotiff_zoom = (z + 1).min(self.sources[self.selected].max_zoom);
    }

    fn print_layers(&self) -> RenderLayers {
        let shade_on = self.print.hillshade;
        RenderLayers {
            base: self.sources[self.selected].clone(),
            overlays: self
                .overlays
                .iter()
                .filter(|o| {
                    o.visible
                        && o.opacity > 0.0
                        && !self.hidden.contains(&self.sources[o.source].id)
                })
                .map(|o| (self.sources[o.source].clone(), o.opacity))
                .collect(),
            terrain: shade_on.then(|| self.terrain_src.clone()),
            shade: if shade_on {
                ShadeParams {
                    hillshade: true,
                    hill_strength: 0.6,
                    ..self.shade2d
                }
            } else {
                ShadeParams::default()
            },
        }
    }

    fn legend_overlays(&self) -> Vec<LegendOverlay> {
        self.overlays
            .iter()
            .filter(|o| o.visible && o.opacity > 0.0)
            .map(|o| LegendOverlay {
                name: self.sources[o.source].name.clone(),
                opacity: o.opacity,
            })
            .collect()
    }

    /// Credit line for every data source that appears on the page.
    fn print_attribution(&self) -> String {
        let mut parts: Vec<String> = vec![self.sources[self.selected].attribution.clone()];
        for o in self
            .overlays
            .iter()
            .filter(|o| o.visible && o.opacity > 0.0)
        {
            let a = &self.sources[o.source].attribution;
            if !parts.contains(a) {
                parts.push(a.clone());
            }
        }
        if self.print.hillshade {
            parts.push(self.terrain_src.attribution.clone());
        }
        parts.join("; ")
    }

    /// Spawn the render on its own thread. `path` is the output file.
    pub(crate) fn start_print_job(
        &mut self,
        editor: &Editor,
        path: PathBuf,
        geotiff_bounds: Option<[f64; 4]>,
    ) {
        let state = Arc::new(Mutex::new(JobState::Running(0.0)));
        let cancel = Arc::new(AtomicBool::new(false));
        self.print.job = Some(JobHandle {
            state: state.clone(),
            cancel: cancel.clone(),
        });
        let (device, queue) = (
            self.render_state.device.clone(),
            self.render_state.queue.clone(),
        );
        let (cache_dir, offline) = (self.cache_dir.clone(), self.work_offline);
        let areas: Vec<_> = self.areas.clone();
        let layers = self.print_layers();
        let mut spec = self.print.spec.clone();
        spec.declination_deg = self.print.declination_on.then_some(self.print.declination);
        spec.attribution = self.print_attribution();
        let legend = self.legend_overlays();
        let project = editor.project.clone();
        let (kind, zoom) = (self.print.kind, self.print.geotiff_zoom);
        std::thread::spawn(move || {
            let mut renderer = MapRenderer::new(&device, &queue);
            let svc = TileService::new(cache_dir, || {});
            svc.set_offline(offline);
            for a in &areas {
                if let (Some(id), Ok(m)) = (&a.source_id, a.open()) {
                    svc.mount_offline(id, m);
                }
            }
            let st = state.clone();
            let mut progress = move |p: f32| {
                if let Ok(mut s) = st.lock()
                    && matches!(*s, JobState::Running(_))
                {
                    *s = JobState::Running(p);
                }
            };
            let result: Result<(Vec<String>, usize), smf_print::PrintError> = match kind {
                PrintKind::Pdf => smf_print::print_to_pdf(
                    &mut renderer,
                    &svc,
                    &spec,
                    &layers,
                    &project,
                    &legend,
                    &mut progress,
                    &cancel,
                )
                .and_then(|out| {
                    atomic_write(&path, &out.bytes)
                        .map_err(|e| smf_print::PrintError::Io(e.to_string()))?;
                    Ok((out.warnings, out.missing_tiles))
                }),
                PrintKind::GeoTiff => {
                    let b = geotiff_bounds.expect("geotiff bounds");
                    smf_print::export_geotiff(&mut renderer, &svc, b, zoom, &layers, &path, &mut progress, &cancel).map(|m| {
                        let w = if m > 0 { vec![format!("{m} map tile(s) could not be loaded; those areas show lower-resolution imagery.")] } else { vec![] };
                        (w, m)
                    })
                }
            };
            *state.lock().unwrap() = match result {
                Ok((warnings, missing)) => JobState::Done {
                    path,
                    warnings,
                    missing,
                },
                Err(smf_print::PrintError::Cancelled) => JobState::Cancelled,
                Err(e) => JobState::Failed(e.to_string()),
            };
        });
    }

    /// Dev hook: print with the current view/basemap and exit when the job ends.
    #[allow(clippy::too_many_arguments)] // dev hook: mirrors the env-var fields one to one
    pub(crate) fn dev_print(
        &mut self,
        editor: &Editor,
        path: PathBuf,
        kind: PrintKind,
        scale: f64,
        dpi: u32,
        zoom: u8,
        bounds: Option<[f64; 4]>,
    ) {
        self.print.kind = kind;
        self.print.spec.scale_denominator = scale;
        self.print.spec.dpi = dpi;
        self.print.spec.center = self.view.center;
        self.print.spec.title = "ShowMeFire Field test print".into();
        self.print.geotiff_zoom = zoom;
        // Extra dev switches so one run can exercise hillshade and the declination arrow.
        self.print.hillshade = std::env::var_os("SMF_PRINT_SHADE").is_some();
        if let Some(d) = std::env::var("SMF_PRINT_DECL")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            self.print.declination_on = true;
            self.print.declination = d;
        }
        self.start_print_job(editor, path, bounds);
    }

    /// Finished job summary for the dev hook: Some(Ok(path)) / Some(Err(msg)) once it ends.
    pub(crate) fn print_outcome(&self) -> Option<Result<String, String>> {
        let job = self.print.job.as_ref()?;
        match &*job.state.lock().unwrap() {
            JobState::Running(_) => None,
            JobState::Done {
                path,
                warnings,
                missing,
            } => Some(Ok(format!(
                "{} (missing tiles {missing}; warnings {warnings:?})",
                path.display()
            ))),
            JobState::Failed(e) => Some(Err(e.clone())),
            JobState::Cancelled => Some(Err("cancelled".into())),
        }
    }

    pub(crate) fn print_window(&mut self, ctx: &egui::Context, editor: &Editor) {
        self.print_progress_window(ctx);
        if !self.print.open {
            return;
        }
        let mut open = true;
        let mut start = false;
        let title = if self.print.kind == PrintKind::Pdf {
            "Print map to PDF"
        } else {
            "Export GeoTIFF"
        };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.print.kind, PrintKind::Pdf, "PDF map");
                    ui.selectable_value(
                        &mut self.print.kind,
                        PrintKind::GeoTiff,
                        "GeoTIFF (georeferenced image)",
                    );
                });
                ui.separator();
                if self.print.kind == PrintKind::Pdf {
                    self.pdf_options(ui, editor);
                } else {
                    self.geotiff_options(ui, editor);
                }
                if let Some(e) = &self.print.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
                }
                let busy = self
                    .print
                    .job
                    .as_ref()
                    .is_some_and(|j| matches!(*j.state.lock().unwrap(), JobState::Running(_)));
                if ui
                    .add_enabled(!busy, egui::Button::new("Choose file and start…"))
                    .clicked()
                {
                    start = true;
                }
            });
        if !open {
            self.print.open = false;
        }
        if start {
            self.print.error = None;
            self.begin_print(editor);
        }
    }

    fn pdf_options(&mut self, ui: &mut egui::Ui, editor: &Editor) {
        let spec = &mut self.print.spec;
        egui::ComboBox::from_label("Paper")
            .selected_text(spec.paper.label())
            .show_ui(ui, |ui| {
                for p in Paper::ALL {
                    ui.selectable_value(&mut spec.paper, p, p.label());
                }
            });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut spec.landscape, false, "Portrait");
            ui.selectable_value(&mut spec.landscape, true, "Landscape");
        });
        ui.horizontal(|ui| {
            ui.label("Scale 1:");
            ui.add(
                egui::DragValue::new(&mut spec.scale_denominator)
                    .range(100.0..=50_000_000.0)
                    .speed(100.0),
            );
            egui::ComboBox::from_id_salt("scalepreset")
                .selected_text("presets")
                .show_ui(ui, |ui| {
                    for s in smf_print::STANDARD_SCALES {
                        if ui
                            .selectable_label(spec.scale_denominator == s, format!("1:{s:.0}"))
                            .clicked()
                        {
                            spec.scale_denominator = s;
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("Resolution");
            for d in [100u32, 150, 200, 300] {
                ui.selectable_value(&mut spec.dpi, d, format!("{d} dpi"));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Center on current view").clicked() {
                let c = if self.mode == crate::map_panel::ViewMode::Flat {
                    self.view.center
                } else {
                    self.cam.target
                };
                self.print.spec.center = c;
            }
            if ui
                .button("Fit selected feature")
                .on_hover_text("Choose center and the finest standard scale that fits")
                .clicked()
            {
                match bounds_of(editor, PrintArea::Selection) {
                    Some(b) => {
                        self.print.spec.center =
                            LngLat::new((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
                        match smf_print::fit_scale(b, &self.print.spec) {
                            Ok(s) => self.print.spec.scale_denominator = s,
                            Err(e) => self.print.error = Some(e.to_string()),
                        }
                    }
                    None => self.print.error = Some("Select a feature first.".into()),
                }
            }
            if ui.button("Fit all features").clicked() {
                match bounds_of(editor, PrintArea::AllFeatures) {
                    Some(b) => {
                        self.print.spec.center =
                            LngLat::new((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
                        if let Ok(s) = smf_print::fit_scale(b, &self.print.spec) {
                            self.print.spec.scale_denominator = s;
                        }
                    }
                    None => self.print.error = Some("There are no features to fit.".into()),
                }
            }
        });
        let spec = &mut self.print.spec;
        ui.horizontal(|ui| {
            ui.label("Title");
            ui.text_edit_singleline(&mut spec.title);
        });
        ui.horizontal(|ui| {
            ui.label("Subtitle");
            ui.text_edit_singleline(&mut spec.subtitle);
        });
        egui::ComboBox::from_label("Grid")
            .selected_text(match spec.grid {
                GridKind::None => "None",
                GridKind::Utm => "UTM",
                GridKind::LatLon => "Latitude/longitude",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut spec.grid, GridKind::None, "None");
                ui.selectable_value(&mut spec.grid, GridKind::Utm, "UTM");
                ui.selectable_value(&mut spec.grid, GridKind::LatLon, "Latitude/longitude");
            });
        ui.horizontal(|ui| {
            ui.checkbox(&mut spec.scale_bar, "Scale bar");
            ui.checkbox(&mut spec.north_arrow, "North arrow");
            ui.checkbox(&mut spec.legend, "Legend");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.print.declination_on, "Magnetic declination");
            ui.add_enabled(
                self.print.declination_on,
                egui::DragValue::new(&mut self.print.declination)
                    .range(-30.0..=30.0)
                    .speed(0.1)
                    .suffix("° (east +)"),
            );
        });
        ui.checkbox(&mut self.print.hillshade, "Include hillshade");
        // What this will produce, so a 600 dpi A2 isn't a surprise.
        let spec = &self.print.spec;
        match smf_print::Layout::new(spec) {
            Ok(layout) => {
                let pj = PrintProjection::new(spec, &layout);
                let (gw, gh) = pj.ground_size_m(spec.scale_denominator);
                let px = pj.frame_px();
                let mp = px[0] as u64 * px[1] as u64;
                ui.label(
                    RichText::new(format!(
                        "Map covers {:.2} x {:.2} km; image {}x{} px ({:.1} MP)",
                        gw / 1000.0,
                        gh / 1000.0,
                        px[0],
                        px[1],
                        mp as f64 / 1e6
                    ))
                    .weak(),
                );
                if mp > smf_print::MAX_PIXELS {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 90, 90),
                        "Too large: lower the resolution or use a smaller page.",
                    );
                }
                ui.label(
                    RichText::new("Print at 100% (no fit-to-page) so the scale is true.").weak(),
                );
            }
            Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e.to_string());
            }
        }
    }

    fn geotiff_options(&mut self, ui: &mut egui::Ui, editor: &Editor) {
        ui.horizontal(|ui| {
            ui.label("Area");
            ui.selectable_value(&mut self.print.area, PrintArea::View, "Visible map");
            ui.selectable_value(
                &mut self.print.area,
                PrintArea::Selection,
                "Selected feature",
            );
            ui.selectable_value(&mut self.print.area, PrintArea::AllFeatures, "All features");
        });
        let max_z = self.sources[self.selected].max_zoom;
        self.print.geotiff_zoom = self.print.geotiff_zoom.min(max_z);
        ui.add(egui::Slider::new(&mut self.print.geotiff_zoom, 1..=max_z).text("zoom"));
        ui.checkbox(&mut self.print.hillshade, "Include hillshade");
        match self.geotiff_bounds(editor) {
            Some(b) => {
                let r = GeoTiffRegion::new(b, self.print.geotiff_zoom);
                let mp = r.width as u64 * r.height as u64;
                ui.label(RichText::new(format!(
                    "{}x{} px ({:.1} MP, about {} uncompressed), {:.2} m/px at the equator, EPSG:3857",
                    r.width,
                    r.height,
                    mp as f64 / 1e6,
                    human_bytes(mp * 3),
                    r.pixel_m
                )).weak());
                if mp > smf_print::MAX_PIXELS {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 90, 90),
                        "Too large: lower the zoom or choose a smaller area.",
                    );
                }
            }
            None => {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 160, 60),
                    "Nothing to export: draw or select a feature first.",
                );
            }
        }
    }

    fn geotiff_bounds(&self, editor: &Editor) -> Option<[f64; 4]> {
        match self.print.area {
            PrintArea::View => Some(self.view_bounds()),
            a => bounds_of(editor, a),
        }
    }

    fn begin_print(&mut self, editor: &Editor) {
        let (ext, label) = match self.print.kind {
            PrintKind::Pdf => ("pdf", "PDF"),
            PrintKind::GeoTiff => ("tif", "GeoTIFF"),
        };
        let name = if self.print.spec.title.trim().is_empty() {
            "map".to_owned()
        } else {
            self.print
                .spec
                .title
                .trim()
                .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
        };
        let bounds = if self.print.kind == PrintKind::GeoTiff {
            match self.geotiff_bounds(editor) {
                Some(b) => {
                    let r = GeoTiffRegion::new(b, self.print.geotiff_zoom);
                    if r.width as u64 * r.height as u64 > smf_print::MAX_PIXELS {
                        self.print.error = Some("That export is too large.".into());
                        return;
                    }
                    Some(b)
                }
                None => {
                    self.print.error = Some("Nothing to export.".into());
                    return;
                }
            }
        } else {
            if let Err(e) = smf_print::Layout::new(&self.print.spec) {
                self.print.error = Some(e.to_string());
                return;
            }
            None
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(label, &[ext])
            .set_file_name(format!("{name}.{ext}"))
            .save_file()
        else {
            return;
        };
        self.start_print_job(editor, path, bounds);
    }

    fn print_progress_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.print.job else { return };
        let mut dismiss = false;
        let mut open_path: Option<PathBuf> = None;
        egui::Window::new("Print / export")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &*job.state.lock().unwrap() {
                JobState::Running(p) => {
                    ui.label("Rendering the map at print resolution…");
                    ui.add(egui::ProgressBar::new(*p).show_percentage());
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
                JobState::Done {
                    path,
                    warnings,
                    missing,
                } => {
                    ui.label(RichText::new("Done").strong());
                    ui.label(path.display().to_string());
                    if *missing > 0 {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 160, 60),
                            format!("{missing} tile(s) missing"),
                        );
                    }
                    for w in warnings {
                        ui.weak(w);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Open").clicked() {
                            open_path = Some(path.clone());
                        }
                        if ui.button("Close").clicked() {
                            dismiss = true;
                        }
                    });
                }
                JobState::Failed(e) => {
                    ui.colored_label(egui::Color32::from_rgb(230, 90, 90), format!("Failed: {e}"));
                    if ui.button("Close").clicked() {
                        dismiss = true;
                    }
                }
                JobState::Cancelled => {
                    ui.label("Cancelled.");
                    if ui.button("Close").clicked() {
                        dismiss = true;
                    }
                }
            });
        if let Some(p) = open_path {
            #[cfg(windows)]
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(&p)
                .spawn();
            #[cfg(not(windows))]
            let _ = p;
        }
        if dismiss {
            self.print.job = None;
        }
    }
}
