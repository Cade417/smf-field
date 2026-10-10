//! Position sources, each on its own thread: a serial NMEA device, gpsd over TCP, a replayed
//! NMEA log, or the operating system's location service.

use crate::{Event, Handle, nmea::NmeaParser};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A GPS on a COM / tty port that streams NMEA (most pucks, Garmin units in NMEA mode).
    Serial { port: String, baud: u32 },
    /// A gpsd daemon, "host:port" (gpsd's default port is 2947).
    Gpsd { addr: String },
    /// Replay a recorded NMEA log, one position per `interval_ms`.
    NmeaFile { path: PathBuf, interval_ms: u64 },
    /// The operating system's location service (Windows only for now).
    System,
}

pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|p| p.into_iter().map(|i| i.port_name).collect())
        .unwrap_or_default()
}

pub fn spawn(src: Source) -> Handle {
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        let r = run(src, &tx, &s);
        if let Err(e) = r {
            let _ = tx.send(Event::Error(e));
        }
    });
    Handle { rx, stop }
}

fn run(src: Source, tx: &Sender<Event>, stop: &AtomicBool) -> Result<(), String> {
    match src {
        Source::Serial { port, baud } => {
            let p = serialport::new(&port, baud)
                .timeout(Duration::from_millis(500))
                .open()
                .map_err(|e| format!("could not open {port}: {e}"))?;
            let _ = tx.send(Event::Status(format!("Listening on {port} at {baud} baud")));
            read_nmea(BufReader::new(p), tx, stop, None)
        }
        Source::Gpsd { addr } => {
            let sock = addr
                .to_socket_addrs()
                .map_err(|e| format!("bad gpsd address {addr}: {e}"))?
                .next()
                .ok_or_else(|| format!("no address for {addr}"))?;
            let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(5))
                .map_err(|e| format!("could not reach gpsd at {addr}: {e}"))?;
            s.set_read_timeout(Some(Duration::from_millis(500))).ok();
            // Raw NMEA keeps one parser for every source.
            s.write_all(b"?WATCH={\"enable\":true,\"nmea\":true}\n")
                .map_err(|e| format!("gpsd write failed: {e}"))?;
            let _ = tx.send(Event::Status(format!("Connected to gpsd at {addr}")));
            read_nmea(BufReader::new(s), tx, stop, None)
        }
        Source::NmeaFile { path, interval_ms } => {
            let f = std::fs::File::open(&path)
                .map_err(|e| format!("could not open {}: {e}", path.display()))?;
            let _ = tx.send(Event::Status(format!("Replaying {}", path.display())));
            read_nmea(
                BufReader::new(f),
                tx,
                stop,
                Some(Duration::from_millis(interval_ms)),
            )?;
            let _ = tx.send(Event::Status("Replay finished".into()));
            // Keep the thread (and the channel) alive until stopped so the last fix stays valid.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        }
        #[cfg(windows)]
        Source::System => crate::wloc::run(tx, stop),
        #[cfg(not(windows))]
        Source::System => Err(
            "The system location service isn't supported on this platform yet; use gpsd or a serial GPS."
                .into(),
        ),
    }
}

/// Read NMEA lines until stopped or the stream ends. `pace` sleeps after each position
/// (used to replay a log at human speed). Timeouts are not errors: the stream is just quiet.
fn read_nmea<R: BufRead>(
    mut r: R,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    pace: Option<Duration>,
) -> Result<(), String> {
    let mut parser = NmeaParser::default();
    let mut buf: Vec<u8> = vec![];
    while !stop.load(Ordering::Relaxed) {
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => {
                return if pace.is_some() {
                    Ok(())
                } else {
                    Err("the GPS stopped sending data".into())
                };
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => continue,
            Err(e) => return Err(format!("GPS read failed: {e}")),
        }
        // A read can return half a line when the device pauses mid-sentence.
        if buf.last() != Some(&b'\n') {
            continue;
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        buf.clear();
        if let Some(ev) = parser.feed(&line) {
            if tx.send(ev).is_err() {
                return Ok(());
            }
            if let Some(d) = pace {
                std::thread::sleep(d);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    const GGA: &str = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";
    const RMC: &str = "$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n";

    #[test]
    fn reads_a_stream_in_order_and_skips_noise() {
        let data = format!("junk\r\n{GGA}$GPGSV,1,1,00*79\r\n{RMC}");
        let (tx, rx) = channel();
        let stop = AtomicBool::new(false);
        let r = read_nmea(
            Cursor::new(data.into_bytes()),
            &tx,
            &stop,
            Some(Duration::ZERO),
        );
        assert!(r.is_ok());
        let evs: Vec<Event> = rx.try_iter().collect();
        assert_eq!(evs.len(), 2);
        assert!(
            matches!(&evs[1], Event::Fix(f) if f.speed_mps.is_some() && f.satellites == Some(8))
        );
    }

    #[test]
    fn a_live_stream_that_ends_is_an_error() {
        let (tx, _rx) = channel();
        let stop = AtomicBool::new(false);
        let r = read_nmea(Cursor::new(GGA.as_bytes().to_vec()), &tx, &stop, None);
        assert!(r.unwrap_err().contains("stopped sending"));
    }

    #[test]
    fn gpsd_client_requests_nmea_and_parses_the_reply() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = [0u8; 128];
            let n = std::io::Read::read(&mut s, &mut req).unwrap();
            let req = String::from_utf8_lossy(&req[..n]).to_string();
            // gpsd greets with a JSON banner, then streams sentences; send them split in two
            // writes to exercise partial-line handling.
            s.write_all(b"{\"class\":\"VERSION\",\"release\":\"3.25\"}\n")
                .unwrap();
            let whole = format!("{GGA}{RMC}");
            let (a, b) = whole.split_at(30);
            s.write_all(a.as_bytes()).unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            s.write_all(b.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            req
        });
        let h = spawn(Source::Gpsd {
            addr: format!("127.0.0.1:{port}"),
        });
        let mut fixes = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fixes < 2 && std::time::Instant::now() < deadline {
            if let Ok(Event::Fix(f)) = h.rx.recv_timeout(Duration::from_millis(200)) {
                assert!((f.lat - 48.1173).abs() < 1e-4);
                fixes += 1;
            }
        }
        assert_eq!(fixes, 2);
        h.stop();
        let req = server.join().unwrap();
        assert!(
            req.contains("?WATCH") && req.contains("\"nmea\":true"),
            "{req}"
        );
    }

    #[test]
    fn unreachable_sources_report_errors_instead_of_panicking() {
        let h = spawn(Source::Gpsd {
            addr: "127.0.0.1:9".into(),
        });
        let ev = h.rx.recv_timeout(Duration::from_secs(8)).unwrap();
        assert!(matches!(ev, Event::Error(m) if m.contains("could not reach")));
        let h = spawn(Source::Serial {
            port: "NOT-A-PORT".into(),
            baud: 4800,
        });
        let ev = h.rx.recv_timeout(Duration::from_secs(8)).unwrap();
        assert!(matches!(ev, Event::Error(m) if m.contains("could not open")));
        let h = spawn(Source::NmeaFile {
            path: "/definitely/missing.nmea".into(),
            interval_ms: 0,
        });
        assert!(matches!(
            h.rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Error(_)
        ));
    }
}

#[cfg(all(test, windows))]
mod live {
    use super::*;

    /// Needs Windows location access to be switched on; run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn system_location_smoke() {
        let h = spawn(Source::System);
        for _ in 0..3 {
            match h.rx.recv_timeout(Duration::from_secs(30)) {
                Ok(ev) => println!("{ev:?}"),
                Err(e) => panic!("no event: {e}"),
            }
        }
    }
}
