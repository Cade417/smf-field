//! GPS navigation: connect to a position source, show where you are on the map, follow it,
//! record a track, and navigate to the selected feature. The source threads live in
//! `smf-gps`; this module owns the UI state and the drawing.

use crate::map_panel::MapPanel;
use crate::overlay::Projector;
use eframe::egui::{self, Color32, Stroke};
use smf_features::{Editor, Geometry};
use smf_geo::{LngLat, bearing_deg, distance_m, format_distance, geodesic_circle};
use smf_gps::{Event, Fix, Handle, Source};
use std::path::PathBuf;
use std::time::Duration;
use web_time::Instant;

const BLUE: Color32 = Color32::from_rgb(30, 110, 240);
/// A fix older than this is shown as stale rather than as where you are now.
const STALE: Duration = Duration::from_secs(10);
/// Ignore accuracy circles that would cover the whole screen anyway.
const MAX_CIRCLE_M: f64 = 5_000.0;
const BAUDS: [u32; 5] = [4800, 9600, 19_200, 38_400, 115_200];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    System,
    Serial,
    Gpsd,
    Replay,
}

pub struct GpsState {
    pub kind: Kind,
    ports: Vec<String>,
    port: String,
    baud: u32,
    addr: String,
    replay: String,
    handle: Option<Handle>,
    pub fix: Option<(Fix, Instant)>,
    pub status: Option<String>,
    pub follow: bool,
    /// Points of the track being recorded, if recording.
    pub track: Option<Vec<LngLat>>,
    pub navigate: bool,
    /// Verification hook (SMF_GPS): connect to this on the first frame.
    pub dev_source: Option<Source>,
}

impl GpsState {
    pub fn new() -> Self {
        let ports = smf_gps::available_ports();
        Self {
            kind: if cfg!(any(windows, target_arch = "wasm32")) {
                Kind::System
            } else {
                Kind::Gpsd
            },
            port: ports.first().cloned().unwrap_or_default(),
            ports,
            baud: 4800,
            addr: "localhost:2947".into(),
            replay: String::new(),
            handle: None,
            fix: None,
            status: None,
            follow: true,
            track: None,
            navigate: true,
            dev_source: None,
        }
    }

    pub fn connected(&self) -> bool {
        self.handle.is_some()
    }

    fn connect(&mut self, src: Source) {
        self.disconnect();
        self.status = Some("Connecting...".into());
        self.handle = Some(smf_gps::spawn(src));
    }

    fn disconnect(&mut self) {
        // Dropping the handle stops the source thread.
        self.handle = None;
        self.fix = None;
    }

    fn source_from_ui(&self) -> Result<Source, String> {
        match self.kind {
            Kind::System => Ok(Source::System),
            Kind::Serial if self.port.is_empty() => Err("Choose a serial port first.".into()),
            Kind::Serial => Ok(Source::Serial {
                port: self.port.clone(),
                baud: self.baud,
            }),
            Kind::Gpsd => Ok(Source::Gpsd {
                addr: self.addr.clone(),
            }),
            Kind::Replay if self.replay.trim().is_empty() => Err("Choose an NMEA log file.".into()),
            Kind::Replay => Ok(Source::NmeaFile {
                path: PathBuf::from(self.replay.trim()),
                interval_ms: 500,
            }),
        }
    }

    pub fn position(&self) -> Option<LngLat> {
        self.fix.as_ref().map(|(f, _)| LngLat::new(f.lng, f.lat))
    }
}

/// "NE", "SW"... for a compass bearing.
fn compass8(bearing: f64) -> &'static str {
    const N: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    N[((bearing.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

/// Distance and bearing text such as "1.20 km at 135° SE".
pub fn nav_text(from: LngLat, to: LngLat) -> String {
    let b = bearing_deg(from, to);
    format!(
        "{} at {:.0}° {}",
        format_distance(distance_m(from, to)),
        b,
        compass8(b)
    )
}

/// Name for a saved track from its first fix time.
fn track_name(unix: u64) -> String {
    let secs = unix % 86_400;
    format!(
        "GPS track {} {:02}:{:02} UTC",
        smf_ext::iso_date(unix),
        secs / 3600,
        secs % 3600 / 60
    )
}

impl MapPanel {
    pub(crate) fn gps_menu(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        let g = &mut self.gps;
        ui.horizontal(|ui| {
            // Browsers can only use the device's own location; the rest need the desktop app.
            #[cfg(not(target_arch = "wasm32"))]
            let kinds = [
                (Kind::System, "This computer"),
                (Kind::Serial, "Serial GPS"),
                (Kind::Gpsd, "gpsd"),
                (Kind::Replay, "NMEA log"),
            ];
            #[cfg(target_arch = "wasm32")]
            let kinds = [(Kind::System, "This device")];
            for (k, label) in kinds {
                ui.selectable_value(&mut g.kind, k, label);
            }
        });
        match g.kind {
            Kind::System => {
                ui.weak("Uses the operating system's location (Wi-Fi/GNSS; accuracy varies).");
            }
            Kind::Serial => {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("gps-port")
                        .selected_text(if g.port.is_empty() {
                            "(no ports)"
                        } else {
                            &g.port
                        })
                        .show_ui(ui, |ui| {
                            for p in g.ports.clone() {
                                ui.selectable_value(&mut g.port, p.clone(), p);
                            }
                        });
                    egui::ComboBox::from_id_salt("gps-baud")
                        .selected_text(format!("{} baud", g.baud))
                        .show_ui(ui, |ui| {
                            for b in BAUDS {
                                ui.selectable_value(&mut g.baud, b, b.to_string());
                            }
                        });
                    if ui.button("Rescan").clicked() {
                        g.ports = smf_gps::available_ports();
                        if !g.ports.contains(&g.port) {
                            g.port = g.ports.first().cloned().unwrap_or_default();
                        }
                    }
                });
            }
            Kind::Gpsd => {
                ui.horizontal(|ui| {
                    ui.label("Address");
                    ui.text_edit_singleline(&mut g.addr);
                });
            }
            Kind::Replay => {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut g.replay);
                    #[cfg(not(target_arch = "wasm32"))]
                    if ui.button("Browse...").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        g.replay = p.display().to_string();
                    }
                });
            }
        }
        ui.horizontal(|ui| {
            if g.connected() {
                if ui.button("Disconnect").clicked() {
                    g.disconnect();
                    g.status = None;
                }
            } else if ui.button("Connect").clicked() {
                match g.source_from_ui() {
                    Ok(s) => g.connect(s),
                    Err(e) => g.status = Some(e),
                }
            }
        });
        if let Some(s) = &g.status {
            ui.label(s);
        }
        ui.separator();
        ui.checkbox(&mut g.follow, "Keep the map centred on me");
        ui.checkbox(
            &mut g.navigate,
            "Show distance and bearing to the selected feature",
        );
        let can_center = g.position().is_some();
        let mut centre = false;
        ui.add_enabled_ui(can_center, |ui| {
            centre = ui.button("Centre on me now").clicked();
        });
        ui.separator();
        if g.track.is_none() {
            if ui.button("Start recording a track").clicked() {
                g.track = Some(vec![]);
            }
        } else {
            let n = g.track.as_ref().map_or(0, Vec::len);
            ui.label(format!("Recording: {n} points"));
            ui.horizontal(|ui| {
                if ui.button("Stop and save").clicked() {
                    let pts = g.track.take().unwrap_or_default();
                    let when = g
                        .fix
                        .as_ref()
                        .and_then(|(f, _)| f.time_unix)
                        .unwrap_or_else(smf_data::unix_now);
                    if pts.len() >= 2 {
                        let id = editor.add(Geometry::Line(pts), Some(track_name(when)));
                        editor.selection = Some(id);
                    } else {
                        g.status = Some("Not enough points to save a track.".into());
                    }
                }
                if ui.button("Discard").clicked() {
                    g.track = None;
                }
            });
        }
        if centre && let Some(p) = self.gps.position() {
            self.centre_on(p, 15.0);
        }
    }

    /// Move the active camera to `p` (at least `min_zoom` in 2D).
    pub(crate) fn centre_on(&mut self, p: LngLat, min_zoom: f64) {
        self.view.center = p;
        self.view.zoom = self.view.zoom.max(min_zoom);
        self.cam.target = p;
        self.cam.target_elev_m = self.heights.sample(p).unwrap_or(0.0) as f64;
    }

    pub(crate) fn poll_gps(&mut self, _ctx: &egui::Context) {
        if let Some(src) = self.gps.dev_source.take() {
            self.gps.connect(src);
        }
        let mut events = vec![];
        if let Some(h) = &self.gps.handle {
            while let Ok(e) = h.rx.try_recv() {
                events.push(e);
            }
        }
        for e in events {
            match e {
                Event::Fix(f) => {
                    let p = LngLat::new(f.lng, f.lat);
                    self.gps.status = Some("Connected".into());
                    if let Some(t) = &mut self.gps.track {
                        // Skip jitter: the GPS wanders by about its own accuracy even when you stand still.
                        let min_step = (f.accuracy_m.unwrap_or(0.0) * 0.5).max(3.0);
                        if t.last().is_none_or(|l| distance_m(*l, p) >= min_step) {
                            t.push(p);
                        }
                    }
                    if self.gps.follow {
                        self.centre_on(p, 0.0);
                    }
                    self.gps.fix = Some((f, Instant::now()));
                }
                Event::NoFix => self.gps.status = Some("Waiting for a position fix...".into()),
                Event::Status(s) => self.gps.status = Some(s),
                Event::Error(e) => {
                    self.gps.status = Some(e);
                    self.gps.handle = None;
                }
            }
        }
        if self.gps.connected() {
            _ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    pub(crate) fn draw_gps(&self, painter: &egui::Painter, pj: &Projector) {
        if let Some(t) = &self.gps.track {
            for seg in pj.polyline(t) {
                painter.add(egui::Shape::line(
                    seg.clone(),
                    Stroke::new(5.0, Color32::WHITE),
                ));
                painter.add(egui::Shape::line(
                    seg,
                    Stroke::new(3.0, Color32::from_rgb(220, 40, 40)),
                ));
            }
        }
        let Some((fix, at)) = &self.gps.fix else {
            return;
        };
        let p = LngLat::new(fix.lng, fix.lat);
        let Some(c) = pj.pos(p) else { return };
        let stale = at.elapsed() > STALE;
        let col = if stale { Color32::from_gray(150) } else { BLUE };
        if let Some(acc) = fix.accuracy_m.filter(|a| *a > 1.0 && *a < MAX_CIRCLE_M) {
            for seg in pj.polyline(&geodesic_circle(p, acc, 48)) {
                painter.add(egui::Shape::line(
                    seg,
                    Stroke::new(1.5, col.gamma_multiply(0.7)),
                ));
            }
        }
        // Direction of travel, drawn toward where the course leads a short way ahead.
        if let (Some(course), Some(speed)) = (fix.course_deg, fix.speed_mps)
            && speed > 0.5
            && let Some(ahead) = pj.pos(smf_geo::destination(p, course, 25.0))
        {
            let d = (ahead - c).normalized();
            if d.is_finite() {
                let tip = c + d * 18.0;
                let side = egui::vec2(-d.y, d.x) * 6.0;
                painter.add(egui::Shape::convex_polygon(
                    vec![tip, c + side, c - side],
                    col,
                    Stroke::new(1.0, Color32::WHITE),
                ));
            }
        }
        painter.circle_filled(c, 9.0, Color32::WHITE);
        painter.circle_filled(c, 6.5, col);

        if let Some((target, _)) = self.nav_target(None) {
            painter.line_segment(
                [c, pj.pos(target).unwrap_or(c)],
                Stroke::new(2.0, Color32::from_rgb(240, 160, 0)),
            );
        }
    }

    /// Nearest vertex of the selected feature, with its feature name.
    fn nav_target(&self, editor: Option<&Editor>) -> Option<(LngLat, String)> {
        // The drawing path has no editor; it uses the target cached by the status pass.
        match editor {
            None => self.gps_target.clone(),
            Some(e) => {
                let from = self.gps.position()?;
                let f = e.project.feature(e.selection?)?;
                let v = f
                    .geometry
                    .vertices()
                    .into_iter()
                    .min_by(|a, b| distance_m(from, *a).total_cmp(&distance_m(from, *b)))?;
                Some((v, f.name.clone()))
            }
        }
    }

    /// Text for the status bar: position quality, speed, and the way to the selected feature.
    pub(crate) fn gps_status_text(&mut self, editor: &Editor) -> Option<String> {
        let (fix, at) = self.gps.fix.clone()?;
        let mut parts = vec![];
        parts.push(match fix.accuracy_m {
            Some(a) => format!("GPS ±{a:.0} m"),
            None => "GPS".to_string(),
        });
        if let Some(s) = fix.speed_mps {
            parts.push(format!("{:.1} km/h ({:.1} mph)", s * 3.6, s * 2.236_94));
        }
        if let Some(a) = fix.alt_m {
            parts.push(format!("{a:.0} m"));
        }
        if at.elapsed() > STALE {
            parts.push("(no recent fix)".into());
        }
        self.gps_target = None;
        if self.gps.navigate
            && let Some((t, name)) = self.nav_target(Some(editor))
        {
            let from = LngLat::new(fix.lng, fix.lat);
            let label = if name.is_empty() {
                "selection".to_string()
            } else {
                name.clone()
            };
            parts.push(format!("To {label}: {}", nav_text(from, t)));
            self.gps_target = Some((t, name));
        }
        Some(parts.join("  |  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_text_reads_naturally() {
        let a = LngLat::new(-92.0, 38.0);
        let t = nav_text(a, LngLat::new(-92.0, 38.01));
        assert!(t.starts_with("1.11 km"), "{t}");
        assert!(t.contains("0° N"), "{t}");
        assert!(nav_text(a, LngLat::new(-92.02, 38.0)).ends_with("W"));
    }

    #[test]
    fn track_names_use_utc_time() {
        // 2026-02-03 17:23:30 UTC
        assert_eq!(track_name(1_770_139_410), "GPS track 2026-02-03 17:23 UTC");
    }

    #[test]
    fn the_ui_builds_sources_and_refuses_empty_choices() {
        let mut g = GpsState::new();
        g.kind = Kind::Serial;
        g.port.clear();
        assert!(g.source_from_ui().is_err());
        g.port = "COM3".into();
        g.baud = 9600;
        assert_eq!(
            g.source_from_ui().unwrap(),
            Source::Serial {
                port: "COM3".into(),
                baud: 9600
            }
        );
        g.kind = Kind::Replay;
        assert!(g.source_from_ui().is_err());
        g.kind = Kind::Gpsd;
        assert!(matches!(g.source_from_ui().unwrap(), Source::Gpsd { .. }));
    }

    #[test]
    fn compass_names() {
        assert_eq!(compass8(0.0), "N");
        assert_eq!(compass8(359.0), "N");
        assert_eq!(compass8(135.0), "SE");
        assert_eq!(compass8(-90.0), "W");
    }
}
