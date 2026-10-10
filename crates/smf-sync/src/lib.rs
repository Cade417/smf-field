//! Team collaboration for ShowMeFire Field: a client for the Show Me Fire `/api/field/v1`
//! service, an engine that turns local edits into operations and applies teammates'
//! operations, and a tick-driven session that keeps a shared map and teammate positions live.

mod client;
mod engine;
mod worker;

#[cfg(not(target_arch = "wasm32"))]
pub use client::wait_blocking;
pub use client::{
    Creds, Events, FieldClient, MapDoc, MapInfo, Me, Member, PlanInfo, PlanLimits, PlanUsage,
    Position, RemoteOp, SyncError, TeamPosition,
};
pub use engine::{
    LocalOp, SyncState, ack_snapshot, apply_remote, diff, load_full, new_uid, snapshot,
};
pub use worker::{Command, Handle, Update, spawn};
