//! Source threads: poll a MapShare feed, or stream APRS-IS.

use crate::{Event, aprs::parse_aprs, inreach};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Garmin caches feeds for a minute or two, so polling faster only wastes requests.
const POLL: Duration = Duration::from_secs(120);
/// How far back to ask for points, so a track trail can be drawn on first load.
const LOOKBACK_SECS: u64 = 6 * 3600;
pub const APRS_DEFAULT_SERVER: &str = "rotate.aprs2.net:14580";
/// Read-only login name when the user has not given a callsign. The usual placeholder
/// N0CALL is refused by APRS-IS servers, so it cannot be used.
pub const APRS_DEFAULT_LOGIN: &str = "SMFFLD";

#[derive(Debug, Clone, PartialEq)]
pub enum LiveSource {
    /// Garmin inReach MapShare name, with the feed password when the owner set one.
    InReach {
        id: String,
        password: Option<String>,
    },
    /// APRS-IS read-only connection with a server-side filter (see `aprs_filter`).
    /// `callsign` is the login name (the user's own, if they have one).
    Aprs {
        server: String,
        filter: String,
        callsign: Option<String>,
    },
}

pub struct Handle {
    pub rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn spawn(src: LiveSource) -> Handle {
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        let r = match src {
            LiveSource::InReach { id, password } => run_inreach(&id, password.as_deref(), &tx, &s),
            LiveSource::Aprs {
                server,
                filter,
                callsign,
            } => run_aprs(&server, &filter, callsign.as_deref(), &tx, &s),
        };
        if let Err(e) = r {
            let _ = tx.send(Event::Error(e));
        }
    });
    Handle { rx, stop }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let mut waited = Duration::ZERO;
    while waited < total && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        waited += Duration::from_millis(250);
    }
}

fn run_inreach(
    id: &str,
    password: Option<&str>,
    tx: &Sender<Event>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    // Validate the name once up front so a typo is reported immediately.
    inreach::mapshare_url(id, None)?;
    while !stop.load(Ordering::Relaxed) {
        let url = inreach::mapshare_url(id, Some(now().saturating_sub(LOOKBACK_SECS)))?;
        let mut req = http.get(&url);
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            // The feed uses HTTP Basic auth with an empty user name.
            req = req.basic_auth("", Some(p));
        }
        match req.send() {
            Ok(r) => match r.status().as_u16() {
                200 => {
                    let text = r.text().map_err(|e| e.without_url().to_string())?;
                    let reports = inreach::parse_mapshare(&text)?;
                    let _ = tx.send(Event::Status(format!(
                        "inReach {id}: {} points",
                        reports.len()
                    )));
                    if !reports.is_empty() {
                        let _ = tx.send(Event::Reports(reports));
                    }
                }
                401 | 403 => {
                    return Err(
                        "The MapShare page needs a password, or the password is wrong.".into(),
                    );
                }
                404 => return Err(format!("No MapShare named \"{id}\" was found.")),
                c => {
                    let _ = tx.send(Event::Status(format!(
                        "inReach feed answered {c}; will retry"
                    )));
                }
            },
            Err(e) => {
                let _ = tx.send(Event::Status(format!(
                    "Could not reach Garmin ({}); will retry",
                    e.without_url()
                )));
            }
        }
        sleep_unless_stopped(POLL, stop);
    }
    Ok(())
}

fn run_aprs(
    server: &str,
    filter: &str,
    callsign: Option<&str>,
    tx: &Sender<Event>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let addr = server
        .to_socket_addrs()
        .map_err(|e| format!("bad APRS server {server}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {server}"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("could not reach the APRS server {server}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok();
    // Passcode -1 is the standard read-only login: we never transmit.
    let login = callsign
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_uppercase)
        .unwrap_or_else(|| APRS_DEFAULT_LOGIN.to_string());
    if login.len() > 9 || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("That is not a valid callsign.".into());
    }
    write!(
        s,
        "user {login} pass -1 vers ShowMeFireField {} filter {}\r\n",
        env!("CARGO_PKG_VERSION"),
        filter
    )
    .map_err(|e| format!("APRS login failed: {e}"))?;
    let _ = tx.send(Event::Status(format!("Connected to APRS-IS ({filter})")));
    let mut r = BufReader::new(s);
    let mut buf: Vec<u8> = vec![];
    while !stop.load(Ordering::Relaxed) {
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => return Err("the APRS server closed the connection".into()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => continue,
            Err(e) => return Err(format!("APRS read failed: {e}")),
        }
        if buf.last() != Some(&b'\n') {
            continue;
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        buf.clear();
        // Server notices start with '#'. A refused login would otherwise look like silence.
        if line.starts_with('#') {
            let l = line.to_ascii_lowercase();
            if l.contains("not allowed") || l.contains("invalid") || l.contains("banned") {
                return Err(format!(
                    "The APRS server refused the login: {}",
                    line.trim_start_matches('#').trim()
                ));
            }
        }
        if let Some(rep) = parse_aprs(&line, now())
            && tx.send(Event::Reports(vec![rep])).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn aprs_client_logs_in_read_only_and_delivers_reports() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut login = String::new();
            BufReader::new(s.try_clone().unwrap())
                .read_line(&mut login)
                .unwrap();
            // Banner, then packets split across writes.
            s.write_all(b"# aprsc test\r\nW0ABC-9>APRS,TCPIP*:!3834.60N/09210.41W>088/036 hi\r\n")
                .unwrap();
            s.write_all(b"K0XYZ>APRS:=4903.50N/").unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            s.write_all(b"07201.75W-\r\n").unwrap();
            std::thread::sleep(Duration::from_millis(300));
            login
        });
        let h = spawn(LiveSource::Aprs {
            server: format!("127.0.0.1:{port}"),
            filter: "b/W0ABC-9/K0XYZ".into(),
            callsign: None,
        });
        let mut got = vec![];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 2 && std::time::Instant::now() < deadline {
            if let Ok(Event::Reports(r)) = h.rx.recv_timeout(Duration::from_millis(200)) {
                got.extend(r);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "W0ABC-9");
        assert_eq!(got[1].id, "K0XYZ");
        h.stop();
        let login = server.join().unwrap();
        assert!(
            login.starts_with("user SMFFLD pass -1 vers ShowMeFireField"),
            "{login}"
        );
        assert!(login.contains("filter b/W0ABC-9/K0XYZ"), "{login}");
    }

    #[test]
    fn bad_names_and_dead_servers_are_reported() {
        let h = spawn(LiveSource::InReach {
            id: "bad name!".into(),
            password: None,
        });
        assert!(matches!(
            h.rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Error(_)
        ));
        let h = spawn(LiveSource::Aprs {
            server: "127.0.0.1:9".into(),
            filter: "b/X".into(),
            callsign: None,
        });
        let ev = h.rx.recv_timeout(Duration::from_secs(12)).unwrap();
        assert!(matches!(ev, Event::Error(m) if m.contains("could not reach")));
    }

    /// Reads real packets from the public APRS-IS network; run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_aprs_smoke() {
        let h = spawn(LiveSource::Aprs {
            server: APRS_DEFAULT_SERVER.into(),
            filter: "r/38.6/-92.2/400".into(),
            callsign: None,
        });
        let mut n = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while n < 3 && std::time::Instant::now() < deadline {
            match h.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::Reports(r)) => {
                    println!("{:?}", r[0]);
                    n += 1;
                }
                Ok(other) => println!("{other:?}"),
                Err(_) => {}
            }
        }
        assert!(n >= 1, "no APRS reports within 90 s");
    }
}
