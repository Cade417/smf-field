//! ShowMeFire Field desktop entry point.

mod map_panel;

use eframe::egui;

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
    shot: Option<ScreenshotHook>,
}

/// Dev/verification hook: with SMF_SCREENSHOT=<file.png> the app lets tiles load,
/// saves its own rendered frame (never a screen grab, so other windows are not
/// captured) and exits. SMF_SCREENSHOT_SECS overrides the 12 s settle time.
struct ScreenshotHook {
    path: std::path::PathBuf,
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
        Self {
            map: map_panel::MapPanel::new(cc),
            shot,
        }
    }
}

impl eframe::App for FieldApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.map.show(ui, frame);
        if let Some(hook) = &mut self.shot {
            let ctx = ui.ctx().clone();
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
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
    }
}
