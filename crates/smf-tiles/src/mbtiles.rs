//! MBTiles (https://github.com/mapbox/mbtiles-spec) read/write. Used for offline areas
//! and for opening MBTiles made by other tools. Rows are stored TMS-flipped per the spec.

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use smf_geo::TileId;
use std::path::Path;
use std::sync::Mutex;

/// Commit a write transaction after this many tiles: one fsync per tile would make downloads crawl.
const COMMIT_EVERY: u32 = 200;

struct Inner {
    conn: Connection,
    pending: u32,
}

pub struct Mbtiles {
    inner: Mutex<Inner>,
    read_only: bool,
}

impl Mbtiles {
    /// Open for writing, creating the file and the standard schema if needed.
    pub fn create(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (name TEXT, value TEXT);
             CREATE UNIQUE INDEX IF NOT EXISTS metadata_name ON metadata (name);
             CREATE TABLE IF NOT EXISTS tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);
             CREATE UNIQUE INDEX IF NOT EXISTS tile_index ON tiles (zoom_level, tile_column, tile_row);",
        )?;
        Ok(Self {
            inner: Mutex::new(Inner { conn, pending: 0 }),
            read_only: false,
        })
    }

    /// Open an existing file without modifying it (offline areas, third-party files).
    pub fn open_read_only(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Fails fast if this isn't an MBTiles database at all.
        conn.query_row("SELECT 1 FROM tiles LIMIT 1", [], |_| Ok(()))
            .optional()?;
        Ok(Self {
            inner: Mutex::new(Inner { conn, pending: 0 }),
            read_only: true,
        })
    }

    pub fn set_meta(&self, name: &str, value: &str) -> rusqlite::Result<()> {
        assert!(!self.read_only, "set_meta on a read-only MBTiles");
        let g = self.inner.lock().unwrap();
        g.conn.execute(
            "INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)",
            params![name, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, name: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row(
                "SELECT value FROM metadata WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn get(&self, t: TileId) -> Option<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row(
                "SELECT tile_data FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
                params![t.z, t.x, t.tms_y()],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn has(&self, t: TileId) -> bool {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row(
                "SELECT 1 FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
                params![t.z, t.x, t.tms_y()],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn put(&self, t: TileId, data: &[u8]) -> rusqlite::Result<()> {
        assert!(!self.read_only, "put on a read-only MBTiles");
        let mut g = self.inner.lock().unwrap();
        if g.pending == 0 {
            g.conn.execute_batch("BEGIN")?;
        }
        g.conn.execute(
            "INSERT OR REPLACE INTO tiles (zoom_level, tile_column, tile_row, tile_data) VALUES (?1,?2,?3,?4)",
            params![t.z, t.x, t.tms_y(), data],
        )?;
        g.pending += 1;
        if g.pending >= COMMIT_EVERY {
            g.conn.execute_batch("COMMIT")?;
            g.pending = 0;
        }
        Ok(())
    }

    /// Commit any open write transaction. Always call before dropping a file you wrote.
    pub fn flush(&self) -> rusqlite::Result<()> {
        let mut g = self.inner.lock().unwrap();
        if g.pending > 0 {
            g.conn.execute_batch("COMMIT")?;
            g.pending = 0;
        }
        Ok(())
    }

    pub fn count(&self) -> u64 {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row("SELECT COUNT(*) FROM tiles", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    }

    pub fn zoom_range(&self) -> Option<(u8, u8)> {
        let g = self.inner.lock().unwrap();
        g.conn
            .query_row(
                "SELECT MIN(zoom_level), MAX(zoom_level) FROM tiles",
                [],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .ok()
            .and_then(|(a, b)| Some((a? as u8, b? as u8)))
    }
}

impl Drop for Mbtiles {
    fn drop(&mut self) {
        if !self.read_only {
            let _ = self.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("smf-mbt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("t.mbtiles")
    }

    #[test]
    fn roundtrip_metadata_tms_rows_and_reopen_read_only() {
        let path = tmp("rt");
        {
            let m = Mbtiles::create(&path).unwrap();
            m.set_meta("name", "Test").unwrap();
            m.set_meta("name", "Test 2").unwrap(); // replaced, not duplicated
            m.put(TileId::new(3, 2, 1), b"abc").unwrap();
            m.put(TileId::new(4, 0, 0), b"def").unwrap();
            assert!(m.has(TileId::new(3, 2, 1)) && !m.has(TileId::new(3, 2, 2)));
            assert_eq!(m.count(), 2);
            assert_eq!(m.zoom_range(), Some((3, 4)));
        } // drop commits
        let r = Mbtiles::open_read_only(&path).unwrap();
        assert_eq!(r.meta("name").as_deref(), Some("Test 2"));
        assert_eq!(r.get(TileId::new(3, 2, 1)).unwrap(), b"abc");
        assert!(r.get(TileId::new(9, 9, 9)).is_none());
        // The spec stores TMS rows: z3 y1 -> row 6.
        let row: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT tile_row FROM tiles WHERE zoom_level=3", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(row, 6);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn many_puts_commit_in_batches_and_flush_persists_the_tail() {
        let path = tmp("batch");
        let m = Mbtiles::create(&path).unwrap();
        for i in 0..(COMMIT_EVERY + 37) {
            m.put(TileId::new(12, i, 5), &[1, 2, 3]).unwrap();
        }
        m.flush().unwrap();
        let r = Mbtiles::open_read_only(&path).unwrap();
        assert_eq!(r.count(), (COMMIT_EVERY + 37) as u64);
        drop(m);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn read_only_rejects_non_mbtiles_files() {
        let path = tmp("bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"this is not a database").unwrap();
        assert!(Mbtiles::open_read_only(&path).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
