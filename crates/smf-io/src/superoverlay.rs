//! KML SuperOverlay export: a map-tile pyramid as a single KMZ that Google Earth loads
//! progressively (each tile's KML links to its four children, shown once zoomed in far enough).
//!
//! Layout inside the zip: `doc.kml` links the top-level tiles; every tile `z/x/y` has
//! `z/x/y.kml` (its GroundOverlay plus links to children) and the image `z/x/y.png|jpg`.

use crate::IoError;
use smf_geo::{TileId, mercator_to_lnglat, tile_bounds_mercator, tile_range};
use std::collections::HashSet;
use std::io::{Seek, Write};

/// A tile is displayed once it covers at least this many screen pixels, and its children
/// take over when it would cover `CHILD_LOD` (twice its pixel size).
const MIN_LOD: u32 = 128;
const CHILD_LOD: u32 = 512;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// File extension for tile image bytes, by magic number; None for anything else.
pub fn image_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

/// (north, south, east, west) in degrees for a tile.
fn latlon(t: TileId) -> (f64, f64, f64, f64) {
    let b = tile_bounds_mercator(t);
    let (sw, ne) = (
        mercator_to_lnglat(b[0], b[1]),
        mercator_to_lnglat(b[2], b[3]),
    );
    (ne.lat, sw.lat, ne.lng, sw.lng)
}

fn region(t: TileId, min_lod: u32, max_lod: i32) -> String {
    let (n, s, e, w) = latlon(t);
    format!(
        "<Region><LatLonAltBox><north>{n:.8}</north><south>{s:.8}</south><east>{e:.8}</east><west>{w:.8}</west></LatLonAltBox><Lod><minLodPixels>{min_lod}</minLodPixels><maxLodPixels>{max_lod}</maxLodPixels></Lod></Region>"
    )
}

fn child_ids(t: TileId) -> [TileId; 4] {
    let (z, x, y) = (t.z + 1, t.x * 2, t.y * 2);
    [
        TileId::new(z, x, y),
        TileId::new(z, x + 1, y),
        TileId::new(z, x, y + 1),
        TileId::new(z, x + 1, y + 1),
    ]
}

/// Write the pyramid for `bounds` (min_lng, min_lat, max_lng, max_lat) over `min_zoom..=max_zoom`.
/// `tile` supplies image bytes (PNG or JPEG) or None where a tile doesn't exist. Returns the
/// number of tiles written.
pub fn write_superoverlay<W: Write + Seek>(
    w: W,
    name: &str,
    bounds: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
    tile: &mut dyn FnMut(TileId) -> Option<Vec<u8>>,
) -> Result<usize, IoError> {
    if min_zoom > max_zoom {
        return Err(IoError::Format("minimum zoom is above maximum zoom".into()));
    }
    let mut zip = zip::ZipWriter::new(w);
    // Tile images are already compressed.
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default();

    let mut have: HashSet<TileId> = HashSet::new();
    let mut ext_of: std::collections::HashMap<TileId, &'static str> = Default::default();
    for z in min_zoom..=max_zoom {
        let (xr, yr) = tile_range(bounds, z);
        for x in xr {
            for y in yr.clone() {
                let id = TileId::new(z, x, y);
                let Some(data) = tile(id) else { continue };
                let Some(ext) = image_ext(&data) else {
                    continue;
                };
                zip.start_file(format!("{z}/{x}/{y}.{ext}"), stored)?;
                zip.write_all(&data)?;
                have.insert(id);
                ext_of.insert(id, ext);
            }
        }
    }
    if have.is_empty() {
        return Err(IoError::Format(
            "no map tiles were available for that area and zoom range".into(),
        ));
    }

    let mut roots: Vec<TileId> = have.iter().copied().filter(|t| t.z == min_zoom).collect();
    roots.sort();
    // A pyramid whose top zoom has gaps still has to be reachable: tiles whose parent is
    // missing become roots too.
    let mut orphans: Vec<TileId> = have
        .iter()
        .copied()
        .filter(|t| t.z > min_zoom && t.parent().is_none_or(|p| !have.contains(&p)))
        .collect();
    orphans.sort();
    roots.extend(orphans);

    let mut doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\"><Document><name>{}</name>",
        esc(name)
    );
    for t in &roots {
        doc.push_str(&format!(
            "<NetworkLink><name>{}/{}/{}</name>{}<Link><href>{}/{}/{}.kml</href><viewRefreshMode>onRegion</viewRefreshMode></Link></NetworkLink>",
            t.z, t.x, t.y, region(*t, MIN_LOD, -1), t.z, t.x, t.y
        ));
    }
    doc.push_str("</Document></kml>\n");
    zip.start_file("doc.kml", deflated)?;
    zip.write_all(doc.as_bytes())?;

    let mut ids: Vec<TileId> = have.iter().copied().collect();
    ids.sort();
    for t in &ids {
        let children: Vec<TileId> = child_ids(*t)
            .into_iter()
            .filter(|c| have.contains(c))
            .collect();
        let (n, s, e, wst) = latlon(*t);
        let mut k = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\"><Document><name>{}/{}/{}</name>{}",
            t.z,
            t.x,
            t.y,
            region(
                *t,
                MIN_LOD,
                if children.is_empty() {
                    -1
                } else {
                    CHILD_LOD as i32
                }
            )
        );
        k.push_str(&format!(
            "<GroundOverlay><drawOrder>{}</drawOrder><Icon><href>{}.{}</href></Icon><LatLonBox><north>{n:.8}</north><south>{s:.8}</south><east>{e:.8}</east><west>{wst:.8}</west></LatLonBox></GroundOverlay>",
            t.z, t.y, ext_of[t]
        ));
        for c in children {
            k.push_str(&format!(
                "<NetworkLink><name>{}/{}/{}</name>{}<Link><href>../../{}/{}/{}.kml</href><viewRefreshMode>onRegion</viewRefreshMode></Link></NetworkLink>",
                c.z, c.x, c.y, region(c, MIN_LOD, -1), c.z, c.x, c.y
            ));
        }
        k.push_str("</Document></kml>\n");
        zip.start_file(format!("{}/{}/{}.kml", t.z, t.x, t.y), deflated)?;
        zip.write_all(k.as_bytes())?;
    }
    zip.finish()?;
    Ok(have.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 1, 2, 3];
    const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9];

    fn build(bounds: [f64; 4], zmin: u8, zmax: u8, missing: &[TileId]) -> (Vec<u8>, usize) {
        let mut buf = Cursor::new(Vec::new());
        let mut gen_tile = |t: TileId| -> Option<Vec<u8>> {
            if missing.contains(&t) {
                None
            } else if t.z.is_multiple_of(2) {
                Some(PNG.to_vec())
            } else {
                Some(JPG.to_vec())
            }
        };
        let n = write_superoverlay(&mut buf, "Test & <map>", bounds, zmin, zmax, &mut gen_tile)
            .unwrap();
        (buf.into_inner(), n)
    }

    fn entries(zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>) -> Vec<String> {
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect()
    }

    fn read(zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> String {
        let mut s = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    /// Resolve `href` against the directory of the KML that contains it.
    fn resolve(from: &str, href: &str) -> String {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for seg in href.split('/') {
            match seg {
                ".." => {
                    parts.pop();
                }
                s => parts.push(s),
            }
        }
        parts.join("/")
    }

    #[test]
    fn every_link_in_the_package_resolves() {
        // Missouri-ish area over z8..z10.
        let (bytes, n) = build([-92.6, 38.3, -91.9, 38.9], 8, 10, &[]);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names = entries(&mut zip);
        let set: HashSet<&str> = names.iter().map(String::as_str).collect();
        let images = names
            .iter()
            .filter(|n| n.ends_with(".png") || n.ends_with(".jpg"))
            .count();
        assert_eq!(images, n);
        assert!(set.contains("doc.kml"));
        let mut kmls = 0;
        for k in names.iter().filter(|n| n.ends_with(".kml")) {
            kmls += 1;
            let text = read(&mut zip, k);
            let d = roxmltree::Document::parse(&text).unwrap_or_else(|e| panic!("{k}: {e}"));
            for h in d.descendants().filter(|n| n.has_tag_name("href")) {
                let target = resolve(k, h.text().unwrap());
                assert!(
                    set.contains(target.as_str()),
                    "{k} links to missing {target}"
                );
            }
        }
        assert_eq!(kmls, n + 1, "one KML per tile plus doc.kml");
    }

    #[test]
    fn tile_geometry_and_detail_levels() {
        let (bytes, _) = build([-92.6, 38.3, -91.9, 38.9], 8, 9, &[]);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names = entries(&mut zip);
        let root = names
            .iter()
            .find(|n| n.starts_with("8/") && n.ends_with(".kml"))
            .unwrap()
            .clone();
        let d_text = read(&mut zip, &root);
        let d = roxmltree::Document::parse(&d_text).unwrap();
        let f = |tag: &str| -> f64 {
            d.descendants()
                .find(|n| n.has_tag_name("LatLonBox"))
                .unwrap()
                .children()
                .find(|c| c.has_tag_name(tag))
                .unwrap()
                .text()
                .unwrap()
                .parse()
                .unwrap()
        };
        let parts: Vec<u32> = root
            .trim_end_matches(".kml")
            .split('/')
            .map(|p| p.parse().unwrap())
            .collect();
        let t = TileId::new(parts[0] as u8, parts[1], parts[2]);
        let b = tile_bounds_mercator(t);
        let (sw, ne) = (
            mercator_to_lnglat(b[0], b[1]),
            mercator_to_lnglat(b[2], b[3]),
        );
        assert!((f("west") - sw.lng).abs() < 1e-6 && (f("east") - ne.lng).abs() < 1e-6);
        assert!((f("south") - sw.lat).abs() < 1e-6 && (f("north") - ne.lat).abs() < 1e-6);
        // A tile with children hands over at 512 px; leaves stay visible forever (-1).
        assert!(
            d_text.contains("<maxLodPixels>512</maxLodPixels>"),
            "{d_text}"
        );
        let leaf = names
            .iter()
            .find(|n| n.starts_with("9/") && n.ends_with(".kml"))
            .unwrap()
            .clone();
        assert!(read(&mut zip, &leaf).contains("<maxLodPixels>-1</maxLodPixels>"));
        // Image extension follows the bytes (even zooms PNG, odd zooms JPEG) and is what the KML cites.
        assert!(read(&mut zip, &root).contains(&format!("<href>{}.png</href>", parts[2])));
        assert!(read(&mut zip, &leaf).contains(".jpg</href>"));
    }

    #[test]
    fn missing_tiles_and_bad_input() {
        // Drop one z9 tile: its parent simply doesn't link to it, and links still resolve.
        let (full, n_full) = build([-92.6, 38.3, -91.9, 38.9], 8, 9, &[]);
        let any9 = {
            let mut z = zip::ZipArchive::new(Cursor::new(full)).unwrap();
            let n = entries(&mut z)
                .into_iter()
                .find(|n| {
                    n.starts_with("9/") && n.ends_with(".png")
                        || n.starts_with("9/") && n.ends_with(".jpg")
                })
                .unwrap();
            let p: Vec<u32> = n
                .split('.')
                .next()
                .unwrap()
                .split('/')
                .map(|x| x.parse().unwrap())
                .collect();
            TileId::new(9, p[1], p[2])
        };
        let (bytes, n) = build([-92.6, 38.3, -91.9, 38.9], 8, 9, &[any9]);
        assert_eq!(n, n_full - 1);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let set: HashSet<String> = entries(&mut zip).into_iter().collect();
        for k in set.iter().filter(|n| n.ends_with(".kml")) {
            let text = read(&mut zip, k);
            let d = roxmltree::Document::parse(&text).unwrap();
            for h in d.descendants().filter(|n| n.has_tag_name("href")) {
                assert!(set.contains(&resolve(k, h.text().unwrap())), "{k}");
            }
        }
        // Names are escaped, so odd titles can't break the XML.
        let doc = read(&mut zip, "doc.kml");
        assert!(doc.contains("Test &amp; &lt;map&gt;") && roxmltree::Document::parse(&doc).is_ok());
        // Nothing to export, or reversed zooms, is an error rather than an empty file.
        let mut none = |_: TileId| None;
        assert!(
            write_superoverlay(
                Cursor::new(Vec::new()),
                "x",
                [0.0, 0.0, 1.0, 1.0],
                3,
                4,
                &mut none
            )
            .is_err()
        );
        let mut any = |_: TileId| Some(PNG.to_vec());
        assert!(
            write_superoverlay(
                Cursor::new(Vec::new()),
                "x",
                [0.0, 0.0, 1.0, 1.0],
                5,
                4,
                &mut any
            )
            .is_err()
        );
        // Non-image bytes are skipped, not packaged.
        let mut junk = |_: TileId| Some(b"<html>error</html>".to_vec());
        assert!(
            write_superoverlay(
                Cursor::new(Vec::new()),
                "x",
                [0.0, 0.0, 1.0, 1.0],
                3,
                4,
                &mut junk
            )
            .is_err()
        );
    }
}
