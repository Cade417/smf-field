use smf_features::{FORMAT_VERSION, Project};
use std::io::{Read, Seek, Write};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a valid .smfmap archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("project.json is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("project was saved by a newer version (format {found}, this app reads up to {max})")]
    TooNew { found: u32, max: u32 },
}

const ENTRY: &str = "project.json";

pub fn write_project<W: Write + Seek>(project: &Project, w: W) -> Result<(), IoError> {
    let mut zip = zip::ZipWriter::new(w);
    zip.start_file(ENTRY, zip::write::SimpleFileOptions::default())?;
    zip.write_all(&serde_json::to_vec_pretty(project)?)?;
    zip.finish()?;
    Ok(())
}

pub fn read_project<R: Read + Seek>(r: R) -> Result<Project, IoError> {
    let mut zip = zip::ZipArchive::new(r)?;
    let mut json = Vec::new();
    zip.by_name(ENTRY)?.read_to_end(&mut json)?;
    // Check the version before full deserialization so a newer file reports a clear
    // upgrade message instead of an opaque missing-field error.
    let v: serde_json::Value = serde_json::from_slice(&json)?;
    let found = v
        .get("format_version")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    if found > FORMAT_VERSION {
        return Err(IoError::TooNew {
            found,
            max: FORMAT_VERSION,
        });
    }
    Ok(serde_json::from_value(v)?)
}

/// Write to a temp file then rename so a crash mid-save never corrupts the existing map.
pub fn save_project(project: &Project, path: &Path) -> Result<(), IoError> {
    let tmp = path.with_extension("smfmap.tmp");
    write_project(project, std::fs::File::create(&tmp)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_project(path: &Path) -> Result<Project, IoError> {
    read_project(std::fs::File::open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::{Editor, Geometry, SavedView};
    use smf_geo::LngLat;
    use std::io::Cursor;

    fn sample() -> Project {
        let mut p = Project::default();
        let folder = p.add_folder("Ops");
        let mut e = Editor::new(p);
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("Command Post".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-92.0, 38.0),
                LngLat::new(-91.9, 38.0),
                LngLat::new(-91.9, 38.1),
            ]),
            None,
        );
        e.add(
            Geometry::RangeRing {
                center: LngLat::new(-92.0, 38.0),
                radii_m: vec![500.0, 1500.5],
            },
            None,
        );
        let mut f = e.project.features[0].clone();
        f.folder = Some(folder);
        f.notes = "Staging area, gate code in radio log".into();
        e.replace(f);
        let mut out = e.project;
        out.view = Some(SavedView {
            center: LngLat::new(-92.0, 38.0),
            zoom: 11.5,
            angle: 0.25,
        });
        out.basemap = Some("usgs-topo".into());
        out
    }

    #[test]
    fn roundtrips_identically_in_memory() {
        let p = sample();
        let mut buf = Cursor::new(Vec::new());
        write_project(&p, &mut buf).unwrap();
        buf.set_position(0);
        assert_eq!(read_project(buf).unwrap(), p);
    }

    #[test]
    fn roundtrips_through_disk_and_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!("smf-io-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.smfmap");
        let p = sample();
        save_project(&p, &path).unwrap();
        save_project(&p, &path).unwrap(); // overwrite existing
        assert_eq!(load_project(&path).unwrap(), p);
        assert!(!path.with_extension("smfmap.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_newer_format_with_clear_error() {
        let mut p = sample();
        p.format_version = FORMAT_VERSION + 1;
        let mut buf = Cursor::new(Vec::new());
        write_project(&p, &mut buf).unwrap();
        buf.set_position(0);
        assert!(matches!(read_project(buf), Err(IoError::TooNew { .. })));
    }

    #[test]
    fn rejects_garbage() {
        assert!(read_project(Cursor::new(b"not a zip".to_vec())).is_err());
    }
}
