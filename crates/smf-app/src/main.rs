//! ShowMeFire Field desktop entry point.

mod map_panel;
mod overlay;
mod panels;

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
            shot,
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
    /// camera; SMF_SLOPE=deg|fire turns on slope shading; SMF_SELECT=<feature name> selects it.
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
        if let Ok(v) = std::env::var("SMF_SLOPE") {
            for p in [&mut self.map.shade2d, &mut self.map.shade3d] {
                p.slope = true;
                p.slope_fire_preset = v == "fire";
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
        !self.dirty()
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
                rfd::MessageDialog::new()
                    .set_title("Could not open map")
                    .set_description(e.to_string())
                    .show();
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
                rfd::MessageDialog::new()
                    .set_title("Could not save map")
                    .set_description(e.to_string())
                    .show();
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
            if do_save_as {
                self.save(true);
            } else if do_save {
                self.save(false);
            }
        });
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
        self.menu(ui);
        self.map.show(ui, &mut self.editor);
        self.screenshot_hook(&ctx);
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
