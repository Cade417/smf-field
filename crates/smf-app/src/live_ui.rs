//! Live team tracking on the map: Garmin inReach MapShare feeds and APRS stations. Sources
//! are configured in the Tracking menu, saved between runs, and each shows as a labelled
//! marker with a short trail and the age of its last report.

use crate::map_panel::MapPanel;
use crate::overlay::{self, Projector};
use eframe::egui::{self, Align2, Color32, Stroke};
use serde::{Deserialize, Serialize};
use smf_geo::LngLat;
use smf_live::{Event, Handle, LiveSource, Network, Report};
use std::collections::{HashMap, VecDeque};

const MAX_TRAIL: usize = 80;
/// Beyond this a marker is shown greyed: it is where the person was, not where they are.
const STALE_SECS: u64 = 3600;
const INREACH_COLOR: Color32 = Color32::from_rgb(235, 130, 20);
const APRS_COLOR: Color32 = Color32::from_rgb(30, 160, 90);

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Cfg {
    /// "inreach" or "aprs"
    pub kind: String,
    /// MapShare name, or the APRS filter text as typed (callsigns or `area:lat,lng,km`).
    pub id: String,
    #[serde(default)]
    pub password: String,
    /// APRS login callsign (optional).
    #[serde(default)]
    pub callsign: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl Cfg {
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "inreach" => format!("inReach: {}", self.id),
            _ => format!("APRS: {}", self.id),
        }
    }

    pub fn source(&self) -> Result<LiveSource, String> {
        match self.kind.as_str() {
            "inreach" => {
                // Reject a bad name now rather than at the first poll.
                smf_live::mapshare_url(&self.id, None)?;
                Ok(LiveSource::InReach {
                    id: self.id.trim().rsplit('/').next().unwrap_or("").to_string(),
                    password: Some(self.password.clone()).filter(|p| !p.is_empty()),
                })
            }
            "aprs" => Ok(LiveSource::Aprs {
                server: smf_live::APRS_DEFAULT_SERVER.into(),
                filter: smf_live::aprs_filter(&self.id)?,
                callsign: Some(self.callsign.clone()).filter(|c| !c.trim().is_empty()),
            }),
            k => Err(format!("unknown tracking source \"{k}\"")),
        }
    }
}

struct Station {
    last: Report,
    trail: VecDeque<LngLat>,
}

struct Running {
    cfg: Cfg,
    /// None when the source could not even start (the reason is in `status`).
    handle: Option<Handle>,
    status: String,
}

#[derive(Default)]
struct AddForm {
    kind_aprs: bool,
    id: String,
    password: String,
    callsign: String,
    error: Option<String>,
}

pub struct LiveState {
    cfgs: Vec<Cfg>,
    running: Vec<Running>,
    stations: HashMap<(Network, String), Station>,
    add: Option<AddForm>,
    /// Verification hook (SMF_LIVE): start this source on the first frame.
    pub dev_source: Option<Cfg>,
}

impl LiveState {
    pub fn new() -> Self {
        let cfgs: Vec<Cfg> = crate::store::load_json("live.json");
        let mut s = Self {
            cfgs,
            running: vec![],
            stations: HashMap::new(),
            add: None,
            dev_source: None,
        };
        for i in 0..s.cfgs.len() {
            if s.cfgs[i].enabled {
                s.start(i);
            }
        }
        s
    }

    fn save(&self) {
        crate::store::save_json("live.json", &self.cfgs);
    }

    fn start(&mut self, i: usize) {
        let cfg = self.cfgs[i].clone();
        match cfg.source() {
            Ok(src) => self.running.push(Running {
                cfg,
                handle: Some(smf_live::spawn(src)),
                status: "Connecting...".into(),
            }),
            // A source that can't start still appears, with the reason, so it can be removed.
            Err(e) => self.running.push(Running {
                handle: None,
                cfg,
                status: e,
            }),
        }
    }

    fn stop(&mut self, cfg: &Cfg) {
        self.running.retain(|r| &r.cfg != cfg);
    }

    fn is_running(&self, cfg: &Cfg) -> bool {
        self.running.iter().any(|r| &r.cfg == cfg)
    }

    /// Fold a report into its station: newest position wins, and the trail only grows when it moved.
    fn apply(&mut self, r: Report) {
        let key = (r.network, r.id.clone());
        let p = LngLat::new(r.lng, r.lat);
        match self.stations.get_mut(&key) {
            Some(s) => {
                // A feed refresh replays older points; keep whichever report is newer.
                let newer = match (r.time_unix, s.last.time_unix) {
                    (Some(a), Some(b)) => a >= b,
                    _ => true,
                };
                if s.trail.back() != Some(&p) {
                    // Insert replayed history in order by appending only when newer.
                    if newer {
                        s.trail.push_back(p);
                        while s.trail.len() > MAX_TRAIL {
                            s.trail.pop_front();
                        }
                    }
                }
                if newer {
                    s.last = r;
                }
            }
            None => {
                self.stations.insert(
                    key,
                    Station {
                        trail: VecDeque::from([p]),
                        last: r,
                    },
                );
            }
        }
    }

    pub fn station_count(&self) -> usize {
        self.stations.len()
    }
}

/// "just now", "3 min ago", "2 h ago", "4 d ago".
pub fn age_text(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

impl MapPanel {
    pub(crate) fn live_menu(&mut self, ui: &mut egui::Ui) {
        let mut remove: Option<usize> = None;
        let mut toggle: Option<usize> = None;
        let mut save = false;
        for (i, c) in self.live.cfgs.iter().enumerate() {
            let on = self.live.is_running(c);
            ui.horizontal(|ui| {
                let mut v = on;
                if ui.checkbox(&mut v, c.label()).changed() {
                    toggle = Some(i);
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
            if let Some(r) = self.live.running.iter().find(|r| &r.cfg == c) {
                ui.weak(&r.status);
            }
        }
        if let Some(i) = toggle {
            let c = self.live.cfgs[i].clone();
            if self.live.is_running(&c) {
                self.live.stop(&c);
                self.live.cfgs[i].enabled = false;
            } else {
                self.live.cfgs[i].enabled = true;
                self.live.start(i);
            }
            save = true;
        }
        if let Some(i) = remove {
            let c = self.live.cfgs.remove(i);
            self.live.stop(&c);
            save = true;
        }
        if save {
            self.live.save();
        }
        if self.live.cfgs.is_empty() {
            ui.weak("No tracking sources yet.");
        }
        ui.separator();
        if ui.button("Add inReach MapShare...").clicked() {
            self.live.add = Some(AddForm::default());
            ui.close();
        }
        if ui.button("Add APRS stations...").clicked() {
            self.live.add = Some(AddForm {
                kind_aprs: true,
                ..AddForm::default()
            });
            ui.close();
        }
        if ui
            .button("Follow APRS within 100 km of the map centre")
            .clicked()
        {
            let c = self.view.center;
            let cfg = Cfg {
                kind: "aprs".into(),
                id: format!("area:{:.3},{:.3},100", c.lat, c.lng),
                enabled: true,
                ..Cfg::default()
            };
            self.live_add(cfg);
            ui.close();
        }
        ui.separator();
        ui.label(format!(
            "{} station(s) on the map",
            self.live.station_count()
        ));
        if ui.button("Clear stations").clicked() {
            self.live.stations.clear();
        }
    }

    fn live_add(&mut self, cfg: Cfg) {
        if self.live.cfgs.contains(&cfg) {
            return;
        }
        self.live.cfgs.push(cfg);
        let i = self.live.cfgs.len() - 1;
        self.live.start(i);
        self.live.save();
    }

    pub(crate) fn live_window(&mut self, ctx: &egui::Context) {
        let Some(mut f) = self.live.add.take() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new(if f.kind_aprs {
            "Add APRS stations"
        } else {
            "Add inReach MapShare"
        })
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            if f.kind_aprs {
                ui.label("Callsigns, separated by commas (wildcards like W0* work), or area:lat,lng,km.");
                ui.add(egui::TextEdit::singleline(&mut f.id).hint_text("W0ABC-9, K0XYZ").desired_width(280.0));
                ui.label("Your callsign (optional, used only to log in read-only):");
                ui.add(egui::TextEdit::singleline(&mut f.callsign).desired_width(140.0));
                ui.weak("Receives only; this app never transmits on APRS.");
            } else {
                ui.label("The MapShare name from the person's share link (share.garmin.com/NAME).");
                ui.add(egui::TextEdit::singleline(&mut f.id).hint_text("MapShare name").desired_width(280.0));
                ui.label("Feed password, if the owner set one:");
                ui.add(egui::TextEdit::singleline(&mut f.password).password(true).desired_width(200.0));
                ui.weak("Saved unencrypted in this app's settings folder. Only the owner's own feed settings can grant access.");
            }
            if let Some(e) = &f.error {
                ui.colored_label(Color32::from_rgb(230, 120, 90), e);
            }
            if ui.button("Add").clicked() {
                submit = true;
            }
        });
        if submit {
            let cfg = Cfg {
                kind: if f.kind_aprs { "aprs" } else { "inreach" }.into(),
                id: f.id.trim().to_string(),
                password: f.password.clone(),
                callsign: f.callsign.trim().to_string(),
                enabled: true,
            };
            match cfg.source() {
                Ok(_) => {
                    self.live_add(cfg);
                    return;
                }
                Err(e) => f.error = Some(e),
            }
        }
        if open {
            self.live.add = Some(f);
        }
    }

    pub(crate) fn poll_live(&mut self, ctx: &egui::Context) {
        if let Some(cfg) = self.live.dev_source.take() {
            self.live_add(cfg);
        }
        let mut reports = vec![];
        for r in &mut self.live.running {
            let Some(h) = &r.handle else { continue };
            while let Ok(e) = h.rx.try_recv() {
                match e {
                    Event::Reports(v) => {
                        r.status = format!("{} update(s)", v.len());
                        reports.extend(v);
                    }
                    Event::Status(s) => r.status = s,
                    Event::Error(e) => r.status = e,
                }
            }
        }
        if !reports.is_empty() {
            for r in reports {
                self.live.apply(r);
            }
            ctx.request_repaint();
        }
        if !self.live.running.is_empty() {
            // Keep the "x min ago" labels current.
            ctx.request_repaint_after(std::time::Duration::from_secs(30));
        }
    }

    pub(crate) fn draw_live(&self, painter: &egui::Painter, pj: &Projector, rect: egui::Rect) {
        let now = smf_data::unix_now();
        let visible = rect.expand(80.0);
        for ((net, _), s) in &self.live.stations {
            let age = s.last.time_unix.map(|t| now.saturating_sub(t));
            let stale = age.is_some_and(|a| a > STALE_SECS);
            let base = if *net == Network::InReach {
                INREACH_COLOR
            } else {
                APRS_COLOR
            };
            let col = if stale { Color32::from_gray(140) } else { base };
            let pts: Vec<LngLat> = s.trail.iter().copied().collect();
            for seg in pj.polyline(&pts) {
                painter.add(egui::Shape::line(
                    seg,
                    Stroke::new(2.0, col.gamma_multiply(0.7)),
                ));
            }
            let Some(c) = pj
                .pos(LngLat::new(s.last.lng, s.last.lat))
                .filter(|c| visible.contains(*c))
            else {
                continue;
            };
            let r = 7.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    c + egui::vec2(0.0, -r),
                    c + egui::vec2(r, 0.0),
                    c + egui::vec2(0.0, r),
                    c + egui::vec2(-r, 0.0),
                ],
                col,
                Stroke::new(2.0, Color32::WHITE),
            ));
            if s.last.emergency {
                painter.circle_stroke(c, 14.0, Stroke::new(3.0, Color32::from_rgb(230, 30, 30)));
            }
            let mut label = s.last.name.clone();
            if let Some(a) = age {
                label.push_str(&format!(" · {}", age_text(a)));
            }
            if s.last.emergency {
                label.push_str(" · SOS");
            }
            overlay::label(
                painter,
                c + egui::vec2(0.0, 12.0),
                Align2::CENTER_TOP,
                &label,
                12.0,
                Color32::BLACK,
            );
        }
    }

    /// Details of the live station nearest a click, if within tolerance.
    pub(crate) fn live_pick(
        live: &LiveState,
        proj: &dyn Fn(LngLat) -> [f64; 2],
        pt: [f64; 2],
        tol: f64,
    ) -> Option<String> {
        let now = smf_data::unix_now();
        let best = live
            .stations
            .values()
            .map(|s| {
                let p = proj(LngLat::new(s.last.lng, s.last.lat));
                ((p[0] - pt[0]).hypot(p[1] - pt[1]), s)
            })
            .filter(|(d, _)| *d <= tol * 1.6)
            .min_by(|a, b| a.0.total_cmp(&b.0))?
            .1;
        let r = &best.last;
        let mut t = format!(
            "{} ({}): {:.5}, {:.5}",
            r.name,
            if r.network == Network::InReach {
                "inReach"
            } else {
                "APRS"
            },
            r.lat,
            r.lng
        );
        if let Some(ts) = r.time_unix {
            t.push_str(&format!(", {}", age_text(now.saturating_sub(ts))));
        }
        if let Some(s) = r.speed_mps.filter(|s| *s > 0.3) {
            t.push_str(&format!(", {:.1} km/h", s * 3.6));
        }
        if !r.text.is_empty() {
            t.push_str(&format!(" - {}", r.text));
        }
        Some(t)
    }

    pub(crate) fn live_credit(&self) -> Option<&'static str> {
        let has = |n: Network| self.live.stations.keys().any(|(k, _)| *k == n);
        match (has(Network::InReach), has(Network::Aprs)) {
            (true, true) => Some("Garmin inReach / APRS-IS"),
            (true, false) => Some("Garmin inReach"),
            (false, true) => Some("APRS-IS"),
            (false, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_live::Network;

    fn rep(id: &str, t: u64, lat: f64) -> Report {
        Report {
            network: Network::Aprs,
            id: id.into(),
            name: id.into(),
            lat,
            lng: -92.0,
            time_unix: Some(t),
            speed_mps: None,
            course_deg: None,
            alt_m: None,
            text: String::new(),
            emergency: false,
        }
    }

    fn state() -> LiveState {
        LiveState {
            cfgs: vec![],
            running: vec![],
            stations: HashMap::new(),
            add: None,
            dev_source: None,
        }
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age_text(5), "just now");
        assert_eq!(age_text(125), "2 min ago");
        assert_eq!(age_text(7300), "2 h ago");
        assert_eq!(age_text(3 * 86_400 + 5), "3 d ago");
    }

    #[test]
    fn stations_keep_the_newest_report_and_a_bounded_trail() {
        let mut s = state();
        s.apply(rep("A", 100, 38.0));
        s.apply(rep("A", 200, 38.001));
        // An older replayed point must not move the marker back.
        s.apply(rep("A", 150, 38.0005));
        let st = &s.stations[&(Network::Aprs, "A".to_string())];
        assert_eq!(st.last.time_unix, Some(200));
        assert_eq!(st.trail.len(), 2);
        // The same position twice doesn't lengthen the trail.
        s.apply(rep("A", 300, 38.001));
        assert_eq!(s.stations[&(Network::Aprs, "A".to_string())].trail.len(), 2);
        // The trail is capped.
        for i in 0..200 {
            s.apply(rep("B", 1000 + i, 38.0 + i as f64 * 1e-4));
        }
        assert_eq!(
            s.stations[&(Network::Aprs, "B".to_string())].trail.len(),
            MAX_TRAIL
        );
        assert_eq!(s.station_count(), 2);
    }

    #[test]
    fn config_validates_before_starting() {
        let ok = Cfg {
            kind: "aprs".into(),
            id: "w0abc-9, k0xyz".into(),
            ..Cfg::default()
        };
        assert!(
            matches!(ok.source().unwrap(), LiveSource::Aprs { filter, .. } if filter == "b/W0ABC-9/K0XYZ")
        );
        let bad = Cfg {
            kind: "aprs".into(),
            id: "x;y".into(),
            ..Cfg::default()
        };
        assert!(bad.source().is_err());
        let ir = Cfg {
            kind: "inreach".into(),
            id: "https://share.garmin.com/MyShare".into(),
            password: "pw".into(),
            ..Cfg::default()
        };
        assert!(
            matches!(ir.source().unwrap(), LiveSource::InReach { id, password } if id == "MyShare" && password.as_deref() == Some("pw"))
        );
        assert!(
            Cfg {
                kind: "inreach".into(),
                id: "bad name".into(),
                ..Cfg::default()
            }
            .source()
            .is_err()
        );
        assert!(
            Cfg {
                kind: "radio".into(),
                ..Cfg::default()
            }
            .source()
            .is_err()
        );
    }

    #[test]
    fn saved_config_round_trips_and_tolerates_old_files() {
        let c = Cfg {
            kind: "inreach".into(),
            id: "Name".into(),
            password: "secret".into(),
            callsign: String::new(),
            enabled: false,
        };
        let back: Vec<Cfg> =
            serde_json::from_str(&serde_json::to_string(&vec![c.clone()]).unwrap()).unwrap();
        assert_eq!(back, vec![c]);
        // Missing optional fields default (enabled on).
        let old: Vec<Cfg> = serde_json::from_str(r#"[{"kind":"aprs","id":"W0ABC"}]"#).unwrap();
        assert!(old[0].enabled && old[0].password.is_empty());
    }
}
