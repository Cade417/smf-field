//! Windows location service (Wi-Fi / GNSS / IP, whatever the machine offers) via WinRT.

use crate::{Event, Fix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;
use windows::Devices::Geolocation::{Geolocator, PositionAccuracy};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

/// Seconds between the Windows epoch (1601) and the unix epoch (1970).
const WIN_TO_UNIX_SECS: i64 = 11_644_473_600;
const POLL: Duration = Duration::from_secs(2);

fn explain(e: &windows::core::Error) -> String {
    // E_ACCESSDENIED: the privacy switch for location (or for desktop apps) is off.
    if e.code().0 as u32 == 0x8007_0005 {
        "Windows blocked location access. Turn on Settings > Privacy & security > Location, including \"Let desktop apps access your location\"."
            .to_string()
    } else {
        format!("Windows location failed: {e}")
    }
}

pub fn run(tx: &Sender<Event>, stop: &AtomicBool) -> Result<(), String> {
    // SAFETY: plain COM initialisation for this thread, paired with process exit.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let g = Geolocator::new().map_err(|e| explain(&e))?;
    let _ = g.SetDesiredAccuracy(PositionAccuracy::High);
    let _ = tx.send(Event::Status("Using the Windows location service".into()));
    while !stop.load(Ordering::Relaxed) {
        let pos = g
            .GetGeopositionAsync()
            .and_then(|op| op.join())
            .map_err(|e| explain(&e))?;
        let c = pos.Coordinate().map_err(|e| explain(&e))?;
        let p = c
            .Point()
            .and_then(|p| p.Position())
            .map_err(|e| explain(&e))?;
        let opt = |r: windows::core::Result<windows::Foundation::IReference<f64>>| {
            r.ok().and_then(|v| v.Value().ok())
        };
        let unix = c
            .Timestamp()
            .ok()
            .map(|t| t.UniversalTime / 10_000_000 - WIN_TO_UNIX_SECS)
            .and_then(|s| u64::try_from(s).ok());
        let altitude = Some(p.Altitude).filter(|a| a.is_finite() && *a != 0.0);
        let _ = tx.send(Event::Fix(Fix {
            lat: p.Latitude,
            lng: p.Longitude,
            alt_m: altitude,
            speed_mps: opt(c.Speed()).filter(|s| s.is_finite()),
            course_deg: opt(c.Heading()).filter(|s| s.is_finite()),
            accuracy_m: c.Accuracy().ok(),
            time_unix: unix,
            ..Fix::default()
        }));
        // Sleep in short steps so Stop is quick.
        let mut waited = Duration::ZERO;
        while waited < POLL && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            waited += Duration::from_millis(100);
        }
    }
    Ok(())
}
