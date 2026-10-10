//! The Google Earth SuperOverlay export downloads many tiles to local files, so it is a desktop
//! feature. These stand-ins keep the map panel identical.

use crate::map_panel::MapPanel;
use eframe::egui;
use web_time::Instant;

#[derive(Default)]
pub struct EarthUi;

impl MapPanel {
    pub(crate) fn open_earth(&mut self) {
        self.notice = Some((
            "Google Earth export is in the desktop app.".into(),
            Instant::now(),
        ));
    }

    pub(crate) fn earth_window(&mut self, _ctx: &egui::Context) {}
}
