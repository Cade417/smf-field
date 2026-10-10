//! Google Earth export: the visible area of the current basemap as a KML SuperOverlay (KMZ).
//! Tiles are fetched with the offline downloader, so the same licence rules apply: sources
//! whose terms forbid bulk download (OSM, OpenTopoMap, Esri imagery) are refused.

use crate::map_panel::MapPanel;
use eframe::egui;
use smf_tiles::{DownloadSpec, Mbtiles, estimate};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

const DEFAULT_SPAN: u8 = 4;

pub struct EarthUi {
    pub open: bool,
    pub max_zoom: u8,
    pub levels: u8,
    /// Verification hook (SMF_EARTH): export to this file on the first laid-out frame.
    pub dev_out: Option<PathBuf>,
    error: Option<String>,
    job: Option<Job>,
}

struct Job {
    download: Option<smf_tiles::Download>,
    progress: Arc<smf_tiles::Progress>,
    tmp: PathBuf,
    out: PathBuf,
    name: String,
    bounds: [f64; 4],
    zooms: (u8, u8),
    /// Set once the downloaded tiles are being packaged.
    packing: Option<Receiver<Result<usize, String>>>,
}

impl Default for EarthUi {
    fn default() -> Self {
        Self {
            open: false,
            max_zoom: 14,
            levels: DEFAULT_SPAN,
            dev_out: None,
            error: None,
            job: None,
        }
    }
}

impl MapPanel {
    pub(crate) fn open_earth(&mut self) {
        self.earth.open = true;
        self.earth.error = None;
        // Suggest the current zoom: that is the detail the user is looking at.
        self.earth.max_zoom = (self.view.zoom.round() as u8).clamp(6, 17);
    }

    fn earth_source(&self) -> std::sync::Arc<smf_tiles::TileSource> {
        self.sources[self.selected].clone()
    }

    pub(crate) fn earth_window(&mut self, ctx: &egui::Context) {
        self.earth_poll(ctx);
        if self.view.size[0] > 200.0
            && let Some(o) = self.earth.dev_out.take()
        {
            self.earth_start(ctx, Some(o));
        }
        if !self.earth.open {
            return;
        }
        let src = self.earth_source();
        let bounds = self.earth_bounds();
        let max_z = self.earth.max_zoom.min(src.max_zoom);
        let min_z = max_z.saturating_sub(self.earth.levels.saturating_sub(1));
        let est = estimate(&src, bounds, min_z, max_z);
        let mut open = true;
        let mut start = false;
        egui::Window::new("Export Google Earth SuperOverlay")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                if let Some(job) = &self.earth.job {
                    let p = &job.progress;
                    let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                    if job.packing.is_some() {
                        ui.label("Packaging the KMZ...");
                        ui.add(egui::ProgressBar::new(1.0));
                    } else {
                        ui.label(format!("Downloading tiles: {done} of {total}"));
                        ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32));
                        if ui.button("Cancel").clicked()
                            && let Some(d) = &job.download
                        {
                            d.cancel();
                        }
                    }
                    return;
                }
                ui.label(format!(
                    "Exports what you see ({}) as a Google Earth KMZ that loads detail as you zoom in.",
                    src.name
                ));
                ui.add(egui::Slider::new(&mut self.earth.max_zoom, 6..=src.max_zoom.min(18)).text("detail (max zoom)"));
                ui.add(egui::Slider::new(&mut self.earth.levels, 1..=6).text("zoom levels"));
                ui.label(format!(
                    "Zoom {min_z} to {max_z}: about {} tiles, roughly {:.0} MB",
                    est.tiles,
                    est.approx_bytes as f64 / 1e6
                ));
                if !src.offline_allowed {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 120, 90),
                        format!("{} doesn't allow bulk export. Pick a different basemap (e.g. USGS).", src.name),
                    );
                }
                if let Some(e) = &self.earth.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 120, 90), e);
                }
                ui.add_enabled_ui(src.offline_allowed, |ui| {
                    if ui.button("Export...").clicked() {
                        start = true;
                    }
                });
            });
        if start {
            self.earth_start(ctx, None);
        }
        // The window closes only when idle; a running export keeps its window.
        self.earth.open = open || self.earth.job.is_some();
    }

    fn earth_bounds(&self) -> [f64; 4] {
        let [w, h] = self.view.size;
        let a = self.view.screen_to_lnglat([0.0, 0.0]);
        let b = self.view.screen_to_lnglat([w, h]);
        let (c, d) = (
            self.view.screen_to_lnglat([w, 0.0]),
            self.view.screen_to_lnglat([0.0, h]),
        );
        let lngs = [a.lng, b.lng, c.lng, d.lng];
        let lats = [a.lat, b.lat, c.lat, d.lat];
        [
            lngs.iter().copied().fold(f64::MAX, f64::min),
            lats.iter().copied().fold(f64::MAX, f64::min),
            lngs.iter().copied().fold(f64::MIN, f64::max),
            lats.iter().copied().fold(f64::MIN, f64::max),
        ]
    }

    fn earth_start(&mut self, ctx: &egui::Context, out: Option<PathBuf>) {
        let src = self.earth_source();
        let bounds = self.earth_bounds();
        let max_z = self.earth.max_zoom.min(src.max_zoom);
        let min_z = max_z.saturating_sub(self.earth.levels.saturating_sub(1));
        let Some(out) = out.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Google Earth KMZ", &["kmz"])
                .set_file_name(format!("{}.kmz", src.name.replace(' ', "_")))
                .save_file()
        }) else {
            return;
        };
        let tmp = self
            .cache_dir
            .join(format!("earth-{}.mbtiles", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let spec = DownloadSpec {
            source: (*src).clone(),
            bounds,
            min_zoom: min_z,
            max_zoom: max_z,
            out: tmp.clone(),
            max_tiles: 0,
            workers: 0,
        };
        match smf_tiles::start_download(spec) {
            Ok(d) => {
                self.earth.error = None;
                self.earth.job = Some(Job {
                    progress: d.progress.clone(),
                    download: Some(d),
                    tmp,
                    out,
                    name: src.name.clone(),
                    bounds,
                    zooms: (min_z, max_z),
                    packing: None,
                });
                ctx.request_repaint();
            }
            Err(e) => self.earth.error = Some(e.to_string()),
        }
    }

    fn earth_poll(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.earth.job else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        if job.packing.is_none() {
            if job.download.as_ref().is_some_and(|d| d.is_finished()) {
                let (tmp, out, name, bounds, zooms) = (
                    job.tmp.clone(),
                    job.out.clone(),
                    job.name.clone(),
                    job.bounds,
                    job.zooms,
                );
                let cancelled = job.progress.cancelled.load(Ordering::Relaxed);
                if let Some(d) = job.download.take() {
                    d.join();
                }
                if cancelled {
                    let _ = std::fs::remove_file(&tmp);
                    self.earth.job = None;
                    self.notice = Some(("Export cancelled.".into(), std::time::Instant::now()));
                    return;
                }
                let (tx, rx) = channel();
                job.packing = Some(rx);
                std::thread::spawn(move || {
                    let r = pack(&tmp, &out, &name, bounds, zooms);
                    let _ = std::fs::remove_file(&tmp);
                    let _ = tx.send(r);
                });
            }
            return;
        }
        let result = job.packing.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(r) = result {
            let out = job.out.clone();
            self.earth.job = None;
            match r {
                Ok(n) => {
                    self.notice = Some((
                        format!("Exported {n} tiles to {}", out.display()),
                        std::time::Instant::now(),
                    ));
                }
                Err(e) => self.earth.error = Some(e),
            }
        }
    }
}

/// Read the downloaded MBTiles and write the KMZ.
fn pack(
    tmp: &std::path::Path,
    out: &std::path::Path,
    name: &str,
    bounds: [f64; 4],
    z: (u8, u8),
) -> Result<usize, String> {
    let mb = Mbtiles::open_read_only(tmp).map_err(|e| e.to_string())?;
    let file = std::fs::File::create(out)
        .map_err(|e| format!("could not create {}: {e}", out.display()))?;
    smf_io::write_superoverlay(file, name, bounds, z.0, z.1, &mut |t| mb.get(t))
        .map_err(|e| e.to_string())
}
