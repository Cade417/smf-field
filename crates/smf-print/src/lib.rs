//! Printing: scale-accurate page layout, a small PDF writer, and raster/GeoTIFF export.

mod compose;
mod job;
mod layout;
mod pdf;
mod raster;

pub use compose::{
    Composed, LegendOverlay, ScaleBar, compose_pdf, deg_interval, scale_bar, utm_interval, utm_zone,
};
pub use job::{GeoTiffTask, PdfOutput, PdfTask, STANDARD_SCALES, fit_scale};
#[cfg(not(target_arch = "wasm32"))]
pub use job::{export_geotiff, print_to_pdf};
pub use layout::{
    FOOTER_H_MM, GridKind, Layout, LayoutError, Paper, PrintProjection, PrintSpec, RectMm,
    ellipsoid_scale_factor, nice_bar,
};
pub use pdf::{Align, Font, Page, Rgb, text_width_mm};
pub use raster::{
    GeoTiffRegion, MAX_PIXELS, PrintError, RegionJob, RenderLayers, Rendered, Step, encode_jpeg,
    geotiff_bytes, utc_date,
};
#[cfg(not(target_arch = "wasm32"))]
pub use raster::{render_region, write_geotiff};
