//! File formats. `.smfmap` is a zip so later versions can add assets (icons,
//! Show Me Fire snapshots) beside `project.json` without breaking old readers.

mod common;
mod csv_io;
mod file;
mod geojson;
mod gpx;
mod kml;
mod project_file;
mod shp;
mod superoverlay;

pub use common::{Imported, ImportedFeature, apply_import};
pub use csv_io::{read_csv, write_csv};
pub use file::{
    ExportReport, Format, IMPORT_EXTENSIONS, export_bytes, export_path, import_bytes, import_path,
};
pub use geojson::{read_geojson, ring_outlines, write_geojson};
pub use gpx::{read_gpx, write_gpx};
pub use kml::{read_kml, read_kmz, write_kml, write_kmz};
pub use project_file::{IoError, load_project, read_project, save_project, write_project};
pub use shp::{read_shapefile, write_shapefiles};
pub use superoverlay::{image_ext, write_superoverlay};
