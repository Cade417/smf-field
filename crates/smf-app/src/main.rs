//! ShowMeFire Field desktop entry point.
//!
//! M0: opens a window and shows which wgpu adapter eframe selected, so GPU
//! problems on a given machine are visible before any map code exists.

use directories::ProjectDirs;
use eframe::egui;
use eframe::wgpu;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
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
        Box::new(|_cc| Ok(Box::new(FieldApp::default()))),
    )
}

#[derive(Default)]
struct FieldApp {
    adapter: Option<wgpu::AdapterInfo>,
}

impl eframe::App for FieldApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.adapter.is_none() {
            self.adapter = frame.wgpu_render_state().map(|rs| rs.adapter.get_info());
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("ShowMeFire Field");
            ui.label("M0 scaffold: map not implemented yet.");
            ui.separator();
            match &self.adapter {
                Some(a) => {
                    ui.label(format!("GPU: {}", a.name));
                    ui.label(format!("Backend: {:?}", a.backend));
                    ui.label(format!("Type: {:?}", a.device_type));
                }
                None => {
                    ui.label("No wgpu render state available.");
                }
            }
            if let Some(dirs) = ProjectDirs::from("org", "ShowMeFire", "Field") {
                ui.label(format!("Cache dir: {}", dirs.cache_dir().display()));
                ui.label(format!("Config dir: {}", dirs.config_dir().display()));
            }
        });
    }
}
