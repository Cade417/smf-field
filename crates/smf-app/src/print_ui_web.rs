//! Printing is not in the web app yet: the desktop app renders and lays out PDFs and GeoTIFFs
//! with local files. These stand-ins keep the menus and the map panel identical; the menu items
//! simply explain where the feature lives.

use crate::map_panel::MapPanel;
use eframe::egui;
use smf_features::Editor;
use web_time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrintKind {
    Pdf,
    GeoTiff,
}

#[derive(Default)]
pub struct PrintUi;

impl MapPanel {
    pub(crate) fn open_print(&mut self, _kind: PrintKind) {
        self.notice = Some((
            "Printing and GeoTIFF export are in the desktop app for now.".into(),
            Instant::now(),
        ));
    }

    pub(crate) fn print_window(&mut self, _ctx: &egui::Context, _editor: &Editor) {}

    pub(crate) fn print_outcome(&self) -> Option<Result<String, String>> {
        None
    }
}
