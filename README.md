# ShowMeFire Field

A native Rust (wgpu + egui) desktop mapping app in the spirit of CalTopo, with Show Me Fire
data built in. It is a separate repo from the other ShowMeFire folders (they are intentionally
not a monorepo).

## What it does

- **Maps**: USGS Topo / Imagery / Imagery+Topo, Esri World Imagery, OpenTopoMap, OSM; smooth pan,
  zoom-to-cursor, rotation, inertia; coordinates in DD / DMS / UTM; geodesic scale bar.
- **3D terrain**: quadtree-LOD terrain draped with any basemap or overlay, hillshade, slope-angle
  shading (degrees or fire-grade percent presets), vertical exaggeration, elevation readout and
  elevation profiles.
- **Drawing and markup**: markers, lines, polygons, freehand, range rings, labels, measure
  (geodesic length and area), folders, styles, snapping, vertex editing, unlimited undo/redo,
  `.smfmap` project files.
- **Import / export**: GeoJSON (incl. CalTopo folders), GPX, KML/KMZ, Shapefile (geographic, UTM,
  Web Mercator `.prj`), CSV waypoints; drag and drop.
- **Print**: scale-accurate PDF maps (Letter to A2, any scale, up to 300 dpi) with UTM or
  lat/lon grid, scale bars, north arrow, declination, legend and credit line; georeferenced
  GeoTIFF export (EPSG:3857).
- **Offline**: download an area (MBTiles, standard schema) with pause/resume/cancel; work-offline
  mode; the app refuses to bulk-download sources whose terms forbid it.
- **Custom layers**: XYZ/TMS URLs, WMS, local MBTiles, local GeoTIFF; overlays with opacity in 2D
  and draped in 3D.
- **Terrain analysis**: slope angle and aspect under the pointer, vertical gain and loss on measured
  lines and elevation profiles.
- **Snap to trails**: while drawing lines and polygons, clicks follow the OpenStreetMap trail network
  (routed along trails, not straight across).
- **Fire history and snow gauges**: NIFC historical fire perimeters and NRCS SNOTEL snow readings,
  fetched for the visible area; click a perimeter or gauge for details.
- **Google Earth**: export the view as a KML SuperOverlay (KMZ) that loads detail as you zoom.
- **GPS navigation**: position from the Windows location service, a serial NMEA device, gpsd or a
  replayed NMEA log; follow mode, track recording (saved as a line), and distance/bearing to the
  selected feature.
- **Live tracking**: Garmin inReach MapShare feeds and APRS-IS stations (read-only) as labelled
  markers with a short trail and the age of the last report.
- **Team collaboration**: sign in to a Show Me Fire team (join code), share a map that everyone
  edits live, and optionally share your GPS position. Needs the `/api/field/v1` service from the
  Show Me Fire API (see that repo's `docs/field_collab.md`).
- **what3words** (needs your own API key): type `///word.word.word` in the toolbar to fly to the
  3 m square, show three-word addresses in the cursor readout (the `///` coordinate mode), and
  switch on a 3 m grid layer when zoomed in. Enter the key under what3words > API key; it is stored
  unencrypted in the app's settings folder. The convert and grid endpoints are not on what3words'
  free plan, and everything is online-only (the word list is proprietary).
- **Show Me Fire data (on by default)**: live fire incidents, incident footprints, detections,
  events, burn bans and fire-weather alerts; fire danger, temperature, humidity, wind, fuel
  moisture, precipitation and spread-rate rasters; point conditions under the cursor; the
  72-hour forecast with a time slider (when a run is published). Layers refresh in the
  background, keep a saved copy for offline use, print, and can be copied into the map.

## Build and run

```
cargo run -p smf-app                  # debug
cargo run --release -p smf-app        # optimized (23 MB binary)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p xtask -- check-shaders   # validate the WGSL shaders
```

The Show Me Fire server is `https://api.showmefire.org`; set `SMF_API_BASE=http://localhost:8000`
to use a local API (reuse an already running dev server rather than starting another one).

Windows is the primary development platform; macOS and Linux are built in CI but have had less
hands-on testing.

## Layout

| crate | role |
|---|---|
| `smf-geo` | Mercator and tile math, geodesy, 2D camera, coordinate formats (no IO, no GPU) |
| `smf-tiles` | tile sources, SQLite cache with TTL, MBTiles, GeoTIFF source, offline downloader |
| `smf-terrain` | elevation decoding, 3D camera, frustum/LOD selection, profiles (pure logic) |
| `smf-render` | wgpu renderer: 2D tiles, draped terrain, hillshade/slope, overlays |
| `smf-features` | project model, drawing tools, undo/redo, picking and snapping |
| `smf-io` | GeoJSON, GPX, KML/KMZ, Shapefile, CSV, `.smfmap` |
| `smf-print` | page layout, PDF writer, scale-accurate rendering, GeoTIFF export |
| `smf-data` | Show Me Fire client, layers, background refresher, forecast, snapshots |
| `smf-w3w` | what3words v3 client and three-word parsing |
| `smf-ext` | NIFC fire-history and SNOTEL snow-gauge fetching |
| `smf-route` | OpenStreetMap trail graph and routing |
| `smf-gps` | NMEA parsing and GPS sources (serial, gpsd, replay, Windows location) |
| `smf-live` | inReach MapShare and APRS-IS tracking |
| `smf-sync` | team collaboration client, sync engine and workers |
| `smf-app` | the egui application |

Only `smf-app` depends on egui; tokio is not used (blocking HTTP on worker threads).

## Verification hooks

Visual checks use the app's own rendering, never an OS screen capture (that records other windows):
`SMF_SCREENSHOT=out.png` (+`SMF_SCREENSHOT_SECS`), `SMF_VIEW=lng,lat,zoom`, `SMF_CAM=...` (3D),
`SMF_DEMO=1`, `SMF_IMPORT=a;b`, `SMF_DOWNLOAD=...`, `SMF_OFFLINE=1`, `SMF_LAYER=file[@opacity]`,
`SMF_PRINT=out.pdf[,scale,dpi]` (+`SMF_PRINT_DELAY`), `SMF_GEOTIFF=out.tif[,zoom,half_span]`,
`SMF_W3W_SEARCH=a.b.c`/`SMF_W3W_GRID=1`/`SMF_W3W_HOVER=lng,lat` (+`W3W_API_KEY`, `SMF_W3W_BASE`), `SMF_EXT=history,snow`, `SMF_TRAIL_ROUTE=...` (+`SMF_TRAILS_FILE`), `SMF_EARTH=out.kmz[,zoom,levels]`, `SMF_GPS=...`, `SMF_GPS_RECORD=1`, `SMF_LIVE=...`, `SMF_TEAM=url|code|name[|map]`, `SMF_INFO=<layer>`, `SMF_COND=lng,lat`, `SMF_FORECAST=<hours>`, `SMF_ABOUT=1`. Hooks that set a
non-interactive run suppress modal dialogs.

## Known limits

- Printed scale is exact at the map centre; Web Mercator cannot be exact in both directions on an
  ellipsoid, so distances are within about 0.2-0.4% depending on latitude.
- PDF text uses the standard Helvetica fonts (Latin-1 plus common punctuation).
- Shapefile import supports geographic, UTM (metres) and Web Mercator; other projections are refused
  with a message instead of being misplaced.
- what3words was verified against a local mock of the documented v3 API, not the live service;
  the grid is not included in printed maps.
- Team collaboration is last-writer-wins per feature, shares features (not folders), and needs the
  server; there is no offline editing queue beyond retrying while the app is open. Teammates' changes
  arrive within about a second. Sign-ins, inReach passwords and the what3words key are saved
  unencrypted in the settings folder.
- Snap to trails depends on the public Overpass servers, which are sometimes busy (the app retries
  and tries mirrors). APRS Mic-E packets are not decoded. inReach parsing follows Garmin's documented
  feed fields but was not checked against a live feed. The KMZ export was validated structurally,
  not opened in Google Earth. Serial GPS was tested with a replayed log and a gpsd test server, not
  a hardware receiver.
- Vector basemaps (Protomaps) and WASM/web builds are not implemented.
- Live incident markers are not clustered when zoomed far out.
- Installer packaging (`[package.metadata.packager]`) and the release workflow are configured but
  have not been exercised on clean machines; signed installers are not produced.
- Update checks are opt-in (`SMF_UPDATE_URL`).
