//! Background threads that keep a shared map and teammate positions live: a poller that
//! long-polls the server for news, and a writer that sends our changes and position.
//! They are separate so a long wait for news never delays our own edits.

use crate::client::{Events, FieldClient, Position, SyncError};
use crate::engine::LocalOp;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

const WAIT_SECS: u64 = 20;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Stay well under the server's per-request operation limit.
const OPS_PER_REQUEST: usize = 200;

pub enum Command {
    Push(Vec<LocalOp>),
    Position(Position),
    ClearPosition,
}

pub enum Update {
    /// News from the server (teammates' operations and positions).
    Events(Events),
    /// Our operations were accepted; the map is now at `rev`.
    Pushed {
        uids: Vec<String>,
        rev: u64,
    },
    /// Our operations were not accepted; they will be offered again.
    PushFailed {
        uids: Vec<String>,
        error: SyncError,
    },
    Status(String),
    /// Credentials stopped working: the workers have exited.
    LoggedOut(String),
}

pub struct Handle {
    tx: Sender<Command>,
    pub rx: Receiver<Update>,
    stop: Arc<AtomicBool>,
    /// While the poller waits for the app to reload the map after a reset.
    paused: Arc<AtomicBool>,
    since_rev: Arc<AtomicU64>,
}

impl Handle {
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }

    /// After reloading the whole map (reset), continue from its revision.
    pub fn resume_from(&self, rev: u64) {
        self.since_rev.store(rev, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let mut waited = Duration::ZERO;
    while waited < d && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        waited += Duration::from_millis(100);
    }
}

/// Start the workers for `map_id` (None: positions only) from the given cursors.
pub fn spawn(
    client: FieldClient,
    map_id: Option<String>,
    since_rev: u64,
    pos_since: u64,
) -> Handle {
    let (cmd_tx, cmd_rx) = channel::<Command>();
    let (up_tx, up_rx) = channel::<Update>();
    let stop = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let rev = Arc::new(AtomicU64::new(since_rev));

    // Poller
    {
        let (client, up, stop, paused, rev, map_id) = (
            client.clone(),
            up_tx.clone(),
            stop.clone(),
            paused.clone(),
            rev.clone(),
            map_id.clone(),
        );
        std::thread::spawn(move || {
            let mut pos_since = pos_since;
            let mut backoff = Duration::from_secs(2);
            while !stop.load(Ordering::Relaxed) {
                if paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let since = rev.load(Ordering::SeqCst);
                match client.events(map_id.as_deref(), since, pos_since, WAIT_SECS) {
                    Ok(ev) => {
                        backoff = Duration::from_secs(2);
                        // A reload may have moved the cursor while we waited: drop stale news.
                        if rev.load(Ordering::SeqCst) != since {
                            continue;
                        }
                        pos_since = pos_since.max(ev.pos_seq);
                        if ev.reset {
                            paused.store(true, Ordering::SeqCst);
                        } else {
                            rev.store(ev.rev.max(since), Ordering::SeqCst);
                        }
                        if ev.reset || !ev.ops.is_empty() || !ev.positions.is_empty() {
                            let _ = up.send(Update::Events(ev));
                        }
                    }
                    Err(SyncError::Auth(m)) => {
                        let _ = up.send(Update::LoggedOut(m));
                        stop.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let _ = up.send(Update::Status(format!("{e}; retrying")));
                        sleep_unless(&stop, backoff);
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
            }
        });
    }

    // Writer
    {
        let (up, stop) = (up_tx, stop.clone());
        let map_id = map_id.clone();
        std::thread::spawn(move || {
            let mut pending_pos: Option<Position> = None;
            let mut clear = false;
            while !stop.load(Ordering::Relaxed) {
                // Gather what is waiting; only the newest position matters.
                let mut pushes: Vec<LocalOp> = vec![];
                match cmd_rx.recv_timeout(Duration::from_millis(300)) {
                    Ok(c) => {
                        let mut next = Some(c);
                        while let Some(c) = next.take() {
                            match c {
                                Command::Push(mut v) => pushes.append(&mut v),
                                Command::Position(p) => {
                                    pending_pos = Some(p);
                                    clear = false;
                                }
                                Command::ClearPosition => {
                                    pending_pos = None;
                                    clear = true;
                                }
                            }
                            next = cmd_rx.try_recv().ok();
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if let Some(m) = &map_id {
                    for chunk in pushes.chunks(OPS_PER_REQUEST) {
                        let uids: Vec<String> = chunk.iter().map(|o| o.uid().to_string()).collect();
                        match client.post_ops(m, chunk) {
                            Ok(rev) => {
                                let _ = up.send(Update::Pushed { uids, rev });
                            }
                            Err(SyncError::Auth(msg)) => {
                                let _ = up.send(Update::LoggedOut(msg));
                                stop.store(true, Ordering::Relaxed);
                                return;
                            }
                            Err(error) => {
                                let _ = up.send(Update::PushFailed { uids, error });
                            }
                        }
                    }
                }
                if let Some(p) = pending_pos.take() {
                    match client.post_position(&p) {
                        Ok(()) => {}
                        // Sending faster than once a second is refused; the next report replaces it.
                        Err(SyncError::Server { status: 429, .. }) => {}
                        Err(SyncError::Auth(msg)) => {
                            let _ = up.send(Update::LoggedOut(msg));
                            stop.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(e) => {
                            let _ = up.send(Update::Status(format!("position not sent: {e}")));
                        }
                    }
                }
                if clear {
                    clear = false;
                    let _ = client.clear_position();
                }
            }
        });
    }

    Handle {
        tx: cmd_tx,
        rx: up_rx,
        stop,
        paused,
        since_rev: rev,
    }
}
