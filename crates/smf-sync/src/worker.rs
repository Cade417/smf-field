//! Keeps a shared map and teammate positions live without any background threads of its own.
//!
//! The session is driven by `Handle::tick()`, which the app calls every frame: it starts the
//! next long poll for news, sends queued edits and position reports, and honours back-off after
//! errors. Requests complete through callbacks (a short-lived thread each on desktop, the
//! browser's fetch on the web) that update shared state and ask for a repaint via `wake`, so
//! the same code runs in both places. Timing uses `web_time`, which works on the web too.

use crate::client::{Events, FieldClient, Position, SyncError};
use crate::engine::LocalOp;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use web_time::Instant;

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
    /// Credentials stopped working: the session has ended.
    LoggedOut(String),
}

struct Inner {
    stopped: bool,
    /// Waiting for the app to reload the map after a reset.
    paused: bool,
    rev: u64,
    pos_since: u64,
    poll_in_flight: bool,
    next_poll: Instant,
    backoff: Duration,
    queue: VecDeque<LocalOp>,
    push_in_flight: bool,
    pending_pos: Option<Position>,
    clear_pos: bool,
    pos_in_flight: bool,
}

pub struct Handle {
    client: FieldClient,
    map_id: Option<String>,
    inner: Arc<Mutex<Inner>>,
    up: Sender<Update>,
    pub rx: Receiver<Update>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Handle {
    pub fn send(&self, c: Command) {
        let Ok(mut g) = self.inner.lock() else { return };
        match c {
            Command::Push(ops) => g.queue.extend(ops),
            Command::Position(p) => {
                g.pending_pos = Some(p);
                g.clear_pos = false;
            }
            Command::ClearPosition => {
                g.pending_pos = None;
                g.clear_pos = true;
            }
        }
    }

    /// After reloading the whole map (reset), continue from its revision.
    pub fn resume_from(&self, rev: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.rev = rev;
            g.paused = false;
        }
    }

    pub fn stop(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.stopped = true;
        }
    }

    /// Start whatever request is due. Cheap; call it every frame.
    pub fn tick(&self) {
        let now = Instant::now();
        let (poll, push, pos, clear) = {
            let Ok(mut g) = self.inner.lock() else { return };
            if g.stopped {
                return;
            }
            let poll = (!g.paused && !g.poll_in_flight && now >= g.next_poll)
                .then(|| (g.rev, g.pos_since));
            if poll.is_some() {
                g.poll_in_flight = true;
            }
            let mut push = None;
            if self.map_id.is_some() && !g.push_in_flight && !g.queue.is_empty() {
                let n = g.queue.len().min(OPS_PER_REQUEST);
                push = Some(g.queue.drain(..n).collect::<Vec<_>>());
                g.push_in_flight = true;
            }
            let mut pos = None;
            if !g.pos_in_flight
                && let Some(p) = g.pending_pos.take()
            {
                pos = Some(p);
                g.pos_in_flight = true;
            }
            let mut clear = false;
            if !g.pos_in_flight && g.clear_pos {
                g.clear_pos = false;
                g.pos_in_flight = true;
                clear = true;
            }
            (poll, push, pos, clear)
        };

        if let Some((since, pos_since)) = poll {
            let (inner, up, wake) = (self.inner.clone(), self.up.clone(), self.wake.clone());
            self.client.events(
                self.map_id.as_deref(),
                since,
                pos_since,
                WAIT_SECS,
                move |r| {
                    if let Ok(mut g) = inner.lock() {
                        g.poll_in_flight = false;
                        match r {
                            Ok(ev) => {
                                g.backoff = Duration::from_secs(2);
                                g.next_poll = Instant::now();
                                // A reload may have moved the cursor while we waited: drop stale news.
                                if g.rev == since {
                                    g.pos_since = g.pos_since.max(ev.pos_seq);
                                    if ev.reset {
                                        g.paused = true;
                                    } else {
                                        g.rev = ev.rev.max(since);
                                    }
                                    if ev.reset || !ev.ops.is_empty() || !ev.positions.is_empty() {
                                        let _ = up.send(Update::Events(ev));
                                    }
                                }
                            }
                            Err(SyncError::Auth(m)) => {
                                g.stopped = true;
                                let _ = up.send(Update::LoggedOut(m));
                            }
                            Err(e) => {
                                let _ = up.send(Update::Status(format!("{e}; retrying")));
                                g.next_poll = Instant::now() + g.backoff;
                                g.backoff = (g.backoff * 2).min(MAX_BACKOFF);
                            }
                        }
                    }
                    wake();
                },
            );
        }

        if let (Some(ops), Some(map)) = (push, self.map_id.as_deref()) {
            let uids: Vec<String> = ops.iter().map(|o| o.uid().to_string()).collect();
            let (inner, up, wake) = (self.inner.clone(), self.up.clone(), self.wake.clone());
            self.client.post_ops(map, &ops, move |r| {
                if let Ok(mut g) = inner.lock() {
                    g.push_in_flight = false;
                    match r {
                        Ok(rev) => {
                            let _ = up.send(Update::Pushed { uids, rev });
                        }
                        Err(SyncError::Auth(m)) => {
                            g.stopped = true;
                            let _ = up.send(Update::LoggedOut(m));
                        }
                        Err(error) => {
                            let _ = up.send(Update::PushFailed { uids, error });
                        }
                    }
                }
                wake();
            });
        }

        if let Some(p) = pos {
            let (inner, up, wake) = (self.inner.clone(), self.up.clone(), self.wake.clone());
            self.client.post_position(&p, move |r| {
                if let Ok(mut g) = inner.lock() {
                    g.pos_in_flight = false;
                    match r {
                        // Sending faster than once a second is refused; the next report replaces it.
                        Ok(()) | Err(SyncError::Server { status: 429, .. }) => {}
                        Err(SyncError::Auth(m)) => {
                            g.stopped = true;
                            let _ = up.send(Update::LoggedOut(m));
                        }
                        Err(e) => {
                            let _ = up.send(Update::Status(format!("position not sent: {e}")));
                        }
                    }
                }
                wake();
            });
        }

        if clear {
            let (inner, wake) = (self.inner.clone(), self.wake.clone());
            self.client.clear_position(move |_| {
                if let Ok(mut g) = inner.lock() {
                    g.pos_in_flight = false;
                }
                wake();
            });
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a session for `map_id` (None: positions only) from the given cursors. `wake` is called
/// whenever something completes, so the UI can repaint and tick again.
pub fn spawn(
    client: FieldClient,
    map_id: Option<String>,
    since_rev: u64,
    pos_since: u64,
    wake: impl Fn() + Send + Sync + 'static,
) -> Handle {
    let (up, rx) = channel::<Update>();
    Handle {
        client,
        map_id,
        inner: Arc::new(Mutex::new(Inner {
            stopped: false,
            paused: false,
            rev: since_rev,
            pos_since,
            poll_in_flight: false,
            next_poll: Instant::now(),
            backoff: Duration::from_secs(2),
            queue: VecDeque::new(),
            push_in_flight: false,
            pending_pos: None,
            clear_pos: false,
            pos_in_flight: false,
        })),
        up,
        rx,
        wake: Arc::new(wake),
    }
}
