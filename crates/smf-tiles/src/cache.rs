use rusqlite::{Connection, OptionalExtension, params};
use smf_geo::TileId;
use std::{path::Path, sync::Mutex};

/// One SQLite file per source in MBTiles layout (TMS rows) so the same file
/// can later double as an offline area. A folder of millions of tiny files is
/// slow on NTFS and painful to delete, hence SQLite.
pub struct DiskCache {
    conn: Mutex<Connection>,
}

impl DiskCache {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        // Several workers open the same file at once. The busy timeout must come first: without it
        // the WAL pragma below fails instantly with SQLITE_BUSY when another connection is initializing.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        // Persistent once set, so a lost race here is harmless.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tiles (
                zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER,
                tile_data BLOB, fetched_at INTEGER, last_access INTEGER,
                PRIMARY KEY (zoom_level, tile_column, tile_row));",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get(&self, t: TileId) -> Option<Vec<u8>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT tile_data FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
            params![t.z, t.x, t.tms_y()],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// The tile and how many seconds ago it was fetched.
    pub fn get_with_age(&self, t: TileId) -> Option<(Vec<u8>, u64)> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT tile_data, fetched_at FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
            params![t.z, t.x, t.tms_y()],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
        .map(|(data, at)| (data, (now - at.unwrap_or(0)).max(0) as u64))
    }

    /// Test helper: pretend every tile was fetched `secs` seconds ago.
    #[cfg(test)]
    pub fn backdate_all(&self, secs: i64) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE tiles SET fetched_at = fetched_at - ?1",
            params![secs],
        )
        .unwrap();
    }

    pub fn put(&self, t: TileId, data: &[u8]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let c = self.conn.lock().unwrap();
        if let Err(e) = c.execute(
            "INSERT OR REPLACE INTO tiles VALUES (?1,?2,?3,?4,?5,?5)",
            params![t.z, t.x, t.tms_y(), data, now],
        ) {
            tracing::warn!("cache write failed: {e}");
        }
    }

    pub fn count(&self) -> u64 {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT COUNT(*) FROM tiles", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_threads_can_open_the_same_new_cache_at_once() {
        let dir = std::env::temp_dir().join(format!("smf-cache-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("race.sqlite");
        let handles: Vec<_> = (0..12)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let c = DiskCache::open(&p).map_err(|e| e.to_string())?;
                    c.put(TileId::new(5, i, 0), b"x");
                    Ok::<_, String>(())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("concurrent open must not fail");
        }
        assert_eq!(DiskCache::open(&path).unwrap().count(), 12);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn roundtrip_and_tms_rows() {
        let dir = std::env::temp_dir().join(format!("smf-cache-test-{}", std::process::id()));
        let cache = DiskCache::open(&dir.join("t.sqlite")).unwrap();
        let t = TileId::new(3, 2, 1);
        assert!(cache.get(t).is_none());
        cache.put(t, b"abc");
        assert_eq!(cache.get(t).unwrap(), b"abc");
        assert_eq!(cache.count(), 1);
        let row: u32 = cache
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT tile_row FROM tiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(row, 6); // MBTiles stores TMS rows
        drop(cache);
        let _ = std::fs::remove_dir_all(dir);
    }
}
