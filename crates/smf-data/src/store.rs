//! Keeps vector data layers fresh in the background: ETag-conditional fetches on a schedule,
//! an on-disk copy of the last good response (so the app opens with data even offline), and
//! exponential backoff when the server is unreachable.

use crate::client::{Fetched, SmfClient};
use crate::layers::{DataError, DataLayerKind, ParsedLayer, parse_layer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use web_time::{Instant, SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    etag: Option<String>,
    fetched_unix: u64,
    body: String,
}

/// The last good response per layer, stored as JSON files.
#[derive(Clone)]
pub struct DataCache {
    dir: PathBuf,
}

impl DataCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, kind: DataLayerKind) -> PathBuf {
        self.dir.join(format!("{}.json", kind.id()))
    }

    // Browsers have no files: there the last copy simply is not kept between visits.
    #[cfg(not(target_arch = "wasm32"))]
    fn load(&self, kind: DataLayerKind) -> Option<CacheFile> {
        serde_json::from_str(&std::fs::read_to_string(self.path(kind)).ok()?).ok()
    }

    #[cfg(target_arch = "wasm32")]
    fn load(&self, _kind: DataLayerKind) -> Option<CacheFile> {
        None
    }

    #[cfg(target_arch = "wasm32")]
    fn store(&self, _kind: DataLayerKind, _f: &CacheFile) {}

    #[cfg(not(target_arch = "wasm32"))]
    fn store(&self, kind: DataLayerKind, f: &CacheFile) {
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.path(kind).with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(f)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(tmp, self.path(kind));
        }
    }
}

pub enum Command {
    SetEnabled(DataLayerKind, bool),
    /// Fetch every enabled layer now, ignoring the schedule.
    RefreshNow,
    SetOffline(bool),
}

#[derive(Debug)]
pub enum Update {
    Loaded {
        kind: DataLayerKind,
        parsed: ParsedLayer,
        fetched_unix: u64,
        from_cache: bool,
    },
    /// The server confirmed our copy is current (HTTP 304).
    Unchanged {
        kind: DataLayerKind,
        fetched_unix: u64,
    },
    Failed {
        kind: DataLayerKind,
        error: String,
    },
}

struct Slot {
    enabled: bool,
    etag: Option<String>,
    last_attempt: Option<Instant>,
    last_ok_unix: Option<u64>,
    fails: u32,
    cache_loaded: bool,
    force: bool,
    in_flight: bool,
}

struct State {
    slots: HashMap<DataLayerKind, Slot>,
    offline: bool,
    stopped: bool,
}

/// Keeps the enabled layers fresh. It has no thread of its own: call `tick()` every frame and
/// it starts whatever fetch is due (requests finish through callbacks, see `smf-net`).
pub struct Refresher {
    state: Arc<Mutex<State>>,
    client: SmfClient,
    cache: DataCache,
    wake: Arc<dyn Fn() + Send + Sync>,
    interval_override: Option<Duration>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
}

/// Seconds between attempts: the layer's interval, doubled per consecutive failure up to 30 minutes.
pub fn next_interval(base_secs: u64, fails: u32) -> u64 {
    (base_secs << fails.min(4)).min(30 * 60)
}

impl Refresher {
    /// `interval_override` shortens every layer's schedule (tests). `wake` is called after each update.
    pub fn spawn(
        client: SmfClient,
        cache: DataCache,
        wake: impl Fn() + Send + Sync + 'static,
        interval_override: Option<Duration>,
    ) -> Self {
        let (tx, rx) = channel::<Update>();
        Self {
            state: Arc::new(Mutex::new(State {
                slots: HashMap::new(),
                offline: false,
                stopped: false,
            })),
            client,
            cache,
            wake: Arc::new(wake),
            interval_override,
            tx,
            rx,
        }
    }

    pub fn send(&self, c: Command) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        match c {
            Command::SetEnabled(k, on) => {
                let s = st.slots.entry(k).or_insert(Slot {
                    enabled: false,
                    etag: None,
                    last_attempt: None,
                    last_ok_unix: None,
                    fails: 0,
                    cache_loaded: false,
                    force: false,
                    in_flight: false,
                });
                s.enabled = on;
            }
            Command::RefreshNow => st.slots.values_mut().for_each(|s| s.force = true),
            Command::SetOffline(o) => {
                st.offline = o;
                // Back online: retry immediately instead of waiting out a long backoff.
                if !o {
                    st.slots.values_mut().for_each(|s| {
                        s.force = true;
                        s.fails = 0;
                    });
                }
            }
        }
    }

    pub fn set_enabled(&self, kind: DataLayerKind, on: bool) {
        self.send(Command::SetEnabled(kind, on));
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.rx.try_recv().ok()
    }

    /// Start any fetch that is due. Cheap; call it every frame.
    pub fn tick(&self) {
        let mut start: Vec<(DataLayerKind, Option<String>)> = vec![];
        {
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            if st.stopped {
                return;
            }
            let offline = st.offline;
            for kind in DataLayerKind::ALL {
                let Some(slot) = st.slots.get_mut(&kind).filter(|s| s.enabled) else {
                    continue;
                };
                // First time on: show the last good copy right away, then update it.
                if !slot.cache_loaded {
                    slot.cache_loaded = true;
                    if let Some(c) = self.cache.load(kind)
                        && let Ok(parsed) = parse_layer(kind, &c.body)
                    {
                        slot.etag = c.etag;
                        slot.last_ok_unix = Some(c.fetched_unix);
                        let _ = self.tx.send(Update::Loaded {
                            kind,
                            parsed,
                            fetched_unix: c.fetched_unix,
                            from_cache: true,
                        });
                        (self.wake)();
                    }
                }
                if offline || slot.in_flight {
                    continue;
                }
                let base = self
                    .interval_override
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(kind.refresh_secs() as f64);
                let wait = Duration::from_secs_f64(base * f64::from(1u32 << slot.fails.min(4)))
                    .min(Duration::from_secs(30 * 60));
                let due = slot.force || slot.last_attempt.is_none_or(|t| t.elapsed() >= wait);
                if !due {
                    continue;
                }
                slot.force = false;
                slot.last_attempt = Some(Instant::now());
                slot.in_flight = true;
                start.push((kind, slot.etag.clone()));
            }
        }
        for (kind, etag) in start {
            let (state, tx, cache, wake) = (
                self.state.clone(),
                self.tx.clone(),
                self.cache.clone(),
                self.wake.clone(),
            );
            self.client.get(kind.path(), etag.as_deref(), move |res| {
                let result = res.and_then(|f| match f {
                    Fetched::NotModified => Ok(None),
                    Fetched::Body { text, etag } => {
                        parse_layer(kind, &text).map(|p| Some((p, text, etag)))
                    }
                });
                let Ok(mut st) = state.lock() else { return };
                let Some(slot) = st.slots.get_mut(&kind) else {
                    return;
                };
                slot.in_flight = false;
                let msg = match result {
                    Ok(None) => {
                        slot.fails = 0;
                        let now = unix_now();
                        slot.last_ok_unix = Some(now);
                        Update::Unchanged {
                            kind,
                            fetched_unix: now,
                        }
                    }
                    Ok(Some((parsed, body, etag))) => {
                        slot.fails = 0;
                        let now = unix_now();
                        slot.etag = etag.clone();
                        slot.last_ok_unix = Some(now);
                        cache.store(
                            kind,
                            &CacheFile {
                                etag,
                                fetched_unix: now,
                                body,
                            },
                        );
                        Update::Loaded {
                            kind,
                            parsed,
                            fetched_unix: now,
                            from_cache: false,
                        }
                    }
                    Err(e) => {
                        slot.fails += 1;
                        tracing::warn!("{} refresh failed: {e}", kind.id());
                        Update::Failed {
                            kind,
                            error: describe(&e),
                        }
                    }
                };
                let _ = tx.send(msg);
                drop(st);
                wake();
            });
        }
    }
}

fn describe(e: &DataError) -> String {
    match e {
        DataError::Network(_) => "can't reach the Show Me Fire server".into(),
        other => other.to_string(),
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.lock() {
            st.stopped = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::testserver::serve;
    use std::sync::atomic::Ordering as O;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("smf-data-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn recv(r: &Refresher) -> Update {
        for _ in 0..400 {
            r.tick();
            if let Some(u) = r.try_recv() {
                return u;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for an update");
    }

    /// Let time pass while ticking, as the app's frames do.
    fn settle(r: &Refresher, ms: u64) {
        let end = std::time::Instant::now() + Duration::from_millis(ms);
        while std::time::Instant::now() < end {
            r.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn recv_none(r: &Refresher, ms: u64) -> bool {
        settle(r, ms);
        r.try_recv().is_none()
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(next_interval(120, 0), 120);
        assert_eq!(next_interval(120, 1), 240);
        assert_eq!(next_interval(120, 3), 960);
        assert_eq!(next_interval(120, 4), 1800, "capped at 30 minutes");
        assert_eq!(next_interval(120, 9), 1800);
    }

    #[test]
    fn fetches_conditionally_then_notices_changes_and_survives_outages() {
        let s = serve("\"v1\"", &fixture("incidents.geojson"));
        let dir = tmp("conditional");
        let client = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        let r = Refresher::spawn(
            client,
            DataCache::new(dir.clone()),
            || {},
            Some(Duration::from_millis(150)),
        );
        r.set_enabled(DataLayerKind::Incidents, true);
        // 1. First fetch loads three incidents.
        match recv(&r) {
            Update::Loaded {
                kind,
                parsed,
                from_cache,
                ..
            } => {
                assert_eq!(kind, DataLayerKind::Incidents);
                assert_eq!(parsed.features.len(), 3);
                assert!(!from_cache);
            }
            u => panic!("{u:?}"),
        }
        // 2. Next scheduled fetch sends If-None-Match and gets 304: nothing re-downloaded or re-parsed.
        assert!(matches!(recv(&r), Update::Unchanged { .. }));
        assert!(s.not_modified.load(O::Relaxed) >= 1);
        // 3. The data changes (new ETag, one incident): picked up.
        let one = fixture("incidents.geojson");
        let mut v: serde_json::Value = serde_json::from_str(&one).unwrap();
        v["features"].as_array_mut().unwrap().truncate(1);
        *s.current.lock().unwrap() = ("\"v2\"".into(), v.to_string());
        loop {
            match recv(&r) {
                Update::Loaded { parsed, .. } => {
                    assert_eq!(parsed.features.len(), 1);
                    break;
                }
                Update::Unchanged { .. } => continue,
                u => panic!("{u:?}"),
            }
        }
        // 4. The server starts failing: the update says so (the UI keeps showing the last data).
        *s.status.lock().unwrap() = 503;
        loop {
            match recv(&r) {
                Update::Failed { error, .. } => {
                    assert!(error.contains("503"), "{error}");
                    break;
                }
                Update::Unchanged { .. } | Update::Loaded { .. } => continue,
            }
        }
        // 5. It recovers and the layer refreshes again.
        *s.status.lock().unwrap() = 200;
        // (backoff doubled the wait, so allow a few intervals)
        loop {
            if let Update::Unchanged { .. } | Update::Loaded { .. } = recv(&r) {
                break;
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn layers_are_only_fetched_while_enabled() {
        let s = serve("\"v\"", &fixture("burn_bans.geojson"));
        let dir = tmp("enabled");
        let client = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        let r = Refresher::spawn(
            client,
            DataCache::new(dir.clone()),
            || {},
            Some(Duration::from_millis(100)),
        );
        assert!(
            recv_none(&r, 350),
            "nothing is fetched until a layer is switched on"
        );
        assert_eq!(s.hits.load(O::Relaxed), 0);
        r.set_enabled(DataLayerKind::BurnBans, true);
        assert!(matches!(
            recv(&r),
            Update::Loaded {
                kind: DataLayerKind::BurnBans,
                ..
            }
        ));
        r.set_enabled(DataLayerKind::BurnBans, false);
        settle(&r, 250);
        while r.try_recv().is_some() {}
        let hits = s.hits.load(O::Relaxed);
        assert!(recv_none(&r, 400));
        assert_eq!(
            s.hits.load(O::Relaxed),
            hits,
            "switched off: no more requests"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn last_good_data_is_available_offline_and_after_restart() {
        let dir = tmp("offline");
        {
            let s = serve("\"v1\"", &fixture("incidents.geojson"));
            let client = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
            let r = Refresher::spawn(client, DataCache::new(dir.clone()), || {}, None);
            r.set_enabled(DataLayerKind::Incidents, true);
            assert!(matches!(
                recv(&r),
                Update::Loaded {
                    from_cache: false,
                    ..
                }
            ));
        } // server and refresher gone
        // "Restart" with a dead server: the cached copy appears immediately, flagged as cached.
        let dead = SmfClient::new("http://127.0.0.1:9").unwrap();
        let r = Refresher::spawn(
            dead,
            DataCache::new(dir.clone()),
            || {},
            Some(Duration::from_millis(100)),
        );
        r.set_enabled(DataLayerKind::Incidents, true);
        match recv(&r) {
            Update::Loaded {
                parsed,
                from_cache,
                fetched_unix,
                ..
            } => {
                assert!(from_cache);
                assert_eq!(parsed.features.len(), 3);
                assert!(unix_now() - fetched_unix < 60);
            }
            u => panic!("{u:?}"),
        }
        // Then the live attempt fails, and says so.
        assert!(matches!(recv(&r), Update::Failed { .. }));
        // Explicit offline mode makes no attempts at all.
        r.send(Command::SetOffline(true));
        settle(&r, 100);
        while r.try_recv().is_some() {}
        assert!(
            recv_none(&r, 500),
            "offline: no network attempts, no failures"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_cache_files_are_ignored() {
        let dir = tmp("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("incidents.json"), b"{ garbage").unwrap();
        let s = serve("\"v\"", &fixture("incidents.geojson"));
        let client = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        let r = Refresher::spawn(client, DataCache::new(dir.clone()), || {}, None);
        r.set_enabled(DataLayerKind::Incidents, true);
        assert!(
            matches!(
                recv(&r),
                Update::Loaded {
                    from_cache: false,
                    ..
                }
            ),
            "falls through to the network"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_now_ignores_the_schedule() {
        let s = serve("\"v\"", &fixture("incidents.geojson"));
        let dir = tmp("refresh");
        let client = SmfClient::new(&format!("http://127.0.0.1:{}", s.port)).unwrap();
        // 10-minute schedule: only an explicit refresh can trigger the second fetch.
        let r = Refresher::spawn(
            client,
            DataCache::new(dir.clone()),
            || {},
            Some(Duration::from_secs(600)),
        );
        r.set_enabled(DataLayerKind::Incidents, true);
        assert!(matches!(recv(&r), Update::Loaded { .. }));
        assert!(recv_none(&r, 400));
        r.send(Command::RefreshNow);
        assert!(matches!(recv(&r), Update::Unchanged { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }
}
