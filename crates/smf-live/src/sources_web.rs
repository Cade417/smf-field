//! Browsers cannot reach these feeds (APRS-IS needs a raw TCP connection and Garmin's feed does
//! not allow cross-site requests), so on the web a live source reports that it is unavailable.

use crate::Event;
use std::sync::mpsc::{Receiver, channel};

pub const APRS_DEFAULT_SERVER: &str = "rotate.aprs2.net:14580";
pub const APRS_DEFAULT_LOGIN: &str = "SMFFLD";

#[derive(Debug, Clone, PartialEq)]
pub enum LiveSource {
    InReach {
        id: String,
        password: Option<String>,
    },
    Aprs {
        server: String,
        filter: String,
        callsign: Option<String>,
    },
}

pub struct Handle {
    pub rx: Receiver<Event>,
}

impl Handle {
    pub fn stop(&self) {}
}

pub fn spawn(_src: LiveSource) -> Handle {
    let (tx, rx) = channel();
    let _ = tx.send(Event::Error(
        "Live tracking from inReach and APRS is only available in the desktop app.".into(),
    ));
    Handle { rx }
}
