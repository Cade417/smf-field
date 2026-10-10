//! The sync engine: turns local edits into operations and applies teammates' operations.
//!
//! Local feature ids are small integers private to each person's project, so shared
//! features get a global `uid`. `SyncState` remembers which local feature has which uid and
//! the exact content the server last confirmed for each, so `diff` only reports real changes
//! and `apply_remote` never echoes a change back.
//!
//! Ordering rule: the server numbers every operation, and the highest number wins per
//! feature. While one of our own operations is in flight for a feature, remote changes to
//! that same feature are ignored, because ours will be ordered after them and win anyway;
//! applying theirs first would leave us showing a version the server no longer holds.

use crate::client::{MapDoc, RemoteOp};
use serde_json::{Value, json};
use smf_features::{Feature, Id, Project};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub enum LocalOp {
    Upsert { uid: String, feature: Value },
    Delete { uid: String },
}

impl LocalOp {
    pub fn uid(&self) -> &str {
        match self {
            LocalOp::Upsert { uid, .. } | LocalOp::Delete { uid } => uid,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            LocalOp::Upsert { uid, feature } => {
                json!({"op": "upsert", "uid": uid, "feature": feature})
            }
            LocalOp::Delete { uid } => json!({"op": "delete", "uid": uid}),
        }
    }
}

/// A fresh feature id: unique enough that two people never collide (not a secret).
pub fn new_uid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // RandomState is seeded from the OS, so two processes started in the same nanosecond still differ.
    let mut a = std::collections::hash_map::RandomState::new().build_hasher();
    a.write_u128(nanos);
    a.write_u64(n);
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    b.write_u64(n);
    b.write_u128(nanos);
    format!(
        "f{:016x}{:015x}",
        a.finish(),
        b.finish() & 0x0fff_ffff_ffff_ffff
    )
}

/// What the server knows, in terms of this machine's feature ids.
#[derive(Default, Debug)]
pub struct SyncState {
    uid_of: HashMap<Id, String>,
    id_of: HashMap<String, Id>,
    /// uid -> canonical content the server last confirmed.
    synced: HashMap<String, String>,
    /// uid -> content (None for a delete) of operations sent but not yet confirmed.
    in_flight: HashMap<String, Option<String>>,
}

/// The feature as shared: everything but the local id and the local folder.
fn shared_json(f: &Feature) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("id");
        o.remove("folder");
    }
    v
}

fn canon(v: &Value) -> String {
    // serde_json keeps object keys sorted, so equal content gives equal text.
    v.to_string()
}

fn parse_feature(v: &Value) -> Option<Feature> {
    let mut v = v.clone();
    let o = v.as_object_mut()?;
    o.insert("id".into(), json!(0));
    o.insert("folder".into(), Value::Null);
    serde_json::from_value(v).ok()
}

impl SyncState {
    pub fn reset(&mut self) {
        *self = SyncState::default();
    }

    fn uid_for(&mut self, id: Id) -> String {
        if let Some(u) = self.uid_of.get(&id) {
            return u.clone();
        }
        let u = new_uid();
        self.link(id, &u);
        u
    }

    fn link(&mut self, id: Id, uid: &str) {
        self.uid_of.insert(id, uid.to_string());
        self.id_of.insert(uid.to_string(), id);
    }

    fn unlink_uid(&mut self, uid: &str) {
        if let Some(id) = self.id_of.remove(uid) {
            self.uid_of.remove(&id);
        }
        self.synced.remove(uid);
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn synced_count(&self) -> usize {
        self.synced.len()
    }

    /// Record the server's answer for the operations returned by the last `diff`.
    pub fn ack(&mut self, uids: &[String], ok: bool) {
        for u in uids {
            let Some(sent) = self.in_flight.remove(u) else {
                continue;
            };
            if !ok {
                // Not accepted: forget it was sent, so the next `diff` reports it again.
                continue;
            }
            match sent {
                Some(c) => {
                    self.synced.insert(u.clone(), c);
                }
                None => self.unlink_uid(u),
            }
        }
    }
}

/// Operations that bring the server in line with the project. Ops returned here are marked
/// in flight until `SyncState::ack` is called for their uids.
pub fn diff(project: &Project, st: &mut SyncState) -> Vec<LocalOp> {
    let mut ops = vec![];
    let mut present: HashSet<String> = HashSet::new();
    for f in &project.features {
        let uid = st.uid_for(f.id);
        present.insert(uid.clone());
        if st.in_flight.contains_key(&uid) {
            continue;
        }
        let v = shared_json(f);
        let c = canon(&v);
        if st.synced.get(&uid) != Some(&c) {
            st.in_flight.insert(uid.clone(), Some(c));
            ops.push(LocalOp::Upsert { uid, feature: v });
        }
    }
    let gone: Vec<String> = st
        .synced
        .keys()
        .filter(|u| !present.contains(*u) && !st.in_flight.contains_key(*u))
        .cloned()
        .collect();
    for uid in gone {
        st.in_flight.insert(uid.clone(), None);
        ops.push(LocalOp::Delete { uid });
    }
    ops
}

/// Apply teammates' operations to the project. Returns how many features changed.
/// Operations authored by `me` are skipped (they are our own, already applied).
pub fn apply_remote(
    project: &mut Project,
    st: &mut SyncState,
    ops: &[RemoteOp],
    me: &str,
) -> usize {
    let mut changed = 0;
    for op in ops {
        if op.by == me || st.in_flight.contains_key(&op.uid) {
            continue;
        }
        match op.op.as_str() {
            "upsert" => {
                let Some(mut f) = op.feature.as_ref().and_then(parse_feature) else {
                    continue;
                };
                let c = canon(&shared_json(&f));
                match st.id_of.get(&op.uid).copied() {
                    Some(id) => {
                        if let Some(i) = project.index_of(id) {
                            f.id = id;
                            f.folder = project.features[i].folder;
                            if project.features[i] != f {
                                project.features[i] = f;
                                changed += 1;
                            }
                        } else {
                            // The local copy was deleted but not yet reported; keep it deleted.
                            continue;
                        }
                    }
                    None => {
                        let id = project.alloc_id();
                        f.id = id;
                        project.features.push(f);
                        st.link(id, &op.uid);
                        changed += 1;
                    }
                }
                st.synced.insert(op.uid.clone(), c);
            }
            "delete" => {
                if let Some(id) = st.id_of.get(&op.uid).copied()
                    && let Some(i) = project.index_of(id)
                {
                    project.features.remove(i);
                    changed += 1;
                }
                st.unlink_uid(&op.uid);
            }
            _ => {}
        }
    }
    changed
}

/// Replace the project's features with a shared map's content (opening a shared map, or
/// recovering after falling too far behind). Folders are not shared and are kept.
pub fn load_full(project: &mut Project, st: &mut SyncState, doc: &MapDoc) -> usize {
    st.reset();
    project.features.clear();
    let mut n = 0;
    for (uid, v) in &doc.features {
        let Some(mut f) = parse_feature(v) else {
            continue;
        };
        let id = project.alloc_id();
        f.id = id;
        let c = canon(&shared_json(&f));
        project.features.push(f);
        st.link(id, uid);
        st.synced.insert(uid.clone(), c);
        n += 1;
    }
    n
}

/// Features of the project as `uid -> JSON`, for creating a shared map from the current one.
/// Marks them as already on the server once `ack_all_synced` is called.
pub fn snapshot(
    project: &Project,
    st: &mut SyncState,
) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    for f in &project.features {
        let uid = st.uid_for(f.id);
        out.insert(uid, shared_json(f));
    }
    out
}

/// After the server accepted `snapshot` as the map's initial content.
pub fn ack_snapshot(st: &mut SyncState, snap: &std::collections::BTreeMap<String, Value>) {
    st.in_flight.clear();
    for (uid, v) in snap {
        st.synced.insert(uid.clone(), canon(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RemoteOp;
    use smf_features::{Geometry, Style};
    use smf_geo::LngLat;

    fn feat(id: Id, name: &str, lng: f64) -> Feature {
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

    fn project(fs: Vec<Feature>) -> Project {
        Project {
            next_id: fs.iter().map(|f| f.id).max().unwrap_or(0) + 1,
            features: fs,
            ..Project::default()
        }
    }

    fn remote(rev: u64, uid: &str, op: &str, f: Option<&Feature>, by: &str) -> RemoteOp {
        RemoteOp {
            rev,
            uid: uid.into(),
            op: op.into(),
            feature: f.map(shared_json),
            by: by.into(),
        }
    }

    fn uids(ops: &[LocalOp]) -> Vec<String> {
        ops.iter().map(|o| o.uid().to_string()).collect()
    }

    #[test]
    fn uids_are_unique_and_valid_for_the_server() {
        let mut seen = HashSet::new();
        for _ in 0..5000 {
            let u = new_uid();
            assert!((8..=64).contains(&u.len()), "{u}");
            assert!(
                u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            );
            assert!(seen.insert(u));
        }
    }

    #[test]
    fn shared_content_has_no_local_ids() {
        let mut f = feat(7, "Camp", -92.0);
        f.folder = Some(3);
        let v = shared_json(&f);
        assert!(v.get("id").is_none() && v.get("folder").is_none());
        assert_eq!(v["name"], "Camp");
        // And it round-trips into a feature with the same content.
        let back = parse_feature(&v).unwrap();
        assert_eq!((back.name.as_str(), &back.geometry), ("Camp", &f.geometry));
    }

    #[test]
    fn diff_reports_adds_edits_and_deletes_once() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "A", -92.0), feat(2, "B", -92.1)]);
        let ops = diff(&p, &mut st);
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().all(|o| matches!(o, LocalOp::Upsert { .. })));
        // Nothing is re-sent while those are in flight, even if asked again.
        assert!(diff(&p, &mut st).is_empty());
        st.ack(&uids(&ops), true);
        assert!(
            diff(&p, &mut st).is_empty(),
            "unchanged content is not an edit"
        );
        assert_eq!(st.synced_count(), 2);

        p.features[0].name = "A2".into();
        let ops = diff(&p, &mut st);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], LocalOp::Upsert { feature, .. } if feature["name"] == "A2"));
        st.ack(&uids(&ops), true);

        let removed = p.features.remove(1);
        let ops = diff(&p, &mut st);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], LocalOp::Delete { .. }));
        st.ack(&uids(&ops), true);
        assert!(diff(&p, &mut st).is_empty());
        assert_eq!(st.synced_count(), 1);
        let _ = removed;
    }

    #[test]
    fn a_refused_push_is_retried_by_the_next_diff() {
        let mut st = SyncState::default();
        let p = project(vec![feat(1, "A", -92.0)]);
        let ops = diff(&p, &mut st);
        st.ack(&uids(&ops), false);
        assert_eq!(st.in_flight_count(), 0);
        assert_eq!(diff(&p, &mut st).len(), 1, "same change is offered again");
    }

    #[test]
    fn remote_changes_apply_without_echo() {
        let mut st = SyncState::default();
        let mut p = project(vec![]);
        let theirs = feat(0, "Hazard", -92.3);
        let n = apply_remote(
            &mut p,
            &mut st,
            &[remote(1, "uid-remote-0001", "upsert", Some(&theirs), "bob")],
            "me",
        );
        assert_eq!(n, 1);
        assert_eq!(p.features.len(), 1);
        assert_eq!(p.features[0].name, "Hazard");
        // What we just received must not look like a local edit.
        assert!(diff(&p, &mut st).is_empty());
        // Their edit replaces the feature but keeps our local id.
        let id = p.features[0].id;
        let edited = feat(0, "Hazard (cleared)", -92.3);
        apply_remote(
            &mut p,
            &mut st,
            &[remote(2, "uid-remote-0001", "upsert", Some(&edited), "bob")],
            "me",
        );
        assert_eq!(
            (
                p.features.len(),
                p.features[0].id,
                p.features[0].name.as_str()
            ),
            (1, id, "Hazard (cleared)")
        );
        assert!(diff(&p, &mut st).is_empty());
        // Their delete removes it, and that isn't echoed back either.
        apply_remote(
            &mut p,
            &mut st,
            &[remote(3, "uid-remote-0001", "delete", None, "bob")],
            "me",
        );
        assert!(p.features.is_empty());
        assert!(diff(&p, &mut st).is_empty());
    }

    #[test]
    fn our_own_operations_coming_back_are_ignored() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "Mine", -92.0)]);
        let ops = diff(&p, &mut st);
        st.ack(&uids(&ops), true);
        // We edit again before the echo of the first push arrives.
        p.features[0].name = "Mine, newer".into();
        let echo = remote(
            1,
            ops[0].uid(),
            "upsert",
            Some(&feat(0, "Mine", -92.0)),
            "me",
        );
        assert_eq!(apply_remote(&mut p, &mut st, &[echo], "me"), 0);
        assert_eq!(p.features[0].name, "Mine, newer");
    }

    #[test]
    fn a_remote_change_does_not_overwrite_an_unconfirmed_local_one() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "Shared", -92.0)]);
        let ops = diff(&p, &mut st);
        st.ack(&uids(&ops), true);
        let uid = ops[0].uid().to_string();
        // We edit and send; before the server answers, a teammate's earlier-ordered edit arrives.
        p.features[0].name = "Ours".into();
        let ours = diff(&p, &mut st);
        assert_eq!(ours.len(), 1);
        let theirs = feat(0, "Theirs", -92.5);
        assert_eq!(
            apply_remote(
                &mut p,
                &mut st,
                &[remote(5, &uid, "upsert", Some(&theirs), "bob")],
                "me"
            ),
            0
        );
        assert_eq!(p.features[0].name, "Ours");
        st.ack(&uids(&ours), true);
        assert!(diff(&p, &mut st).is_empty(), "converged on our version");
    }

    #[test]
    fn a_locally_deleted_feature_is_not_resurrected_by_a_remote_edit() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "Shared", -92.0)]);
        let ops = diff(&p, &mut st);
        st.ack(&uids(&ops), true);
        let uid = ops[0].uid().to_string();
        p.features.clear();
        let theirs = feat(0, "Theirs", -92.5);
        assert_eq!(
            apply_remote(
                &mut p,
                &mut st,
                &[remote(9, &uid, "upsert", Some(&theirs), "bob")],
                "me"
            ),
            0
        );
        assert!(p.features.is_empty());
        let del = diff(&p, &mut st);
        assert!(matches!(del[0], LocalOp::Delete { .. }));
    }

    #[test]
    fn opening_a_shared_map_replaces_local_features_and_matches_the_server() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "Old local", -92.0)]);
        let mut doc = MapDoc {
            id: "m1".into(),
            name: "Shared".into(),
            rev: 4,
            features: Default::default(),
        };
        doc.features
            .insert("uid-aaaaaaaa".into(), shared_json(&feat(0, "One", -92.1)));
        doc.features
            .insert("uid-bbbbbbbb".into(), shared_json(&feat(0, "Two", -92.2)));
        doc.features
            .insert("uid-cccccccc".into(), json!({"garbage": true}));
        assert_eq!(
            load_full(&mut p, &mut st, &doc),
            2,
            "unreadable features are skipped"
        );
        let names: Vec<&str> = p.features.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"]);
        assert!(
            diff(&p, &mut st).is_empty(),
            "freshly loaded content is in sync"
        );
    }

    #[test]
    fn sharing_the_current_map_uploads_a_snapshot() {
        let mut st = SyncState::default();
        let mut p = project(vec![feat(1, "A", -92.0), feat(2, "B", -92.1)]);
        let snap = snapshot(&p, &mut st);
        assert_eq!(snap.len(), 2);
        assert!(snap.keys().all(|u| u.len() >= 8));
        ack_snapshot(&mut st, &snap);
        assert!(diff(&p, &mut st).is_empty());
        p.features[1].notes = "changed".into();
        assert_eq!(diff(&p, &mut st).len(), 1);
    }
}
