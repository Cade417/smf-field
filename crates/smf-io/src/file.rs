//! Path-based import/export dispatch: picks the format from the file extension.

use crate::{Imported, IoError};
use smf_features::Project;
use std::path::{Path, PathBuf};

/// Refuse absurdly large inputs instead of exhausting memory.
const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    GeoJson,
    Gpx,
    Kml,
    Kmz,
    Shapefile,
    Csv,
}

impl Format {
    pub const ALL: [Format; 6] = [
        Format::GeoJson,
        Format::Gpx,
        Format::Kml,
        Format::Kmz,
        Format::Shapefile,
        Format::Csv,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Format::GeoJson => "GeoJSON",
            Format::Gpx => "GPX",
            Format::Kml => "KML",
            Format::Kmz => "KMZ",
            Format::Shapefile => "Shapefile",
            Format::Csv => "CSV (markers)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::GeoJson => "geojson",
            Format::Gpx => "gpx",
            Format::Kml => "kml",
            Format::Kmz => "kmz",
            Format::Shapefile => "shp",
            Format::Csv => "csv",
        }
    }

    pub fn from_path(p: &Path) -> Option<Format> {
        match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "geojson" | "json" => Some(Format::GeoJson),
            "gpx" => Some(Format::Gpx),
            "kml" => Some(Format::Kml),
            "kmz" => Some(Format::Kmz),
            "shp" => Some(Format::Shapefile),
            "csv" | "txt" | "tsv" => Some(Format::Csv),
            _ => None,
        }
    }
}

/// Extensions accepted by the import dialog and drag-and-drop.
pub const IMPORT_EXTENSIONS: &[&str] = &[
    "geojson", "json", "gpx", "kml", "kmz", "shp", "csv", "tsv", "txt",
];

fn read_limited(path: &Path) -> Result<String, IoError> {
    if std::fs::metadata(path)?.len() > MAX_IMPORT_BYTES {
        return Err(IoError::Format(format!(
            "{} is larger than the {} MB import limit",
            path.display(),
            MAX_IMPORT_BYTES / 1024 / 1024
        )));
    }
    let bytes = std::fs::read(path)?;
    String::from_utf8(bytes)
        .map_err(|_| IoError::Format(format!("{} is not UTF-8 text", path.display())))
}

pub fn import_path(path: &Path) -> Result<Imported, IoError> {
    let fmt = Format::from_path(path).ok_or_else(|| {
        IoError::Format(format!(
            "Don't know how to import \"{}\" (supported: GeoJSON, GPX, KML, KMZ, Shapefile, CSV).",
            path.display()
        ))
    })?;
    let bad = |what: &str, e: &dyn std::fmt::Display| {
        IoError::Format(format!("{what} file is invalid: {e}"))
    };
    match fmt {
        Format::GeoJson => {
            crate::read_geojson(&read_limited(path)?).map_err(|e| bad("GeoJSON", &e))
        }
        Format::Gpx => crate::read_gpx(&read_limited(path)?).map_err(|e| bad("GPX", &e)),
        Format::Kml => crate::read_kml(&read_limited(path)?).map_err(|e| bad("KML", &e)),
        Format::Kmz => crate::read_kmz(std::fs::File::open(path)?),
        Format::Shapefile => crate::read_shapefile(path),
        Format::Csv => crate::read_csv(&read_limited(path)?),
    }
}

/// Import from bytes already in memory (browsers have no file paths). The format comes from
/// `name`'s extension. Shapefiles span several files and are only supported by `import_path`.
pub fn import_bytes(name: &str, bytes: &[u8]) -> Result<Imported, IoError> {
    let fmt = Format::from_path(Path::new(name)).ok_or_else(|| {
        IoError::Format(format!(
            "Don't know how to import \"{name}\" (supported: GeoJSON, GPX, KML, KMZ, CSV)."
        ))
    })?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(IoError::Format(format!(
            "{name} is larger than the {} MB import limit",
            MAX_IMPORT_BYTES / 1024 / 1024
        )));
    }
    let bad = |what: &str, e: &dyn std::fmt::Display| {
        IoError::Format(format!("{what} file is invalid: {e}"))
    };
    let text = || {
        String::from_utf8(bytes.to_vec())
            .map_err(|_| IoError::Format(format!("{name} is not UTF-8 text")))
    };
    match fmt {
        Format::GeoJson => crate::read_geojson(&text()?).map_err(|e| bad("GeoJSON", &e)),
        Format::Gpx => crate::read_gpx(&text()?).map_err(|e| bad("GPX", &e)),
        Format::Kml => crate::read_kml(&text()?).map_err(|e| bad("KML", &e)),
        Format::Kmz => crate::read_kmz(std::io::Cursor::new(bytes)),
        Format::Csv => crate::read_csv(&text()?),
        Format::Shapefile => Err(IoError::Format(
            "Shapefiles are made of several files; import them in the desktop app.".into(),
        )),
    }
}

/// Export to bytes (for browsers, which download the result). Returns the file content and
/// any notes about data that could not be represented. Shapefiles need several files and are
/// only supported by `export_path`.
pub fn export_bytes(project: &Project, fmt: Format) -> Result<(Vec<u8>, Vec<String>), IoError> {
    Ok(match fmt {
        Format::GeoJson => (crate::write_geojson(project).into_bytes(), vec![]),
        Format::Gpx => {
            let (text, w) = crate::write_gpx(project);
            (text.into_bytes(), w)
        }
        Format::Kml => (crate::write_kml(project).into_bytes(), vec![]),
        Format::Kmz => {
            let mut buf = std::io::Cursor::new(Vec::new());
            crate::write_kmz(project, &mut buf)?;
            (buf.into_inner(), vec![])
        }
        Format::Csv => {
            let (text, w) = crate::write_csv(project);
            (text.into_bytes(), w)
        }
        Format::Shapefile => {
            return Err(IoError::Format(
                "Shapefiles are made of several files; export them from the desktop app.".into(),
            ));
        }
    })
}

#[derive(Debug, Default)]
pub struct ExportReport {
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Write to a temp file and rename so a failed export never leaves a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), IoError> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("out")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Export the whole project. For shapefiles, `path` names the base: up to three
/// `<name>_points/_lines/_polygons.shp` sets are written beside it.
pub fn export_path(project: &Project, path: &Path, fmt: Format) -> Result<ExportReport, IoError> {
    let mut report = ExportReport::default();
    match fmt {
        Format::GeoJson => write_atomic(path, crate::write_geojson(project).as_bytes())?,
        Format::Gpx => {
            let (text, w) = crate::write_gpx(project);
            report.warnings = w;
            write_atomic(path, text.as_bytes())?;
        }
        Format::Kml => write_atomic(path, crate::write_kml(project).as_bytes())?,
        Format::Kmz => {
            let mut buf = std::io::Cursor::new(Vec::new());
            crate::write_kmz(project, &mut buf)?;
            write_atomic(path, buf.get_ref())?;
        }
        Format::Csv => {
            let (text, w) = crate::write_csv(project);
            report.warnings = w;
            write_atomic(path, text.as_bytes())?;
        }
        Format::Shapefile => {
            let dir = path.parent().unwrap_or(Path::new("."));
            let base = path.file_stem().and_then(|s| s.to_str()).unwrap_or("map");
            let (files, w) = crate::write_shapefiles(project, dir, base)?;
            report.files = files;
            report.warnings = w;
            return Ok(report);
        }
    }
    report.files.push(path.to_owned());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smf_features::{Editor, Geometry};
    use smf_geo::LngLat;

    fn project() -> Project {
        let mut e = Editor::default();
        e.add(
            Geometry::Marker(LngLat::new(-92.1, 38.5)),
            Some("Pin".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.1)]),
            Some("Line".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-92.0, 38.0),
                LngLat::new(-91.9, 38.0),
                LngLat::new(-91.9, 38.1),
            ]),
            Some("Poly".into()),
        );
        e.project
    }

    #[test]
    fn every_format_survives_export_then_import_by_path() {
        let dir = std::env::temp_dir().join(format!("smf-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = project();
        for fmt in Format::ALL {
            let path = dir.join(format!("out.{}", fmt.extension()));
            let report = export_path(&p, &path, fmt).unwrap();
            assert!(!report.files.is_empty(), "{fmt:?}");
            let mut features = 0;
            for f in &report.files {
                assert_eq!(Format::from_path(f), Some(fmt));
                features += import_path(f).unwrap().features.len();
            }
            // CSV can only carry the marker; everything else carries all three features.
            let expected = if fmt == Format::Csv { 1 } else { 3 };
            assert_eq!(features, expected, "{fmt:?}");
        }
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_extension_and_missing_files_give_clear_errors() {
        let err = import_path(Path::new("map.xyz")).unwrap_err().to_string();
        assert!(err.contains("Don't know how to import"), "{err}");
        assert!(import_path(Path::new("definitely-missing.geojson")).is_err());
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(Format::from_path(Path::new("A.GPX")), Some(Format::Gpx));
        assert_eq!(
            Format::from_path(Path::new("a.JSON")),
            Some(Format::GeoJson)
        );
        assert_eq!(Format::from_path(Path::new("noext")), None);
    }

    #[test]
    fn bytes_import_and_export_round_trip() {
        let p = project();
        for fmt in [
            Format::GeoJson,
            Format::Gpx,
            Format::Kml,
            Format::Kmz,
            Format::Csv,
        ] {
            let (bytes, _warn) = export_bytes(&p, fmt).unwrap();
            let imp = import_bytes(&format!("out.{}", fmt.extension()), &bytes).unwrap();
            assert!(
                !imp.features.is_empty(),
                "{fmt:?} round trip lost everything"
            );
        }
        let err = import_bytes("x.shp", b"").unwrap_err().to_string();
        assert!(err.contains("desktop"), "{err}");
        assert!(export_bytes(&p, Format::Shapefile).is_err());
        assert!(import_bytes("mystery.xyz", b"data").is_err());
        assert!(
            import_bytes("bad.geojson", &[0xFF, 0xFE]).is_err(),
            "not UTF-8 is refused"
        );
    }
}
