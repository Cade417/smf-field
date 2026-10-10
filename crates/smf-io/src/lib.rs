//! File formats. `.smfmap` is a zip so later versions can add assets (icons,
//! Show Me Fire snapshots) beside `project.json` without breaking old readers.

mod project_file;

pub use project_file::{IoError, load_project, read_project, save_project, write_project};
