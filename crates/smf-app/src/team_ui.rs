//! Team collaboration in the app: sign in to a Show Me Fire team, open or share a map that
//! everyone edits live, and (optionally) share your GPS position with teammates. The
//! protocol and conflict rules live in `smf-sync`; this module is the menu, the state
//! machine around it, and the drawing of teammates.

use crate::map_panel::MapPanel;
use crate::overlay::{self, Projector};
use eframe::egui::{self, Align2, Color32};
use serde::{Deserialize, Serialize};
use smf_features::Editor;
use smf_geo::LngLat;
use smf_sync::{
    Command, Creds, FieldClient, Handle, MapDoc, MapInfo, Me, Position, SyncError, SyncState,
    TeamPosition, Update,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use web_time::Instant;

/// How often local edits are compared with what the server has.
const DIFF_EVERY: Duration = Duration::from_millis(300);
/// How often our position is offered to the server while sharing.
const POSITION_EVERY: Duration = Duration::from_secs(5);
/// A GPS fix older than this is not shared as "where I am".
const FIX_MAX_AGE: Duration = Duration::from_secs(20);
const TEAM_COLOR: Color32 = Color32::from_rgb(150, 70, 220);

#[derive(Serialize, Deserialize, Default, Clone)]
struct Saved {
    server: String,
    token: String,
    member_id: String,
    team_name: String,
    display_name: String,
    #[serde(default)]
    last_map_id: String,
    #[serde(default)]
    last_map_name: String,
    #[serde(default)]
    share_location: bool,
}

enum Msg {
    SignedIn(Result<Creds, String>, String, String),
    Me(Result<Me, String>),
    Maps(Result<Vec<MapInfo>, String>),
    Opened(Result<MapDoc, String>),
    Shared(Result<(MapInfo, BTreeMap<String, serde_json::Value>), String>),
    JoinCode(Result<String, String>),
    Done(Result<(), String>),
}

#[derive(Default)]
struct Form {
    server: String,
    display: String,
    code: String,
    create_key: String,
    team_name: String,
    creating: bool,
}

struct Session {
    handle: Handle,
    map: Option<(String, String)>,
    state: SyncState,
    last_diff: Instant,
    /// Waiting for the full map after a reset.
    reloading: bool,
}

pub struct TeamState {
    saved: Saved,
    client: Option<FieldClient>,
    me: Option<Me>,
    maps: Vec<MapInfo>,
    pub status: Option<String>,
    form: Form,
    session: Option<Session>,
    teammates: HashMap<String, TeamPosition>,
    last_pos_sent: Option<Instant>,
    sharing_sent: bool,
    new_code: Option<String>,
    /// uids chosen for a map being shared, until the server accepts it.
    pending_share: Option<SyncState>,
    confirm_replace: Option<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Lets session callbacks ask for a repaint (set every frame).
    ctx: Option<egui::Context>,
    /// Verification hook (SMF_TEAM=url|code|name[|map name]): join and open a map at startup.
    pub dev: Option<(String, String, String, Option<String>)>,
    dev_open_map: Option<String>,
}

fn default_server() -> String {
    smf_data::default_base()
}

impl TeamState {
    pub fn new() -> Self {
        let saved: Saved = crate::store::load_json("team.json");
        let (tx, rx) = channel();
        let mut s = Self {
            form: Form {
                server: if saved.server.is_empty() {
                    default_server()
                } else {
                    saved.server.clone()
                },
                display: saved.display_name.clone(),
                ..Form::default()
            },
            saved,
            client: None,
            me: None,
            maps: vec![],
            status: None,
            session: None,
            teammates: HashMap::new(),
            last_pos_sent: None,
            sharing_sent: false,
            new_code: None,
            pending_share: None,
            confirm_replace: None,
            tx,
            rx,
            ctx: None,
            dev: None,
            dev_open_map: None,
        };
        s.restore();
        s
    }

    fn save(&self) {
        crate::store::save_json("team.json", &self.saved);
    }

    /// Resume a saved sign-in: check the token still works before relying on it.
    fn restore(&mut self) {
        if self.saved.token.is_empty() {
            return;
        }
        match FieldClient::new(&self.saved.server, Some(self.saved.token.clone())) {
            Ok(c) => {
                self.client = Some(c.clone());
                let tx = self.tx.clone();
                c.me(move |r| {
                    let _ = tx.send(Msg::Me(r.map_err(|e| e.to_string())));
                });
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    pub fn ctx_handle(&self) -> Option<egui::Context> {
        self.ctx.clone()
    }

    pub fn signed_in(&self) -> bool {
        self.client.is_some()
    }

    /// Start a request; when it finishes the result is mapped to a `Msg` and the UI repaints.
    fn task<T: Send + 'static>(
        &self,
        ctx: &egui::Context,
        start: impl FnOnce(Box<dyn FnOnce(Result<T, SyncError>) + Send>),
        wrap: impl FnOnce(Result<T, String>) -> Msg + Send + 'static,
    ) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        start(Box::new(move |r| {
            let _ = tx.send(wrap(r.map_err(|e| e.to_string())));
            ctx.request_repaint();
        }));
    }

    fn start_session(&mut self, map: Option<(String, String)>, rev: u64, state: SyncState) {
        let Some(c) = self.client.clone() else { return };
        let ctx = self.ctx.clone();
        let wake = move || {
            if let Some(c) = &ctx {
                c.request_repaint();
            }
        };
        self.session = Some(Session {
            handle: smf_sync::spawn(c, map.as_ref().map(|m| m.0.clone()), rev, 0, wake),
            map,
            state,
            last_diff: Instant::now(),
            reloading: false,
        });
        self.teammates.clear();
        self.sharing_sent = false;
    }

    fn sign_out(&mut self) {
        if self.saved.share_location
            && let Some(s) = &self.session
        {
            s.handle.send(Command::ClearPosition);
            // Start the request now; it completes even though the session is dropped below.
            s.handle.tick();
        }
        self.session = None;
        self.client = None;
        self.me = None;
        self.maps.clear();
        self.teammates.clear();
        self.saved = Saved {
            server: self.saved.server.clone(),
            display_name: self.saved.display_name.clone(),
            ..Saved::default()
        };
        self.save();
    }
}

fn age(secs: u64) -> String {
    crate::live_ui::age_text(secs)
}

impl MapPanel {
    pub(crate) fn team_menu(&mut self, ui: &mut egui::Ui, editor: &mut Editor) {
        let ctx = ui.ctx().clone();
        let gps_on = self.gps.connected();
        if !self.team.signed_in() {
            self.team_signin_ui(ui, &ctx);
            return;
        }
        let t = &mut self.team;
        let team_name =
            t.me.as_ref()
                .map_or(t.saved.team_name.clone(), |m| m.team_name.clone());
        ui.label(egui::RichText::new(format!("Team: {team_name}")).strong());
        ui.weak(format!("Signed in as {}", t.saved.display_name));
        if let Some(p) = t.me.as_ref().and_then(|m| m.plan.as_ref()) {
            let line = format!(
                "Plan: {} ({}). Members {}/{}, maps {}/{}",
                p.plan,
                p.status,
                p.usage.members,
                p.limits.max_members,
                p.usage.maps,
                p.limits.max_maps
            );
            if p.status == "active" {
                ui.weak(line);
            } else {
                // A lapsed plan makes the team read-only; say so where people will see it.
                ui.colored_label(Color32::from_rgb(230, 160, 60), line);
                ui.colored_label(Color32::from_rgb(230, 160, 60), &p.message);
            }
        }
        if let Some(me) = &t.me {
            ui.collapsing(format!("Members ({})", me.members.len()), |ui| {
                for m in &me.members {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{}{}",
                            m.display_name,
                            if m.is_owner { " (owner)" } else { "" }
                        ));
                        if me.is_owner
                            && m.id != me.member_id
                            && ui.small_button("Remove").clicked()
                        {
                            let (c, id) = (t.client.clone(), m.id.clone());
                            if let Some(c) = c {
                                t.task(&ctx, move |d| c.remove_member(&id, d), Msg::Done);
                            }
                        }
                    });
                }
            });
            if me.is_owner {
                if ui.button("New join code").clicked()
                    && let Some(c) = t.client.clone()
                {
                    t.task(&ctx, move |d| c.new_join_code(d), Msg::JoinCode);
                }
                if let Some(code) = &t.new_code {
                    ui.horizontal(|ui| {
                        ui.label("Share this code:");
                        ui.monospace(code);
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(code.clone());
                        }
                    });
                }
            }
        }
        ui.separator();

        // Current shared map
        let syncing = t.session.as_ref().and_then(|s| s.map.clone());
        match &syncing {
            Some((_, name)) => {
                ui.label(format!("Live map: {name}"));
                if ui.button("Stop syncing this map").clicked() {
                    t.start_session(None, 0, SyncState::default());
                    t.saved.last_map_id.clear();
                    t.save();
                }
            }
            None => {
                ui.weak("Not syncing a map.");
            }
        }
        if ui.button("Share this map with the team").clicked() {
            let name = editor.project.name.clone();
            let mut tmp = SyncState::default();
            let snap = smf_sync::snapshot(&editor.project, &mut tmp);
            let snap_for_call = snap.clone();
            if let Some(c) = t.client.clone() {
                t.status = Some("Sharing...".into());
                t.task(
                    &ctx,
                    move |d| c.create_map(&name, &snap_for_call, d),
                    move |r| Msg::Shared(r.map(|i| (i, snap))),
                );
                // `tmp` carried the uids; keep it so the content matches what was uploaded.
                t.pending_share = Some(tmp);
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Team maps");
            if ui.small_button("Refresh").clicked()
                && let Some(c) = t.client.clone()
            {
                t.task(&ctx, move |d| c.list_maps(d), Msg::Maps);
            }
        });
        if t.maps.is_empty() {
            ui.weak("No shared maps yet (press Refresh).");
        }
        let local_has_features = !editor.project.features.is_empty();
        for m in t.maps.clone() {
            ui.horizontal(|ui| {
                ui.label(&m.name);
                let confirming = t.confirm_replace.as_deref() == Some(&m.id);
                let label = if confirming { "Replace my map" } else { "Open" };
                if ui.small_button(label).clicked() {
                    if local_has_features && !confirming {
                        // Opening a shared map swaps out the features on screen: ask first.
                        t.confirm_replace = Some(m.id.clone());
                    } else {
                        t.confirm_replace = None;
                        if let Some(c) = t.client.clone() {
                            let id = m.id.clone();
                            t.task(&ctx, move |d| c.get_map(&id, d), Msg::Opened);
                        }
                    }
                }
            });
        }
        if t.confirm_replace.is_some() {
            ui.colored_label(
                Color32::from_rgb(230, 160, 60),
                "Opening replaces the features in your current map. Click again to confirm.",
            );
        }
        ui.separator();
        if ui
            .checkbox(
                &mut t.saved.share_location,
                "Share my GPS position with the team",
            )
            .changed()
        {
            t.save();
            if !t.saved.share_location
                && let Some(s) = &t.session
            {
                s.handle.send(Command::ClearPosition);
                t.sharing_sent = false;
            }
        }
        if t.saved.share_location && !gps_on {
            ui.weak("Connect a GPS (GPS menu) to share where you are.");
        }
        if let Some(s) = &t.status {
            ui.separator();
            ui.label(s);
        }
        ui.separator();
        if ui.button("Sign out").clicked() {
            t.sign_out();
            ui.close();
        }
    }

    fn team_signin_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = &mut self.team;
        ui.label("Join a team to share maps and positions.");
        ui.horizontal(|ui| {
            ui.label("Server");
            ui.add(egui::TextEdit::singleline(&mut t.form.server).desired_width(240.0));
        });
        ui.horizontal(|ui| {
            ui.label("Your name");
            ui.add(egui::TextEdit::singleline(&mut t.form.display).desired_width(160.0));
        });
        if !t.form.creating {
            ui.horizontal(|ui| {
                ui.label("Join code");
                ui.add(egui::TextEdit::singleline(&mut t.form.code).desired_width(140.0));
                if ui.button("Join").clicked() {
                    let (server, code, name) = (
                        t.form.server.trim().to_string(),
                        t.form.code.clone(),
                        t.form.display.trim().to_string(),
                    );
                    t.begin_join(ctx, server, code, name);
                }
            });
            if ui.small_button("Create a new team instead...").clicked() {
                t.form.creating = true;
            }
        } else {
            ui.horizontal(|ui| {
                ui.label("Team name");
                ui.add(egui::TextEdit::singleline(&mut t.form.team_name).desired_width(160.0));
            });
            ui.horizontal(|ui| {
                ui.label("Show Me Fire account token");
                ui.add(
                    egui::TextEdit::singleline(&mut t.form.create_key)
                        .password(true)
                        .desired_width(140.0),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Create").clicked() {
                    let (server, key, team, name) = (
                        t.form.server.trim().to_string(),
                        t.form.create_key.clone(),
                        t.form.team_name.trim().to_string(),
                        t.form.display.trim().to_string(),
                    );
                    t.begin_create(ctx, server, key, team, name);
                }
                if ui.button("Back").clicked() {
                    t.form.creating = false;
                }
            });
            ui.weak("Creating a team needs a Show Me Fire account on a paid plan. Teammates only need the join code. The token is not saved.");
        }
        if let Some(s) = &t.status {
            ui.label(s);
        }
        ui.weak("Your sign-in is saved unencrypted in this app's settings folder.");
    }
}

impl TeamState {
    fn begin_join(&mut self, ctx: &egui::Context, server: String, code: String, name: String) {
        if name.is_empty() || code.trim().is_empty() {
            self.status = Some("Enter your name and the join code.".into());
            return;
        }
        self.status = Some("Joining...".into());
        let (s2, shown_name) = (server.clone(), name.clone());
        match FieldClient::new(&server, None) {
            Ok(c) => self.task(
                ctx,
                move |d| c.join(&code, &name, d),
                move |r| Msg::SignedIn(r, s2, shown_name),
            ),
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    fn begin_create(
        &mut self,
        ctx: &egui::Context,
        server: String,
        key: String,
        team: String,
        name: String,
    ) {
        if name.is_empty() || team.is_empty() || key.is_empty() {
            self.status = Some("Enter a team name, your name and your account token.".into());
            return;
        }
        self.status = Some("Creating...".into());
        let s2 = server.clone();
        let name2 = name.clone();
        match FieldClient::new(&server, None) {
            Ok(c) => self.task(
                ctx,
                move |d| c.create_team(&key, &team, &name, d),
                move |r| Msg::SignedIn(r, s2, name2),
            ),
            Err(e) => self.status = Some(e.to_string()),
        }
    }
}

impl MapPanel {
    /// Called every frame: handles finished background tasks, syncs local edits, applies
    /// teammates' changes and shares our position.
    pub(crate) fn team_tick(&mut self, ctx: &egui::Context, editor: &mut Editor) {
        self.team.ctx = Some(ctx.clone());
        if let Some(s) = &self.team.session {
            s.handle.tick();
        }
        if let Some((server, code, name, map)) = self.team.dev.take() {
            self.team.dev_open_map = map;
            self.team.begin_join(ctx, server, code, name);
        }
        while let Ok(m) = self.team.rx.try_recv() {
            self.team_on_msg(ctx, editor, m);
        }
        let Some(sess) = &mut self.team.session else {
            return;
        };
        let me_id = self.team.saved.member_id.clone();

        // 1. Our edits out.
        if sess.map.is_some() && !sess.reloading && sess.last_diff.elapsed() >= DIFF_EVERY {
            sess.last_diff = Instant::now();
            let ops = smf_sync::diff(&editor.project, &mut sess.state);
            if !ops.is_empty() {
                sess.handle.send(Command::Push(ops));
            }
        }

        // 2. News in.
        let mut reload = false;
        let mut logged_out: Option<String> = None;
        while let Ok(u) = sess.handle.rx.try_recv() {
            match u {
                Update::Events(ev) => {
                    if ev.reset {
                        reload = true;
                    } else if !sess.reloading {
                        let n = smf_sync::apply_remote(
                            &mut editor.project,
                            &mut sess.state,
                            &ev.ops,
                            &me_id,
                        );
                        if n > 0 {
                            editor.revision += 1;
                            if editor
                                .selection
                                .is_some_and(|id| editor.project.feature(id).is_none())
                            {
                                editor.selection = None;
                            }
                        }
                    }
                    for p in ev.positions {
                        if p.member_id == me_id {
                            continue;
                        }
                        if p.cleared {
                            self.team.teammates.remove(&p.member_id);
                        } else {
                            self.team.teammates.insert(p.member_id.clone(), p);
                        }
                    }
                }
                Update::Pushed { uids, .. } => sess.state.ack(&uids, true),
                Update::PushFailed { uids, error } => {
                    sess.state.ack(&uids, false);
                    self.team.status = Some(format!("Could not send changes: {error}"));
                }
                Update::Status(s) => self.team.status = Some(s),
                Update::LoggedOut(m) => logged_out = Some(m),
            }
        }
        if let Some(m) = logged_out {
            self.team.sign_out();
            self.team.status = Some(format!("Signed out: {m}"));
            return;
        }
        if reload && let Some((id, _)) = sess.map.clone() {
            sess.reloading = true;
            if let Some(c) = self.team.client.clone() {
                self.team.task(ctx, move |d| c.get_map(&id, d), Msg::Opened);
            }
        }

        // 3. Our position out.
        let share = self.team.saved.share_location;
        if share
            && self
                .team
                .last_pos_sent
                .is_none_or(|t| t.elapsed() >= POSITION_EVERY)
            && let Some((fix, at)) = &self.gps.fix
            && at.elapsed() <= FIX_MAX_AGE
            && let Some(s) = &self.team.session
        {
            s.handle.send(Command::Position(Position {
                lat: fix.lat,
                lng: fix.lng,
                accuracy: fix.accuracy_m,
                speed: fix.speed_mps,
                course: fix.course_deg,
                alt: fix.alt_m,
            }));
            self.team.last_pos_sent = Some(Instant::now());
            self.team.sharing_sent = true;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn team_on_msg(&mut self, ctx: &egui::Context, editor: &mut Editor, m: Msg) {
        let t = &mut self.team;
        match m {
            Msg::SignedIn(Ok(c), server, display) => {
                t.saved = Saved {
                    server: server.clone(),
                    token: c.token.clone(),
                    member_id: c.member_id.clone(),
                    team_name: c.team_name.clone(),
                    display_name: display,
                    ..Saved::default()
                };
                t.save();
                t.client = FieldClient::new(&server, Some(c.token.clone())).ok();
                t.new_code = c.join_code.clone();
                t.status = Some(match &c.join_code {
                    Some(code) => format!("Team created. Join code: {code}"),
                    None => "Joined.".into(),
                });
                t.form.creating = false;
                t.form.create_key.clear();
                t.start_session(None, 0, SyncState::default());
                if let Some(cl) = t.client.clone() {
                    let cl2 = cl.clone();
                    t.task(ctx, move |d| cl.me(d), Msg::Me);
                    t.task(ctx, move |d| cl2.list_maps(d), Msg::Maps);
                }
            }
            Msg::SignedIn(Err(e), ..) => t.status = Some(e),
            Msg::Me(Ok(me)) => {
                t.saved.team_name = me.team_name.clone();
                t.saved.member_id = me.member_id.clone();
                t.me = Some(me);
                if t.session.is_none() {
                    t.start_session(None, 0, SyncState::default());
                }
                if let Some(c) = t.client.clone() {
                    t.task(ctx, move |d| c.list_maps(d), Msg::Maps);
                }
            }
            Msg::Me(Err(e)) => {
                // A rejected token means the sign-in is gone; a network error just means offline.
                if e.contains("token") {
                    t.sign_out();
                }
                t.status = Some(e);
            }
            Msg::Maps(Ok(maps)) => {
                if let Some(want) = t.dev_open_map.take()
                    && let Some(m) = maps.iter().find(|m| m.name == want)
                    && let Some(c) = t.client.clone()
                {
                    let id = m.id.clone();
                    t.task(ctx, move |d| c.get_map(&id, d), Msg::Opened);
                }
                t.maps = maps;
            }
            Msg::Maps(Err(e)) => t.status = Some(e),
            Msg::Opened(Ok(doc)) => {
                let reopening = t.session.as_ref().is_some_and(|s| s.reloading);
                let mut state = SyncState::default();
                let n = smf_sync::load_full(&mut editor.project, &mut state, &doc);
                editor.project.name = doc.name.clone();
                editor.revision += 1;
                editor.selection = None;
                let rev = doc.rev;
                if reopening && let Some(s) = &mut t.session {
                    // Same session, fresh copy: continue from the reloaded revision.
                    s.state = state;
                    s.reloading = false;
                    s.handle.resume_from(rev);
                } else {
                    t.start_session(Some((doc.id.clone(), doc.name.clone())), rev, state);
                }
                t.saved.last_map_id = doc.id;
                t.saved.last_map_name = doc.name.clone();
                t.save();
                t.status = Some(format!("Opened \"{}\" ({n} features)", doc.name));
                if let Some(b) = editor.project.bounds() {
                    self.pending_fit = Some(b);
                }
            }
            Msg::Opened(Err(e)) => {
                if let Some(s) = &mut t.session {
                    s.reloading = false;
                }
                t.status = Some(e);
            }
            Msg::Shared(Ok((info, snap))) => {
                // The uids generated for the snapshot are the ones this machine now uses.
                let mut state = t.pending_share.take().unwrap_or_default();
                smf_sync::ack_snapshot(&mut state, &snap);
                t.start_session(Some((info.id.clone(), info.name.clone())), info.rev, state);
                t.saved.last_map_id = info.id;
                t.saved.last_map_name = info.name.clone();
                t.save();
                t.status = Some(format!(
                    "Sharing \"{}\". Teammates can open it from the Team menu.",
                    info.name
                ));
                if let Some(c) = t.client.clone() {
                    t.task(ctx, move |d| c.list_maps(d), Msg::Maps);
                }
            }
            Msg::Shared(Err(e)) => {
                t.pending_share = None;
                t.status = Some(e);
            }
            Msg::JoinCode(Ok(code)) => t.new_code = Some(code),
            Msg::JoinCode(Err(e)) | Msg::Done(Err(e)) => t.status = Some(e),
            Msg::Done(Ok(())) => {
                if let Some(c) = t.client.clone() {
                    t.task(ctx, move |d| c.me(d), Msg::Me);
                }
            }
        }
    }

    pub(crate) fn draw_team(&self, painter: &egui::Painter, pj: &Projector, rect: egui::Rect) {
        let now = smf_data::unix_now();
        let visible = rect.expand(80.0);
        for p in self.team.teammates.values() {
            let Some(c) = pj
                .pos(LngLat::new(p.lng, p.lat))
                .filter(|c| visible.contains(*c))
            else {
                continue;
            };
            let r = 8.0;
            painter.circle_filled(c, r + 2.0, Color32::WHITE);
            painter.circle_filled(c, r, TEAM_COLOR);
            let secs = now.saturating_sub(p.ts);
            overlay::label(
                painter,
                c + egui::vec2(0.0, 13.0),
                Align2::CENTER_TOP,
                &format!("{} · {}", p.display_name, age(secs)),
                12.0,
                Color32::BLACK,
            );
        }
    }

    pub(crate) fn team_pick(
        team: &TeamState,
        proj: &dyn Fn(LngLat) -> [f64; 2],
        pt: [f64; 2],
        tol: f64,
    ) -> Option<String> {
        let now = smf_data::unix_now();
        let best = team
            .teammates
            .values()
            .map(|p| {
                let s = proj(LngLat::new(p.lng, p.lat));
                ((s[0] - pt[0]).hypot(s[1] - pt[1]), p)
            })
            .filter(|(d, _)| *d <= tol * 1.6)
            .min_by(|a, b| a.0.total_cmp(&b.0))?
            .1;
        let mut t = format!(
            "{} (teammate): {:.5}, {:.5}, {}",
            best.display_name,
            best.lat,
            best.lng,
            age(now.saturating_sub(best.ts))
        );
        if let Some(s) = best.speed.filter(|s| *s > 0.3) {
            t.push_str(&format!(", {:.1} km/h", s * 3.6));
        }
        Some(t)
    }
}
