//! Show Me Fire data: the HTTP client, layer parsing/styling, a background refresher with an
//! offline cache, the raster layer catalog and forecast runs.

mod catalog;
mod client;
mod layers;
mod snapshot;
mod store;

pub use catalog::{
    Conditions, ForecastLayer, ForecastRun, LegendEntry, RasterLayer, parse_catalog,
    parse_conditions, parse_forecast_run,
};
pub use client::{DEFAULT_BASE, Fetched, SmfClient, default_base, validate_base};
pub use layers::{
    DataError, DataFeature, DataLayerKind, DataStyle, ParsedLayer, links, parse_layer, style_for,
    summary, title_for,
};
pub use snapshot::snapshot_features;
pub use store::{Command, DataCache, Refresher, Update, next_interval, unix_now};
