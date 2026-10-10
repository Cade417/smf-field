//! ShowMeFire Field desktop entry point.

mod earth_ui;
mod ext_ui;
mod gps_ui;
mod layers;
mod layers_ui;
mod map_panel;
mod overlay;
mod panels;
mod print_ui;
mod smf_ui;
mod trails_ui;
mod update;
mod w3w_ui;

use eframe::egui;
use smf_features::{Editor, Geometry, Project, SavedView, Tool};
use smf_geo::LngLat;
use std::path::PathBuf;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu=warn,naga=warn,egui_wgpu=warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("ShowMeFire Field")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ShowMeFire Field",
        options,
        Box::new(|cc| Ok(Box::new(FieldApp::new(cc)))),
    )
}

struct FieldApp {
    map: map_panel::MapPanel,
    editor: Editor,
    path: Option<PathBuf>,
    /// `editor.revision` at the last save/load; differs when there are unsaved edits.
    saved_revision: u64,
    status: Option<(String, std::time::Instant)>,
    title: String,
    shot: Option<ScreenshotHook>,
    /// SMF_PRINT / SMF_GEOTIFF: close the app when the print job ends.
    exit_on_print: bool,
    /// Deferred SMF_PRINT request: (path, scale, dpi, when).
    pending_print: Option<(PathBuf, f64, u32, std::time::Instant)>,
    /// Automated run: never open modal dialogs.
    quiet: bool,
    about_open: bool,
    update: std::sync::Arc<std::sync::Mutex<update::UpdateState>>,
}

/// Dev/verification hook: with SMF_SCREENSHOT=<file.png> the app lets tiles load,
/// saves its own rendered frame (never a screen grab, so other windows are not
/// captured) and exits. SMF_SCREENSHOT_SECS overrides the 12 s settle time.
struct ScreenshotHook {
    path: PathBuf,
    started: std::time::Instant,
    settle: std::time::Duration,
    requested: bool,
}

impl FieldApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let shot = std::env::var_os("SMF_SCREENSHOT").map(|p| ScreenshotHook {
            path: p.into(),
            started: std::time::Instant::now(),
            settle: std::time::Duration::from_secs(
                std::env::var("SMF_SCREENSHOT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(12),
            ),
            requested: false,
        });
        let mut app = Self {
            map: map_panel::MapPanel::new(cc),
            editor: Editor::default(),
            path: None,
            saved_revision: 0,
            status: None,
            title: String::new(),
            pending_print: None,
            exit_on_print: std::env::var_os("SMF_PRINT").is_some()
                || std::env::var_os("SMF_GEOTIFF").is_some(),
            quiet: shot.is_some()
                || std::env::var_os("SMF_IMPORT").is_some()
                || std::env::var_os("SMF_PRINT").is_some()
                || std::env::var_os("SMF_GEOTIFF").is_some(),
            shot,
            about_open: std::env::var_os("SMF_ABOUT").is_some(),
            update: std::sync::Arc::new(std::sync::Mutex::new(update::UpdateState::Idle)),
        };
        if let Some(p) = std::env::var_os("SMF_OPEN") {
            app.open_path(p.into());
        } else if std::env::var_os("SMF_DEMO").is_some() {
            app.load_demo();
        }
        app.apply_dev_env();
        app
    }

    /// Verification hooks. SMF_CAM="lng,lat,distance_m,pitch_deg,heading_deg" starts in 3D at that
    /// camera; SMF_SLOPE=deg|fire turns on slope shading; SMF_SELECT=<feature name> selects it; SMF_IMPORT=a;b imports files at startup; SMF_VIEW=lng,lat,zoom; SMF_DOWNLOAD=lng,lat,span,zmin,zmax,name; SMF_OFFLINE=1; SMF_BASEMAP=id; SMF_INFO=<layer id>; SMF_COND=lng,lat; SMF_EARTH=out.kmz[,max_zoom,levels]; SMF_GPS=system|serial:COM3,4800|gpsd:host:port|replay:file,ms; SMF_GPS_RECORD=1; SMF_EXT=history,snow; SMF_TRAIL_ROUTE=lng,lat;lng,lat;... (+SMF_TRAILS_FILE=overpass.json); SMF_W3W_SEARCH=a.b.c; SMF_W3W_GRID=1; SMF_W3W_HOVER=lng,lat (all need W3W_API_KEY; SMF_W3W_BASE=<url> points at a mock); SMF_FORECAST=<lead hours>; SMF_API_BASE=<url>; SMF_LAYER=file[@opacity]; SMF_PRINT=out.pdf[,scale,dpi] (+SMF_PRINT_DELAY secs); SMF_GEOTIFF=out.tif[,zoom,half_span_deg].
    fn apply_dev_env(&mut self) {
        if let Ok(v) = std::env::var("SMF_CAM") {
            let n: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if let [lng, lat, dist, pitch, heading] = n[..] {
                self.map.view.center = LngLat::new(lng, lat);
                self.map.cam.target = LngLat::new(lng, lat);
                self.map.cam.distance_m = dist;
                self.map.cam.pitch = pitch.to_radians();
                self.map.cam.heading = heading.to_radians();
                self.map.mode = map_panel::ViewMode::Terrain;
            } else {
                tracing::warn!("SMF_CAM needs 5 numbers: lng,lat,distance_m,pitch_deg,heading_deg");
            }
        }
        if let Ok(v) = std::env::var("SMF_VIEW") {
            let n: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if let [lng, lat, zoom] = n[..] {
                self.map.view.center = LngLat::new(lng, lat);
                self.map.view.zoom = zoom;
            } else {
                tracing::warn!("SMF_VIEW needs 3 numbers: lng,lat,zoom");
            }
        }
        if let Ok(v) = std::env::var("SMF_DOWNLOAD") {
            let parts: Vec<&str> = v.split(',').map(str::trim).collect();
            if let [lng, lat, span, zmin, zmax, name] = parts[..]
                && let (Ok(lng), Ok(lat), Ok(span), Ok(zmin), Ok(zmax)) = (
                    lng.parse::<f64>(),
                    lat.parse::<f64>(),
                    span.parse::<f64>(),
                    zmin.parse::<u8>(),
                    zmax.parse::<u8>(),
                )
            {
                self.map.dev_download(
                    [lng - span, lat - span, lng + span, lat + span],
                    zmin,
                    zmax,
                    name,
                );
            } else {
                tracing::warn!("SMF_DOWNLOAD needs lng,lat,span_deg,zmin,zmax,name");
            }
        }
        if let Ok(id) = std::env::var("SMF_BASEMAP") {
            self.map.set_basemap(&id);
        }
        if let Ok(v) = std::env::var("SMF_LAYER") {
            let (path, opacity) = match v.rsplit_once('@') {
                Some((p, o)) => (p.to_owned(), o.parse().unwrap_or(0.5)),
                None => (v, 0.5),
            };
            self.map.dev_add_layer(&path, opacity);
        }
        if let Ok(v) = std::env::var("SMF_PRINT") {
            // path[,scale_denominator,dpi]; starts after SMF_PRINT_DELAY seconds (default 0) so background
            // data (catalog, incidents) has loaded, as it would for a person using the dialog.
            let p: Vec<&str> = v.split(',').map(str::trim).collect();
            let scale = p.get(1).and_then(|x| x.parse().ok()).unwrap_or(24_000.0);
            let dpi = p.get(2).and_then(|x| x.parse().ok()).unwrap_or(150);
            let delay = std::env::var("SMF_PRINT_DELAY")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(0.0);
            self.pending_print = Some((
                PathBuf::from(p[0]),
                scale,
                dpi,
                std::time::Instant::now() + std::time::Duration::from_secs_f64(delay),
            ));
        }
        if let Ok(v) = std::env::var("SMF_GEOTIFF") {
            // path,zoom,half_span_degrees  (centered on SMF_VIEW)
            let p: Vec<&str> = v.split(',').map(str::trim).collect();
            let zoom = p.get(1).and_then(|x| x.parse().ok()).unwrap_or(14);
            let span: f64 = p.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.03);
            let c = self.map.view.center;
            let b = [c.lng - span, c.lat - span, c.lng + span, c.lat + span];
            self.map.dev_print(
                &self.editor,
                PathBuf::from(p[0]),
                print_ui::PrintKind::GeoTiff,
                24_000.0,
                150,
                zoom,
                Some(b),
            );
        }
        if let Ok(k) = std::env::var("SMF_INFO") {
            self.map.smf.dev_info = smf_data::DataLayerKind::ALL
                .into_iter()
                .find(|d| d.id() == k);
        }
        if let Ok(v) = std::env::var("SMF_FORECAST") {
            self.map.smf.dev_forecast = v.trim().parse().ok();
        }
        if let Ok(v) = std::env::var("SMF_COND") {
            // lng,lat: query point conditions now (the same call a resting pointer makes)
            let n: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if let [lng, lat] = n[..] {
                self.map.dev_conditions = Some(LngLat::new(lng, lat));
            }
        }
        if let Ok(v) = std::env::var("SMF_TRAIL_ROUTE") {
            // lng,lat;lng,lat;...: snap-to-trails drawing through these clicks
            let pts: Vec<LngLat> = v
                .split(';')
                .filter_map(|q| {
                    let n: Vec<f64> = q.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    (n.len() == 2).then(|| LngLat::new(n[0], n[1]))
                })
                .collect();
            self.map.trails.on = true;
            self.map.trails.dev_route = Some(pts);
        }
        if let Ok(v) = std::env::var("SMF_EARTH") {
            // out.kmz[,max_zoom,levels]: export the view as a SuperOverlay at startup
            let p: Vec<&str> = v.split(',').collect();
            self.map.earth.dev_out = Some(PathBuf::from(p[0]));
            if let Some(z) = p.get(1).and_then(|x| x.trim().parse().ok()) {
                self.map.earth.max_zoom = z;
            }
            if let Some(l) = p.get(2).and_then(|x| x.trim().parse().ok()) {
                self.map.earth.levels = l;
            }
        }
        if let Ok(v) = std::env::var("SMF_GPS") {
            // system | serial:COM3[,baud] | gpsd:host:port | replay:file[,interval_ms]
            self.map.gps.dev_source = match v.split_once(':') {
                None if v == "system" => Some(smf_gps::Source::System),
                Some(("serial", r)) => {
                    let (p, b) = r.split_once(',').unwrap_or((r, "4800"));
                    Some(smf_gps::Source::Serial {
                        port: p.into(),
                        baud: b.parse().unwrap_or(4800),
                    })
                }
                Some(("gpsd", r)) => Some(smf_gps::Source::Gpsd { addr: r.into() }),
                Some(("replay", r)) => {
                    let (p, ms) = r.rsplit_once(',').unwrap_or((r, "500"));
                    Some(smf_gps::Source::NmeaFile {
                        path: PathBuf::from(p),
                        interval_ms: ms.parse().unwrap_or(500),
                    })
                }
                _ => None,
            };
        }
        if std::env::var_os("SMF_GPS_RECORD").is_some() {
            self.map.gps.track = Some(vec![]);
        }
        if let Ok(v) = std::env::var("SMF_EXT") {
            // history,snow: switch those layers on at startup
            for k in v.split(',') {
                match k.trim() {
                    "history" => self.map.ext.set_on(ext_ui::ExtKind::FireHistory, true),
                    "snow" => self.map.ext.set_on(ext_ui::ExtKind::SnowGauges, true),
                    _ => {}
                }
            }
        }
        if let Ok(v) = std::env::var("SMF_W3W_SEARCH") {
            self.map.w3w.dev_search = Some(v);
        }
        if std::env::var_os("SMF_W3W_GRID").is_some() {
            self.map.w3w.grid_on = true;
        }
        if let Ok(v) = std::env::var("SMF_W3W_HOVER") {
            let n: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if let [lng, lat] = n[..] {
                self.map.w3w.dev_hover = Some(LngLat::new(lng, lat));
            }
        }
        if std::env::var_os("SMF_OFFLINE").is_some() {
            self.map.set_work_offline(true);
        }
        if let Ok(v) = std::env::var("SMF_SLOPE") {
            for p in [&mut self.map.shade2d, &mut self.map.shade3d] {
                p.slope = true;
                p.slope_fire_preset = v == "fire";
            }
        }
        if let Ok(list) = std::env::var("SMF_IMPORT") {
            for path in list.split(';').filter(|p| !p.is_empty()) {
                self.import_file(PathBuf::from(path));
            }
        }
        if let Ok(name) = std::env::var("SMF_SELECT") {
            self.editor.selection = self
                .editor
                .project
                .features
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.id);
        }
    }

    fn dirty(&self) -> bool {
        self.editor.revision != self.saved_revision
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), std::time::Instant::now()));
    }

    /// Sample data so the drawing overlay can be verified without clicking (SMF_DEMO=1).
    fn load_demo(&mut self) {
        let e = &mut self.editor;
        let c = LngLat::new(-92.30, 38.58);
        let folder = e.project.add_folder("Incident 1");
        let m = e.add(Geometry::Marker(c), Some("Command Post".into()));
        e.add(
            Geometry::RangeRing {
                center: c,
                radii_m: vec![2000.0, 5000.0, 10000.0],
            },
            Some("Rings".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-92.38, 38.62),
                LngLat::new(-92.30, 38.66),
                LngLat::new(-92.22, 38.61),
                LngLat::new(-92.27, 38.55),
                LngLat::new(-92.34, 38.57),
            ]),
            Some("Burn unit".into()),
        );
        e.add(
            Geometry::Line(vec![
                LngLat::new(-92.45, 38.50),
                LngLat::new(-92.35, 38.52),
                LngLat::new(-92.20, 38.49),
            ]),
            Some("Dozer line".into()),
        );
        e.add(
            Geometry::Label(LngLat::new(-92.30, 38.70)),
            Some("North flank".into()),
        );
        e.measure = vec![
            LngLat::new(-92.40, 38.45),
            LngLat::new(-92.25, 38.47),
            LngLat::new(-92.22, 38.42),
        ];
        if let Some(mut f) = e.project.feature(m).cloned() {
            f.folder = Some(folder);
            e.replace(f);
        }
        e.selection = e
            .project
            .features
            .iter()
            .find(|f| f.name == "Burn unit")
            .map(|f| f.id);
        self.map.view.center = LngLat::new(-92.30, 38.58);
        self.map.view.zoom = 10.5;
    }

    fn new_project(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        self.editor.replace_project(Project::default());
        self.path = None;
        self.saved_revision = self.editor.revision;
    }

    fn confirm_discard(&self) -> bool {
        // Automated runs have nobody to ask, and must not hang on a modal dialog.
        !self.dirty()
            || self.quiet
            || rfd::MessageDialog::new()
                .set_title("Unsaved changes")
                .set_description("Discard unsaved changes to this map?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show()
                == rfd::MessageDialogResult::Yes
    }

    fn open_dialog(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("ShowMeFire map", &["smfmap"])
            .pick_file()
        {
            self.open_path(p);
        }
    }

    fn open_path(&mut self, p: PathBuf) {
        match smf_io::load_project(&p) {
            Ok(project) => {
                let view = project.view;
                let bounds = project.bounds();
                if let Some(b) = project.basemap.clone() {
                    self.map.set_basemap(&b);
                }
                self.editor.replace_project(project);
                self.saved_revision = self.editor.revision;
                match (view, bounds) {
                    (Some(v), _) => {
                        self.map.view.center = v.center;
                        self.map.view.zoom = v.zoom;
                        self.map.view.angle = v.angle;
                    }
                    (None, Some(b)) => self.map.pending_fit = Some(b),
                    _ => {}
                }
                self.say(format!("Opened {}", p.display()));
                self.path = Some(p);
            }
            Err(e) => {
                alert(self.quiet, "Could not open map", e.to_string());
            }
        }
    }

    fn save(&mut self, as_new: bool) {
        let path = match (&self.path, as_new) {
            (Some(p), false) => p.clone(),
            _ => {
                let Some(p) = rfd::FileDialog::new()
                    .add_filter("ShowMeFire map", &["smfmap"])
                    .set_file_name(format!("{}.smfmap", self.editor.project.name))
                    .save_file()
                else {
                    return;
                };
                p
            }
        };
        // Persist what the user was looking at so reopening lands in the same place.
        self.editor.project.view = Some(SavedView {
            center: self.map.view.center,
            zoom: self.map.view.zoom,
            angle: self.map.view.angle,
        });
        self.editor.project.basemap = Some(self.map.basemap_id().to_owned());
        match smf_io::save_project(&self.editor.project, &path) {
            Ok(()) => {
                self.saved_revision = self.editor.revision;
                self.say(format!("Saved {}", path.display()));
                self.path = Some(path);
            }
            Err(e) => {
                alert(self.quiet, "Could not save map", e.to_string());
            }
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Shift+S must be consumed before plain S, otherwise S would swallow it.
        let (new, open, save_as, plain_save) = ctx.input_mut(|i| {
            use egui::{Key, KeyboardShortcut, Modifiers};
            let mut shortcut = |m, k| i.consume_shortcut(&KeyboardShortcut::new(m, k));
            (
                shortcut(Modifiers::COMMAND, Key::N),
                shortcut(Modifiers::COMMAND, Key::O),
                shortcut(Modifiers::COMMAND | Modifiers::SHIFT, Key::S),
                shortcut(Modifiers::COMMAND, Key::S),
            )
        });
        egui::Panel::top("menu").show(ui, |ui| {
            let mut do_new = new;
            let mut do_open = open;
            let mut do_save = plain_save;
            let mut do_save_as = save_as;
            let mut do_import = false;
            let mut do_export: Option<smf_io::Format> = None;
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New        Ctrl+N").clicked() {
                        do_new = true;
                        ui.close();
                    }
                    if ui.button("Open…      Ctrl+O").clicked() {
                        do_open = true;
                        ui.close();
                    }
                    if ui.button("Save       Ctrl+S").clicked() {
                        do_save = true;
                        ui.close();
                    }
                    if ui.button("Save As…   Ctrl+Shift+S").clicked() {
                        do_save_as = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Import…   (or drop files here)").clicked() {
                        do_import = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Print map to PDF…").clicked() {
                        self.map.open_print(print_ui::PrintKind::Pdf);
                        ui.close();
                    }
                    if ui.button("Export GeoTIFF…").clicked() {
                        self.map.open_print(print_ui::PrintKind::GeoTiff);
                        ui.close();
                    }
                    if ui.button("Export Google Earth SuperOverlay…").clicked() {
                        self.map.open_earth();
                        ui.close();
                    }
                    ui.menu_button("Export", |ui| {
                        for fmt in smf_io::Format::ALL {
                            if ui.button(fmt.label()).clicked() {
                                do_export = Some(fmt);
                                ui.close();
                            }
                        }
                    });
                });
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(self.editor.can_undo(), egui::Button::new("Undo   Ctrl+Z"))
                        .clicked()
                    {
                        self.editor.undo();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.editor.can_redo(), egui::Button::new("Redo   Ctrl+Y"))
                        .clicked()
                    {
                        self.editor.redo();
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Shortcuts and credits").clicked() {
                        self.about_open = true;
                        ui.close();
                    }
                });
                ui.separator();
                for (tool, label, tip) in [
                    (Tool::Select, "Select", "V"),
                    (Tool::Marker, "Marker", "M"),
                    (Tool::Line, "Line", "L"),
                    (Tool::Polygon, "Polygon", "P"),
                    (Tool::Freehand, "Freehand", "F"),
                    (Tool::RangeRing, "Rings", "R"),
                    (Tool::Label, "Label", "T"),
                    (Tool::Measure, "Measure", "D"),
                ] {
                    if ui
                        .selectable_label(self.editor.tool == tool, label)
                        .on_hover_text(format!("Shortcut: {tip}"))
                        .clicked()
                    {
                        self.editor.cancel();
                        self.editor.tool = tool;
                    }
                }
                if let Some((msg, t)) = &self.status
                    && t.elapsed().as_secs() < 6
                {
                    ui.separator();
                    ui.weak(msg);
                }
            });
            if do_new {
                self.new_project();
            }
            if do_open {
                self.open_dialog();
            }
            if do_import {
                self.import_dialog();
            }
            if let Some(fmt) = do_export {
                self.export_dialog(fmt);
            }
            if do_save_as {
                self.save(true);
            } else if do_save {
                self.save(false);
            }
        });
    }

    fn import_dialog(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("Map data", smf_io::IMPORT_EXTENSIONS)
            .pick_files();
        for f in files.unwrap_or_default() {
            self.import_file(f);
        }
    }

    fn import_file(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match smf_io::import_path(&path) {
            Ok(imp) if imp.features.is_empty() => {
                let mut msg = format!("{name} contained no features I can show.");
                for w in &imp.warnings {
                    msg.push_str(&format!(
                        "
- {w}"
                    ));
                }
                alert(self.quiet, "Nothing imported", msg);
            }
            Ok(imp) => {
                let warnings = imp.warnings.clone();
                let ids = smf_io::apply_import(&mut self.editor, imp);
                self.editor.selection = None;
                if let Some(b) = bounds_of(&self.editor, &ids) {
                    // Several files at once (drop/multi-select): frame them all, not just the last.
                    self.map.pending_fit = Some(match self.map.pending_fit {
                        Some(p) => [
                            p[0].min(b[0]),
                            p[1].min(b[1]),
                            p[2].max(b[2]),
                            p[3].max(b[3]),
                        ],
                        None => b,
                    });
                }
                self.say(format!("Imported {} feature(s) from {name}", ids.len()));
                if !warnings.is_empty() {
                    notify_warnings(
                        self.quiet,
                        &format!("Imported {name} with notes"),
                        &warnings,
                    );
                }
            }
            Err(e) => {
                alert(self.quiet, "Could not import", format!("{name}: {e}"));
            }
        }
    }

    fn export_dialog(&mut self, fmt: smf_io::Format) {
        let stem: String = self
            .editor
            .project
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let ext = fmt.extension();
        let Some(path) = rfd::FileDialog::new()
            .add_filter(fmt.label(), &[ext])
            .set_file_name(format!("{stem}.{ext}"))
            .save_file()
        else {
            return;
        };
        match smf_io::export_path(&self.editor.project, &path, fmt) {
            Ok(r) => {
                self.say(format!(
                    "Exported {} file(s) as {}",
                    r.files.len(),
                    fmt.label()
                ));
                if !r.warnings.is_empty() {
                    notify_warnings(self.quiet, "Exported with notes", &r.warnings);
                }
            }
            Err(e) => {
                alert(self.quiet, "Could not export", e.to_string());
            }
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        egui::Window::new("ShowMeFire Field").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.separator();
            ui.label(egui::RichText::new("Shortcuts").strong());
            egui::Grid::new("shortcuts").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                for (k, v) in [
                    ("V / M / L / P / R / T / D / F", "Select, marker, line, polygon, range rings, label, measure, freehand"),
                    ("Enter / double-click", "Finish a line or polygon"),
                    ("Esc / Backspace", "Cancel / remove the last point"),
                    ("Delete", "Delete the selection"),
                    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
                    ("Ctrl+N / O / S", "New / open / save"),
                    ("Scroll / drag", "Zoom / pan (3D: dolly / pan on the ground)"),
                    ("Right-drag", "Rotate (3D: orbit and tilt)"),
                    ("Shift while drawing", "Don't snap to existing points"),
                    ("Hover or click the map", "Conditions at the pointer; click a fire feature for details"),
                ] {
                    ui.label(egui::RichText::new(k).monospace());
                    ui.label(v);
                    ui.end_row();
                }
            });
            ui.separator();
            ui.label(egui::RichText::new("Credits").strong());
            ui.label("Fire data, conditions and forecast: Show Me Fire (showmefire.org).");
            ui.label("Basemaps: USGS The National Map, Esri, OpenStreetMap contributors, OpenTopoMap.");
            ui.label("Terrain: Mapzen, USGS 3DEP, SRTM and other open elevation sources.");
            ui.label("Built with Rust, wgpu and egui.");
            ui.separator();
            let url = std::env::var("SMF_UPDATE_URL").ok().filter(|u| !u.trim().is_empty());
            let state = self.update.lock().unwrap().clone();
            match (&url, &state) {
                (None, _) => {
                    ui.weak("Update checks are off (set SMF_UPDATE_URL to a release manifest to enable).");
                }
                (Some(_), update::UpdateState::Checking) => {
                    ui.label("Checking for updates...");
                }
                (Some(u), _) => {
                    if ui.button("Check for updates").clicked() {
                        *self.update.lock().unwrap() = update::UpdateState::Checking;
                        let (u, st, c) = (u.clone(), self.update.clone(), ctx.clone());
                        std::thread::spawn(move || {
                            *st.lock().unwrap() = update::check(&u);
                            c.request_repaint();
                        });
                    }
                }
            }
            match state {
                update::UpdateState::UpToDate => {
                    ui.label("You are up to date.");
                }
                update::UpdateState::Available(r) => {
                    ui.colored_label(egui::Color32::from_rgb(80, 200, 120), format!("Version {} is available.", r.version));
                    if !r.notes.is_empty() {
                        ui.label(&r.notes);
                    }
                    if ui.link("Open download page").clicked() {
                        let _ = open::that(&r.url);
                    }
                }
                update::UpdateState::Failed(e) => {
                    ui.colored_label(egui::Color32::from_rgb(230, 130, 90), e);
                }
                _ => {}
            }
        });
        self.about_open = open;
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let name = self.path.as_ref().and_then(|p| p.file_name()).map_or_else(
            || self.editor.project.name.clone(),
            |n| n.to_string_lossy().into_owned(),
        );
        let title = format!(
            "{}{} - ShowMeFire Field",
            name,
            if self.dirty() { " *" } else { "" }
        );
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

impl eframe::App for FieldApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.confirm_discard() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.update_title(&ctx);
        self.about_window(&ctx);
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for p in dropped {
            self.import_file(p);
        }
        self.menu(ui);
        self.map.show(ui, &mut self.editor);
        self.screenshot_hook(&ctx);
        if let Some((path, scale, dpi, when)) = self.pending_print.clone() {
            if std::time::Instant::now() >= when {
                self.pending_print = None;
                self.map.dev_print(
                    &self.editor,
                    path,
                    print_ui::PrintKind::Pdf,
                    scale,
                    dpi,
                    0,
                    None,
                );
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
        if self.exit_on_print {
            match self.map.print_outcome() {
                Some(Ok(msg)) => {
                    tracing::info!("print finished: {msg}");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(Err(e)) => {
                    tracing::error!("print failed: {e}");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }
    }
}

impl FieldApp {
    fn screenshot_hook(&mut self, ctx: &egui::Context) {
        let Some(hook) = &mut self.shot else { return };
        if !hook.requested && hook.started.elapsed() > hook.settle {
            hook.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
            let buf = image::RgbaImage::from_raw(img.width() as u32, img.height() as u32, rgba)
                .expect("screenshot buffer size");
            match buf.save(&hook.path) {
                Ok(()) => tracing::info!("screenshot saved to {}", hook.path.display()),
                Err(e) => tracing::error!("screenshot failed: {e}"),
            }
            // Skip the unsaved-changes prompt: this is an automated run.
            self.saved_revision = self.editor.revision;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// Bounding box (min_lng, min_lat, max_lng, max_lat) of the given features, padded so a single point isn't zoomed to max.
fn bounds_of(editor: &Editor, ids: &[u64]) -> Option<[f64; 4]> {
    let mut b: Option<[f64; 4]> = None;
    for id in ids {
        for p in editor.project.feature(*id)?.geometry.vertices() {
            let e = b.get_or_insert([p.lng, p.lat, p.lng, p.lat]);
            *e = [
                e[0].min(p.lng),
                e[1].min(p.lat),
                e[2].max(p.lng),
                e[3].max(p.lat),
            ];
        }
    }
    b.map(|b| {
        if (b[2] - b[0]).abs() < 1e-5 && (b[3] - b[1]).abs() < 1e-5 {
            [b[0] - 0.005, b[1] - 0.005, b[2] + 0.005, b[3] + 0.005]
        } else {
            b
        }
    })
}

fn notify_warnings(quiet: bool, title: &str, warnings: &[String]) {
    let mut msg = String::new();
    for w in warnings.iter().take(12) {
        msg.push_str(&format!(
            "- {w}
"
        ));
    }
    if warnings.len() > 12 {
        msg.push_str(&format!(
            "...and {} more
",
            warnings.len() - 12
        ));
    }
    alert(quiet, title, msg);
}

/// Modal message box, except in automated runs (SMF_SCREENSHOT / SMF_IMPORT) where it would block
/// forever: there the message goes to the log instead.
fn alert(quiet: bool, title: &str, msg: impl Into<String>) {
    let msg = msg.into();
    if quiet {
        tracing::warn!("{title}: {msg}");
    } else {
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(msg)
            .show();
    }
}
