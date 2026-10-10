//! GPS in the browser: only the device's own location through the Geolocation API (the page
//! must be served over HTTPS and the user must allow it). Serial ports, gpsd and log replay need
//! the desktop app.

use crate::{Event, Fix, Handle};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Serial { port: String, baud: u32 },
    Gpsd { addr: String },
    NmeaFile { path: PathBuf, interval_ms: u64 },
    System,
}

pub fn available_ports() -> Vec<String> {
    Vec::new()
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

pub fn spawn(src: Source) -> Handle {
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let mut cleanup: Option<Box<dyn FnOnce()>> = None;
    match src {
        Source::System => {
            let geo = web_sys::window().and_then(|w| w.navigator().geolocation().ok());
            match geo {
                None => {
                    let _ = tx.send(Event::Error(
                        "This browser does not offer location access.".into(),
                    ));
                }
                Some(geo) => {
                    let tx_ok = tx.clone();
                    let ok = Closure::<dyn FnMut(web_sys::Position)>::new(
                        move |p: web_sys::Position| {
                            let c = p.coords();
                            let _ = tx_ok.send(Event::Fix(Fix {
                                lat: c.latitude(),
                                lng: c.longitude(),
                                alt_m: finite(c.altitude()),
                                speed_mps: finite(c.speed()),
                                course_deg: finite(c.heading()),
                                accuracy_m: Some(c.accuracy()).filter(|a| a.is_finite()),
                                time_unix: Some((p.timestamp() / 1000.0) as u64),
                                ..Fix::default()
                            }));
                        },
                    );
                    let tx_err = tx.clone();
                    let err = Closure::<dyn FnMut(web_sys::PositionError)>::new(
                        move |e: web_sys::PositionError| {
                            let msg = match e.code() {
                            1 => "Location access was denied. Allow it in the browser's site settings.".to_string(),
                            2 => "The browser could not determine your location.".to_string(),
                            3 => "Finding your location timed out.".to_string(),
                            _ => e.message(),
                        };
                            let _ = tx_err.send(Event::Error(msg));
                        },
                    );
                    let opts = web_sys::PositionOptions::new();
                    opts.set_enable_high_accuracy(true);
                    opts.set_maximum_age(5000);
                    match geo.watch_position_with_error_callback_and_options(
                        ok.as_ref().unchecked_ref(),
                        Some(err.as_ref().unchecked_ref()),
                        &opts,
                    ) {
                        Ok(id) => {
                            let _ = tx.send(Event::Status("Using this device's location".into()));
                            cleanup = Some(Box::new(move || {
                                geo.clear_watch(id);
                                drop(ok);
                                drop(err);
                            }));
                        }
                        Err(_) => {
                            let _ =
                                tx.send(Event::Error("Could not start location tracking.".into()));
                        }
                    }
                }
            }
        }
        _ => {
            let _ = tx.send(Event::Error(
                "Serial GPS, gpsd and NMEA logs are only available in the desktop app.".into(),
            ));
        }
    }
    Handle {
        rx,
        stop,
        cleanup: std::cell::RefCell::new(cleanup),
    }
}
