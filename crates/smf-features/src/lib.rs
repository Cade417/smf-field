//! Document model and editing logic. No UI, IO or GPU: everything here is
//! deterministic so tools, undo/redo and picking are unit-testable.

mod editor;
mod model;
mod pick;

pub use editor::{Command, Editor, Tool, simplify};
pub use model::{
    FORMAT_VERSION, Feature, Folder, Geometry, Id, Project, SavedView, Style, default_radii,
};
pub use pick::{Hit, pick, screen_paths, snap};
