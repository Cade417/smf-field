//! Live team tracking: positions of people and vehicles from Garmin inReach MapShare feeds
//! and from APRS-IS. Each source runs on its own thread and reports through a channel.

mod aprs;
mod inreach;
#[cfg(not(target_arch = "wasm32"))]
mod sources;
#[cfg(target_arch = "wasm32")]
#[path = "sources_web.rs"]
mod sources;

pub use aprs::{aprs_filter, parse_aprs};
pub use inreach::{mapshare_url, parse_mapshare, parse_us_datetime};
pub use sources::{APRS_DEFAULT_LOGIN, APRS_DEFAULT_SERVER, Handle, LiveSource, spawn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Network {
    InReach,
    Aprs,
}

/// One reported position.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub network: Network,
    /// Stable id within the network (IMEI/name for inReach, callsign for APRS).
    pub id: String,
    /// What to show on the map.
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    pub time_unix: Option<u64>,
    pub speed_mps: Option<f64>,
    pub course_deg: Option<f64>,
    pub alt_m: Option<f64>,
    /// Message text or APRS comment.
    pub text: String,
    pub emergency: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// New reports (several for a feed refresh, usually one for APRS).
    Reports(Vec<Report>),
    Status(String),
    /// The source stopped; the message says why.
    Error(String),
}
