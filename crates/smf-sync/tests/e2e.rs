//! End-to-end check of two clients against a real Show Me Fire Field server.
//!
//! Ignored by default. To run it, start the server (see the API repo's
//! `docs/field_collab.md`) with `FIELD_TEAM_CREATE_KEY` set, then:
//!
//!   SMF_FIELD_URL=http://127.0.0.1:8099 SMF_FIELD_KEY=<key> \
//!     cargo test -p smf-sync --test e2e -- --ignored --nocapture

use smf_features::{Feature, Geometry, Project, Style};
use smf_geo::LngLat;
use smf_sync::{
    Command, FieldClient, Handle, Position, SyncState, Update, ack_snapshot, apply_remote, diff,
    load_full, snapshot, spawn, wait_blocking,
};
use std::time::{Duration, Instant};

fn feat(id: u64, name: &str, lng: f64) -> Feature {
    Feature {
        id,
        name: name.into(),
        geometry: Geometry::Marker(LngLat::new(lng, 38.5)),
        style: Style::default(),
        folder: None,
        visible: true,
        notes: String::new(),
    }
}

struct Peer {
    me: String,
    project: Project,
    state: SyncState,
    handle: Handle,
    positions: Vec<smf_sync::TeamPosition>,
    logged_out: Option<String>,
    /// Number of push batches the server has acknowledged.
    pushed: usize,
}

impl Peer {
    /// Send local changes, then process whatever the server has pushed to us, for up to `secs`
    /// or until `done` is true.
    fn pump(&mut self, secs: u64, done: impl Fn(&Peer) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end && !done(self) {
            self.handle.tick();
            let ops = diff(&self.project, &mut self.state);
            if !ops.is_empty() {
                self.handle.send(Command::Push(ops));
            }
            while let Ok(u) = self.handle.rx.recv_timeout(Duration::from_millis(100)) {
                match u {
                    Update::Events(ev) => {
                        assert!(!ev.reset, "unexpected reset");
                        apply_remote(&mut self.project, &mut self.state, &ev.ops, &self.me);
                        for p in ev.positions {
                            self.positions.retain(|x| x.member_id != p.member_id);
                            if !p.cleared {
                                self.positions.push(p);
                            }
                        }
                    }
                    Update::Pushed { uids, .. } => {
                        self.state.ack(&uids, true);
                        self.pushed += 1;
                    }
                    Update::PushFailed { uids, error } => {
                        self.state.ack(&uids, false);
                        panic!("push failed: {error}");
                    }
                    Update::Status(s) => println!("  status: {s}"),
                    Update::LoggedOut(m) => self.logged_out = Some(m),
                }
            }
        }
    }

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self
            .project
            .features
            .iter()
            .map(|f| f.name.clone())
            .collect();
        n.sort();
        n
    }
}

#[test]
#[ignore]
fn two_clients_converge_through_a_real_server() {
    let (Ok(url), Ok(key)) = (
        std::env::var("SMF_FIELD_URL"),
        std::env::var("SMF_FIELD_KEY"),
    ) else {
        panic!("set SMF_FIELD_URL and SMF_FIELD_KEY to run this test");
    };
    let anon = FieldClient::new(&url, None).unwrap();

    // Alice creates the team; Bob joins with the code.
    let alice_creds = wait_blocking(|d| anon.create_team(&key, "E2E crew", "Alice", d)).unwrap();
    let code = alice_creds.join_code.clone().unwrap();
    let bob_creds = wait_blocking(|d| anon.join(&code, "Bob", d)).unwrap();
    let alice_client = FieldClient::new(&url, Some(alice_creds.token.clone())).unwrap();
    let bob_client = FieldClient::new(&url, Some(bob_creds.token.clone())).unwrap();
    let me = wait_blocking(|d| alice_client.me(d)).unwrap();
    assert_eq!(
        (me.team_name.as_str(), me.members.len(), me.is_owner),
        ("E2E crew", 2, true)
    );

    // Alice shares her map (two features).
    let mut a_state = SyncState::default();
    let a_project = Project {
        next_id: 3,
        features: vec![feat(1, "Camp", -92.10), feat(2, "Trailhead", -92.11)],
        ..Project::default()
    };
    let snap = snapshot(&a_project, &mut a_state);
    let info = wait_blocking(|d| alice_client.create_map("Search area", &snap, d)).unwrap();
    ack_snapshot(&mut a_state, &snap);
    assert_eq!(info.rev, 2);

    // Bob opens it.
    let doc = wait_blocking(|d| bob_client.get_map(&info.id, d)).unwrap();
    let mut b_state = SyncState::default();
    let mut b_project = Project::default();
    assert_eq!(load_full(&mut b_project, &mut b_state, &doc), 2);

    let mut alice = Peer {
        me: alice_creds.member_id.clone(),
        project: a_project,
        state: a_state,
        handle: spawn(
            alice_client.clone(),
            Some(info.id.clone()),
            info.rev,
            0,
            || {},
        ),
        positions: vec![],
        logged_out: None,
        pushed: 0,
    };
    let mut bob = Peer {
        me: bob_creds.member_id.clone(),
        project: b_project,
        state: b_state,
        handle: spawn(bob_client.clone(), Some(info.id.clone()), doc.rev, 0, || {}),
        positions: vec![],
        logged_out: None,
        pushed: 0,
    };
    assert_eq!(bob.names(), vec!["Camp", "Trailhead"]);

    // Alice edits one feature and adds another; Bob sees both.
    alice.project.features[0].name = "Base camp".into();
    let hazard_id = alice.project.alloc_id();
    alice
        .project
        .features
        .push(feat(hazard_id, "Hazard", -92.12));
    alice.pump(5, |p| p.pushed >= 1 && p.state.in_flight_count() == 0);
    bob.pump(15, |p| {
        p.names() == vec!["Base camp", "Hazard", "Trailhead"]
    });
    assert_eq!(
        bob.names(),
        vec!["Base camp", "Hazard", "Trailhead"],
        "Bob should see Alice's edits"
    );

    // Bob deletes one and adds one; Alice sees it.
    bob.project.features.retain(|f| f.name != "Trailhead");
    let id = bob.project.alloc_id();
    bob.project.features.push(feat(id, "Water", -92.2));
    bob.pump(5, |p| p.pushed >= 1 && p.state.in_flight_count() == 0);
    alice.pump(15, |p| p.names() == vec!["Base camp", "Hazard", "Water"]);
    assert_eq!(
        alice.names(),
        vec!["Base camp", "Hazard", "Water"],
        "Alice should see Bob's changes"
    );
    assert_eq!(alice.names(), bob.names());

    // Neither side thinks it has anything left to send: no echo loops.
    alice.pump(2, |_| false);
    bob.pump(2, |_| false);
    let left = diff(&alice.project, &mut alice.state);
    assert!(left.is_empty(), "Alice still has to send: {left:?}");
    let left = diff(&bob.project, &mut bob.state);
    assert!(left.is_empty(), "Bob still has to send: {left:?}");

    // The server's copy matches too.
    let server = wait_blocking(|d| alice_client.get_map(&info.id, d)).unwrap();
    let mut on_server: Vec<String> = server
        .features
        .values()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect();
    on_server.sort();
    assert_eq!(on_server, alice.names());

    // Bob shares his position; Alice sees it, then it disappears when he stops.
    bob.handle.send(Command::Position(Position {
        lat: 38.5,
        lng: -92.1,
        speed: Some(1.4),
        ..Position::default()
    }));
    alice.pump(15, |p| !p.positions.is_empty());
    assert_eq!(alice.positions.len(), 1);
    assert_eq!(alice.positions[0].display_name, "Bob");
    assert_eq!(alice.positions[0].speed, Some(1.4));
    bob.handle.send(Command::ClearPosition);
    alice.pump(15, |p| p.positions.is_empty());
    assert!(alice.positions.is_empty(), "Bob stopped sharing");

    // Alice removes Bob: his credentials stop working and his worker reports it.
    wait_blocking(|d| alice_client.remove_member(&bob_creds.member_id, d)).unwrap();
    bob.pump(30, |p| p.logged_out.is_some());
    assert!(bob.logged_out.is_some(), "Bob should be logged out");
    assert!(wait_blocking(|d| bob_client.me(d)).is_err());
}
