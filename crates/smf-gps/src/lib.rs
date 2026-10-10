//! GPS input for field navigation. Positions come from NMEA 0183 (serial GPS pucks and
//! Garmin units in NMEA mode, gpsd, or a replayed log file) or from the operating
//! system's location service. Every source runs on its own thread and reports through a
//! channel, so the map never blocks on a device.

mod nmea;
mod sources;
#[cfg(windows)]
mod wloc;

pub use nmea::{NmeaParser, checksum_ok, parse_latlon};
pub use sources::{Source, available_ports, spawn};

use std::sync::mpsc::Receiver;
use std::sync::{Arc, atomic::AtomicBool};

/// One position report.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fix {
    pub lng: f64,
    pub lat: f64,
    pub alt_m: Option<f64>,
    pub speed_mps: Option<f64>,
    /// Direction of travel, degrees clockwise from true north.
    pub course_deg: Option<f64>,
    /// Estimated horizontal error radius in metres, when the source states one.
    pub accuracy_m: Option<f64>,
    pub hdop: Option<f64>,
    pub satellites: Option<u32>,
    pub time_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Fix(Fix),
    /// The device is talking but has no position yet.
    NoFix,
    /// Connected / source-specific progress, for display.
    Status(String),
    /// The source stopped; the message says why.
    Error(String),
}

/// A running source. Dropping it (or calling `stop`) ends its thread.
pub struct Handle {
    pub rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}
