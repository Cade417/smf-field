//! Composes the printed page: map image, grid, vector markup, scale bar, north arrow,
//! legend, title and the data-source credit line.

use crate::layout::{GridKind, Layout, LayoutError, PrintProjection, PrintSpec, nice_bar};
use crate::pdf::{Align, Font, Page, Rgb, text_width_mm};
use smf_features::{Feature, Geometry, Project};
use smf_geo::{LngLat, destination, format_distance, geodesic_circle};

/// An overlay shown in the legend ("Fire danger (60%)").
#[derive(Debug, Clone)]
pub struct LegendOverlay {
    pub name: String,
    pub opacity: f32,
    /// Swatch colour (live-data layers); None draws a neutral grey.
    pub color: Option<[u8; 3]>,
}

/// (label, the feature it stands for if any, swatch colour for data layers)
type LegendEntry<'a> = (String, Option<&'a Feature>, Option<[u8; 3]>);

pub struct Composed {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleBar {
    pub len_m: f64,
    pub len_mm: f64,
    pub divisions: u32,
}

/// Scale-bar geometry in page millimetres: its drawn length is the ground length divided by the scale.
pub fn scale_bar(len_m: f64, divisions: u32, scale_denominator: f64) -> ScaleBar {
    ScaleBar {
        len_m,
        len_mm: len_m * 1000.0 / scale_denominator,
        divisions,
    }
}

/// Grid interval (metres for UTM, degrees for lat/lon) giving roughly 4-9 lines across the frame.
pub fn utm_interval(frame_ground_m: f64) -> f64 {
    [
        100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0,
        200_000.0, 500_000.0,
    ]
    .into_iter()
    .find(|i| frame_ground_m / i <= 9.0)
    .unwrap_or(1_000_000.0)
}

pub fn deg_interval(frame_span_deg: f64) -> f64 {
    [
        1.0 / 720.0,
        1.0 / 360.0,
        1.0 / 120.0,
        1.0 / 60.0,
        2.0 / 60.0,
        5.0 / 60.0,
        10.0 / 60.0,
        0.5,
        1.0,
        2.0,
        5.0,
        10.0,
    ]
    .into_iter()
    .find(|i| frame_span_deg / i <= 9.0)
    .unwrap_or(20.0)
}

pub fn utm_zone(lng: f64) -> u8 {
    (((lng + 180.0) / 6.0).floor() as i32 + 1).clamp(1, 60) as u8
}

fn to_utm(p: LngLat, zone: u8) -> (f64, f64) {
    let (n, e, _) = utm::to_utm_wgs84(p.lat, p.lng, zone);
    (e, n)
}

fn from_utm(e: f64, n: f64, zone: u8, north: bool) -> Option<LngLat> {
    utm::wsg84_utm_to_lat_lon(e, n, zone, if north { 'N' } else { 'M' })
        .ok()
        .map(|(lat, lng)| LngLat::new(lng, lat))
}

fn deg_label(v: f64, lat: bool, interval: f64) -> String {
    let hemi = match (lat, v >= 0.0) {
        (true, true) => 'N',
        (true, false) => 'S',
        (false, true) => 'E',
        (false, false) => 'W',
    };
    let a = v.abs();
    let d = a.floor();
    let m_total = (a - d) * 60.0;
    let m = (m_total + 1e-6).floor();
    let s = ((m_total - m) * 60.0).round();
    if interval >= 1.0 / 60.0 - 1e-9 {
        if m < 0.5 {
            format!("{d:.0}\u{b0}{hemi}")
        } else {
            format!("{d:.0}\u{b0}{m:02.0}'{hemi}")
        }
    } else {
        format!("{d:.0}\u{b0}{m:02.0}'{s:02.0}\"{hemi}")
    }
}

/// (is_easting, grid value in metres, polyline in page mm)
type GridLine = (bool, f64, Vec<(f64, f64)>);

const GRID_COLOR: Rgb = Rgb(0.15, 0.25, 0.55);

fn width_mm(style_width: f32) -> f64 {
    (style_width as f64 * 0.2).clamp(0.15, 2.5)
}

#[allow(clippy::too_many_arguments)] // drawing helper: position, text style and colour are all independent
fn halo_text(
    page: &mut Page,
    x: f64,
    y: f64,
    size: f64,
    font: Font,
    align: Align,
    s: &str,
    color: Rgb,
) {
    let w = text_width_mm(s, size, font);
    let x0 = match align {
        Align::Left => x,
        Align::Center => x - w / 2.0,
        Align::Right => x - w,
    };
    let h = size * 0.3528 * 1.25;
    page.save();
    page.alpha(0.78);
    page.fill_color(Rgb::WHITE);
    page.rect(x0 - 0.6, y - h * 0.82, w + 1.2, h);
    page.fill();
    page.restore();
    page.fill_color(color);
    page.text(x, y, size, font, align, s);
}

fn draw_features(page: &mut Page, pj: &PrintProjection, project: &Project) {
    for f in project.features.iter().filter(|f| project.is_visible(f)) {
        draw_feature(page, pj, f);
    }
}

fn draw_feature(page: &mut Page, pj: &PrintProjection, f: &Feature) {
    let c = Rgb::from_u8(f.style.color);
    let proj = |ll: &LngLat| pj.to_page_mm(*ll);
    page.save();
    page.round_joins();
    match &f.geometry {
        Geometry::Line(v) => {
            page.stroke_color(c);
            page.line_width(width_mm(f.style.width));
            page.polyline(&v.iter().map(proj).collect::<Vec<_>>(), false);
            page.stroke();
        }
        Geometry::Polygon(v) => {
            let pts: Vec<_> = v.iter().map(proj).collect();
            page.save();
            page.alpha(f.style.fill_alpha as f64 / 255.0);
            page.fill_color(c);
            page.polyline(&pts, true);
            page.fill();
            page.restore();
            page.stroke_color(c);
            page.line_width(width_mm(f.style.width));
            page.polyline(&pts, true);
            page.stroke();
        }
        Geometry::RangeRing { center, radii_m } => {
            page.stroke_color(c);
            page.line_width(width_mm(f.style.width) * 0.8);
            for r in radii_m {
                let ring: Vec<_> = geodesic_circle(*center, *r, 144).iter().map(proj).collect();
                page.polyline(&ring, false);
                page.stroke();
            }
            for r in radii_m {
                let (x, y) = pj.to_page_mm(destination(*center, 0.0, *r));
                halo_text(
                    page,
                    x,
                    y - 0.8,
                    6.5,
                    Font::Regular,
                    Align::Center,
                    &format_distance(*r),
                    Rgb::BLACK,
                );
            }
            let (x, y) = pj.to_page_mm(*center);
            page.fill_color(c);
            page.circle(x, y, 0.9);
            page.fill();
        }
        Geometry::Marker(p) => {
            let (x, y) = pj.to_page_mm(*p);
            page.fill_color(Rgb::WHITE);
            page.circle(x, y, 2.1);
            page.fill();
            page.fill_color(c);
            page.circle(x, y, 1.5);
            page.fill();
            if !f.name.is_empty() {
                halo_text(
                    page,
                    x + 2.8,
                    y + 1.0,
                    7.0,
                    Font::Regular,
                    Align::Left,
                    &f.name,
                    Rgb::BLACK,
                );
            }
        }
        Geometry::Label(p) => {
            let (x, y) = pj.to_page_mm(*p);
            halo_text(page, x, y, 9.0, Font::Bold, Align::Center, &f.name, c);
        }
    }
    page.restore();
}

/// Points where `poly` crosses the horizontal line y = `y0` (page mm), returned as x values.
fn cross_h(poly: &[(f64, f64)], y0: f64) -> Vec<f64> {
    poly.windows(2)
        .filter(|w| (w[0].1 - y0) * (w[1].1 - y0) < 0.0)
        .map(|w| w[0].0 + (y0 - w[0].1) / (w[1].1 - w[0].1) * (w[1].0 - w[0].0))
        .collect()
}

fn cross_v(poly: &[(f64, f64)], x0: f64) -> Vec<f64> {
    poly.windows(2)
        .filter(|w| (w[0].0 - x0) * (w[1].0 - x0) < 0.0)
        .map(|w| w[0].1 + (x0 - w[0].0) / (w[1].0 - w[0].0) * (w[1].1 - w[0].1))
        .collect()
}

fn draw_grid(
    page: &mut Page,
    spec: &PrintSpec,
    layout: &Layout,
    pj: &PrintProjection,
    warnings: &mut Vec<String>,
) {
    let f = layout.frame;
    let b = pj.bounds();
    match spec.grid {
        GridKind::None => {}
        GridKind::LatLon => {
            let step = deg_interval((b[2] - b[0]).max(b[3] - b[1]));
            let mut lines: Vec<(bool, f64, f64)> = vec![]; // (is_lat, value, page coordinate)
            let mut v = (b[0] / step).ceil() * step;
            while v <= b[2] {
                lines.push((false, v, pj.to_page_mm(LngLat::new(v, spec.center.lat)).0));
                v += step;
            }
            let mut v = (b[1] / step).ceil() * step;
            while v <= b[3] {
                lines.push((true, v, pj.to_page_mm(LngLat::new(spec.center.lng, v)).1));
                v += step;
            }
            page.save();
            page.rect(f.x, f.y, f.w, f.h);
            page.clip();
            page.stroke_color(GRID_COLOR);
            page.alpha(0.55);
            page.line_width(0.12);
            for (is_lat, _, c) in &lines {
                if *is_lat {
                    page.polyline(&[(f.x, *c), (f.x + f.w, *c)], false);
                } else {
                    page.polyline(&[(*c, f.y), (*c, f.y + f.h)], false);
                }
                page.stroke();
            }
            page.restore();
            page.fill_color(GRID_COLOR);
            for (is_lat, val, c) in lines {
                let label = deg_label(val, is_lat, step);
                if is_lat {
                    page.text_rotated(
                        f.x - 1.0,
                        c,
                        90.0,
                        6.0,
                        Font::Regular,
                        Align::Center,
                        &label,
                    );
                } else {
                    page.text(c, f.y - 1.2, 6.0, Font::Regular, Align::Center, &label);
                }
            }
        }
        GridKind::Utm => {
            let zone = utm_zone(spec.center.lng);
            let north = spec.center.lat >= 0.0;
            // Extent of the frame in UTM, from a grid of sample points (the frame is a rotated box in UTM).
            let (mut e0, mut e1, mut n0, mut n1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for i in 0..=8 {
                for j in 0..=8 {
                    let ll =
                        pj.from_page_mm(f.x + f.w * i as f64 / 8.0, f.y + f.h * j as f64 / 8.0);
                    let (e, n) = to_utm(ll, zone);
                    e0 = e0.min(e);
                    e1 = e1.max(e);
                    n0 = n0.min(n);
                    n1 = n1.max(n);
                }
            }
            if (spec.center.lng - (zone as f64 * 6.0 - 183.0)).abs() > 4.0 {
                warnings.push(format!(
                    "Map is far from the centre of UTM zone {zone}: grid convergence is large."
                ));
            }
            let step = utm_interval((e1 - e0).max(n1 - n0));
            let samples = 28;
            let mut lines: Vec<GridLine> = vec![];
            let mut e = (e0 / step).ceil() * step;
            while e <= e1 {
                let pts: Vec<_> = (0..=samples)
                    .filter_map(|k| {
                        from_utm(e, n0 + (n1 - n0) * k as f64 / samples as f64, zone, north)
                    })
                    .map(|ll| pj.to_page_mm(ll))
                    .collect();
                lines.push((true, e, pts));
                e += step;
            }
            let mut n = (n0 / step).ceil() * step;
            while n <= n1 {
                let pts: Vec<_> = (0..=samples)
                    .filter_map(|k| {
                        from_utm(e0 + (e1 - e0) * k as f64 / samples as f64, n, zone, north)
                    })
                    .map(|ll| pj.to_page_mm(ll))
                    .collect();
                lines.push((false, n, pts));
                n += step;
            }
            page.save();
            page.rect(f.x, f.y, f.w, f.h);
            page.clip();
            page.stroke_color(GRID_COLOR);
            page.alpha(0.55);
            page.line_width(0.12);
            for (_, _, pts) in &lines {
                page.polyline(pts, false);
                page.stroke();
            }
            page.restore();
            page.fill_color(GRID_COLOR);
            let label = |v: f64| {
                if step >= 1000.0 {
                    format!("{}", (v / 1000.0).round())
                } else {
                    format!("{v:.0}")
                }
            };
            for (is_e, val, pts) in &lines {
                let l = label(*val);
                if *is_e {
                    for x in cross_h(pts, f.y)
                        .into_iter()
                        .filter(|x| *x > f.x && *x < f.x + f.w)
                    {
                        page.text(x, f.y - 1.2, 6.0, Font::Regular, Align::Center, &l);
                    }
                    for x in cross_h(pts, f.y + f.h)
                        .into_iter()
                        .filter(|x| *x > f.x && *x < f.x + f.w)
                    {
                        page.text(x, f.y + f.h + 3.2, 6.0, Font::Regular, Align::Center, &l);
                    }
                } else {
                    for y in cross_v(pts, f.x)
                        .into_iter()
                        .filter(|y| *y > f.y && *y < f.y + f.h)
                    {
                        page.text_rotated(
                            f.x - 1.0,
                            y,
                            90.0,
                            6.0,
                            Font::Regular,
                            Align::Center,
                            &l,
                        );
                    }
                    for y in cross_v(pts, f.x + f.w)
                        .into_iter()
                        .filter(|y| *y > f.y && *y < f.y + f.h)
                    {
                        page.text_rotated(
                            f.x + f.w + 3.0,
                            y,
                            90.0,
                            6.0,
                            Font::Regular,
                            Align::Center,
                            &l,
                        );
                    }
                }
            }
        }
    }
}

fn draw_scale_bar(page: &mut Page, spec: &PrintSpec, layout: &Layout) {
    let f = layout.frame;
    // Aim for a bar about a quarter of the frame width on the ground.
    let max_m = f.w / 1000.0 * spec.scale_denominator * 0.27;
    let rows: Vec<(ScaleBar, String)> = [false, true]
        .into_iter()
        .map(|imp| {
            let (len, div, label) = nice_bar(max_m, imp);
            (scale_bar(len, div, spec.scale_denominator), label)
        })
        .collect();
    let box_w = rows
        .iter()
        .map(|(b, l)| b.len_mm + text_width_mm(l, 6.5, Font::Regular) + 6.0)
        .fold(0.0, f64::max)
        .max(46.0);
    let (bx, by, bh) = (f.x + 3.0, f.y + f.h - 3.0 - 19.0, 19.0);
    page.save();
    page.alpha(0.88);
    page.fill_color(Rgb::WHITE);
    page.rect(bx, by, box_w, bh);
    page.fill();
    page.restore();
    page.fill_color(Rgb::BLACK);
    page.text(
        bx + 2.0,
        by + 4.0,
        7.0,
        Font::Bold,
        Align::Left,
        &format!("1:{}", thousands(spec.scale_denominator)),
    );
    for (row, (bar, label)) in rows.iter().enumerate() {
        let y = by + 7.0 + row as f64 * 6.0;
        let seg = bar.len_mm / bar.divisions as f64;
        page.stroke_color(Rgb::BLACK);
        page.line_width(0.2);
        for k in 0..bar.divisions {
            page.fill_color(if k % 2 == 0 { Rgb::BLACK } else { Rgb::WHITE });
            page.rect(bx + 2.0 + k as f64 * seg, y, seg, 1.6);
            page.fill_stroke();
        }
        page.fill_color(Rgb::BLACK);
        page.text(bx + 2.0, y + 4.2, 6.0, Font::Regular, Align::Center, "0");
        page.text(
            bx + 2.0 + bar.len_mm,
            y + 4.2,
            6.0,
            Font::Regular,
            Align::Center,
            label,
        );
    }
}

/// Largest font size up to `base` at which `text` fits in `max_w` mm (never below 3.5 pt).
fn fit_size(text: &str, base: f64, max_w: f64) -> f64 {
    let w = text_width_mm(text, base, Font::Regular);
    if w <= max_w {
        base
    } else {
        (base * max_w / w).max(3.5)
    }
}

fn thousands(v: f64) -> String {
    let s = format!("{v:.0}");
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn draw_north_arrow(page: &mut Page, spec: &PrintSpec, layout: &Layout) {
    let f = layout.frame;
    let (w, h) = (18.0, 24.0);
    let (bx, by) = (f.x + f.w - w - 3.0, f.y + 3.0);
    page.save();
    page.alpha(0.88);
    page.fill_color(Rgb::WHITE);
    page.rect(bx, by, w, h);
    page.fill();
    page.restore();
    let (cx, base) = (bx + w / 2.0, by + h - 4.0);
    // True north: a filled arrow.
    page.fill_color(Rgb::BLACK);
    page.polyline(
        &[
            (cx, by + 7.0),
            (cx - 2.6, base),
            (cx, base - 2.4),
            (cx + 2.6, base),
        ],
        true,
    );
    page.fill();
    page.text(cx, by + 5.5, 8.0, Font::Bold, Align::Center, "N");
    if let Some(d) = spec.declination_deg {
        // Magnetic north: a half arrow rotated clockwise by the (east-positive) declination.
        let (s, c) = d.to_radians().sin_cos();
        let (len, ox) = (base - (by + 8.0), cx + 5.0);
        let tip = (ox + len * s, base - len * c);
        page.stroke_color(Rgb::BLACK);
        page.line_width(0.3);
        page.polyline(&[(ox, base), tip], false);
        page.stroke();
        let hemi = if d >= 0.0 { 'E' } else { 'W' };
        page.fill_color(Rgb::BLACK);
        page.text(
            bx + w - 1.0,
            by + h - 0.8,
            5.0,
            Font::Regular,
            Align::Right,
            &format!("MN {:.1}\u{b0}{hemi}", d.abs()),
        );
    }
}

fn draw_legend(page: &mut Page, layout: &Layout, project: &Project, overlays: &[LegendOverlay]) {
    let mut entries: Vec<LegendEntry> = vec![];
    for f in project
        .features
        .iter()
        .filter(|f| project.is_visible(f) && !f.name.is_empty())
    {
        if !matches!(f.geometry, Geometry::Label(_))
            && !entries.iter().any(|(n, _, _)| *n == f.name)
        {
            entries.push((f.name.clone(), Some(f), None));
        }
    }
    entries.truncate(14);
    for o in overlays {
        // Opacity >= 1 means "not a blended layer" (live data counts etc.): no percentage.
        let label = if o.opacity < 0.995 {
            format!("{} ({:.0}%)", o.name, o.opacity * 100.0)
        } else {
            o.name.clone()
        };
        entries.push((label, None, o.color));
    }
    if entries.is_empty() {
        return;
    }
    let fr = layout.frame;
    let row = 4.4;
    let w = entries
        .iter()
        .map(|(n, _, _)| text_width_mm(n, 6.5, Font::Regular))
        .fold(0.0, f64::max)
        .max(text_width_mm("Legend", 7.5, Font::Bold))
        + 14.0;
    let h = 7.0 + entries.len() as f64 * row;
    let (x, y) = (fr.x + fr.w - w - 3.0, fr.y + fr.h - h - 3.0);
    page.save();
    page.alpha(0.9);
    page.fill_color(Rgb::WHITE);
    page.rect(x, y, w, h);
    page.fill();
    page.restore();
    page.fill_color(Rgb::BLACK);
    page.text(x + 2.0, y + 4.5, 7.5, Font::Bold, Align::Left, "Legend");
    for (i, (name, feat, swatch)) in entries.iter().enumerate() {
        let ry = y + 7.0 + i as f64 * row;
        page.save();
        match feat.map(|f| (&f.geometry, Rgb::from_u8(f.style.color))) {
            Some((Geometry::Polygon(_), c)) => {
                page.fill_color(c);
                page.alpha(0.6);
                page.rect(x + 2.0, ry + 0.4, 7.0, 2.6);
                page.fill();
            }
            Some((Geometry::Line(_), c)) | Some((Geometry::RangeRing { .. }, c)) => {
                page.stroke_color(c);
                page.line_width(0.6);
                page.polyline(&[(x + 2.0, ry + 1.7), (x + 9.0, ry + 1.7)], false);
                page.stroke();
            }
            Some((_, c)) => {
                page.fill_color(c);
                page.circle(x + 5.5, ry + 1.7, 1.4);
                page.fill();
            }
            None => {
                page.fill_color(swatch.map_or(Rgb(0.7, 0.7, 0.7), Rgb::from_u8));
                page.rect(x + 2.0, ry + 0.4, 7.0, 2.6);
                page.fill();
            }
        }
        page.restore();
        page.fill_color(Rgb::BLACK);
        page.text(x + 11.0, ry + 2.7, 6.5, Font::Regular, Align::Left, name);
    }
}

/// Build the finished PDF. `map_jpeg` is the rendered map at the frame's pixel size.
pub fn compose_pdf(
    spec: &PrintSpec,
    map_jpeg: Vec<u8>,
    map_px: [u32; 2],
    project: &Project,
    extra: &[Feature],
    overlays: &[LegendOverlay],
    date: &str,
) -> Result<Composed, LayoutError> {
    let layout = Layout::new(spec)?;
    let pj = PrintProjection::new(spec, &layout);
    let f = layout.frame;
    let mut warnings = Vec::new();
    let mut page = Page::new(layout.page_w, layout.page_h);

    if layout.title_h > 0.0 {
        page.fill_color(Rgb::BLACK);
        page.text(
            spec.margin_mm,
            spec.margin_mm + 5.5,
            15.0,
            Font::Bold,
            Align::Left,
            &spec.title,
        );
        if !spec.subtitle.is_empty() {
            page.text(
                spec.margin_mm,
                spec.margin_mm + 10.5,
                8.5,
                Font::Regular,
                Align::Left,
                &spec.subtitle,
            );
        }
    }

    let img = page.add_jpeg(map_jpeg, map_px[0], map_px[1]);
    page.draw_image(img, f.x, f.y, f.w, f.h);

    draw_grid(&mut page, spec, &layout, &pj, &mut warnings);
    page.save();
    page.rect(f.x, f.y, f.w, f.h);
    page.clip();
    // Live data first (beneath), then the user's own markup; only the latter is listed in the legend.
    for f in extra {
        draw_feature(&mut page, &pj, f);
    }
    draw_features(&mut page, &pj, project);
    page.restore();

    page.stroke_color(Rgb::BLACK);
    page.line_width(0.35);
    page.rect(f.x, f.y, f.w, f.h);
    page.stroke();

    if spec.scale_bar {
        draw_scale_bar(&mut page, spec, &layout);
    }
    if spec.north_arrow {
        draw_north_arrow(&mut page, spec, &layout);
    }
    if spec.legend {
        draw_legend(&mut page, &layout, project, overlays);
    }

    // Footer: the credit line is a licence requirement for the map data, so it is always printed.
    // Two lines, each shrunk if needed so long attributions never run into each other or off the page.
    let mut credit = spec.attribution.clone();
    if spec.grid == GridKind::Utm {
        if !credit.is_empty() {
            credit.push_str("  |  ");
        }
        credit.push_str(&format!(
            "UTM zone {}{} grid, WGS84, labels in km",
            utm_zone(spec.center.lng),
            if spec.center.lat >= 0.0 { 'N' } else { 'S' }
        ));
    }
    let note = format!(
        "ShowMeFire Field  |  {date}  |  print at 100% (no fit-to-page) for true 1:{} scale",
        thousands(spec.scale_denominator)
    );
    page.fill_color(Rgb::BLACK);
    for (k, line) in [credit, note].iter().enumerate() {
        let size = fit_size(line, 6.0, f.w);
        page.text(
            f.x,
            // Below the grid labels (about 3.2 mm under the frame), in the footer band.
            f.y + f.h + 7.4 + k as f64 * 2.9,
            size,
            Font::Regular,
            Align::Left,
            line,
        );
    }

    Ok(Composed {
        bytes: page.finish(if spec.title.is_empty() {
            "Map"
        } else {
            &spec.title
        }),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Document;
    use smf_features::{Editor, Style};

    fn tiny_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([180, 200, 160]));
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80)
            .encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    fn spec() -> PrintSpec {
        PrintSpec {
            title: "Pine Ridge Unit 3".into(),
            subtitle: "Operations map".into(),
            // 93.0 W is the zone 15 central meridian, so a grid line at E 500000 passes through the page centre.
            center: LngLat::new(-93.0, 38.5),
            dpi: 60,
            attribution: "USGS The National Map".into(),
            ..PrintSpec::default()
        }
    }

    fn build(spec: &PrintSpec, project: &Project, ov: &[LegendOverlay]) -> (Vec<u8>, Composed) {
        let layout = Layout::new(spec).unwrap();
        let px = PrintProjection::new(spec, &layout).frame_px();
        let jpg = tiny_jpeg(px[0], px[1]);
        (
            jpg.clone(),
            compose_pdf(spec, jpg, px, project, &[], ov, "2026-10-09").unwrap(),
        )
    }

    fn project() -> Project {
        let mut e = Editor::default();
        e.current_style = Style {
            color: [200, 30, 30],
            width: 3.0,
            fill_alpha: 90,
        };
        e.add(
            Geometry::Marker(LngLat::new(-93.0, 38.5)),
            Some("Command Post".into()),
        );
        e.add(
            Geometry::Line(vec![LngLat::new(-93.01, 38.49), LngLat::new(-92.99, 38.51)]),
            Some("Dozer line".into()),
        );
        e.add(
            Geometry::Polygon(vec![
                LngLat::new(-93.01, 38.5),
                LngLat::new(-92.99, 38.5),
                LngLat::new(-92.99, 38.52),
            ]),
            Some("Burn unit".into()),
        );
        e.add(
            Geometry::RangeRing {
                center: LngLat::new(-93.0, 38.5),
                radii_m: vec![500.0, 1000.0],
            },
            Some("Rings".into()),
        );
        e.project
    }

    #[test]
    fn page_has_title_attribution_scale_and_legend_text() {
        let (_, c) = build(
            &spec(),
            &project(),
            &[LegendOverlay {
                name: "Fire danger".into(),
                opacity: 0.6,
                color: None,
            }],
        );
        let doc = Document::load_mem(&c.bytes).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
        let text = doc.extract_text(&[1]).unwrap();
        for want in [
            "Pine Ridge Unit 3",
            "Operations map",
            "USGS The National Map",
            "1:24,000",
            "Legend",
            "Command Post",
            "Dozer line",
            "Burn unit",
            "Fire danger (60%)",
            "print at 100%",
            "2026-10-09",
        ] {
            assert!(text.contains(want), "missing {want:?} in {text:?}");
        }
        assert!(text.contains("UTM zone 15N"), "grid datum note: {text:?}");
    }

    /// The drawn scale-bar segments must be exactly ground length / scale on paper.
    #[test]
    fn scale_bar_is_drawn_at_the_true_length_for_the_scale() {
        let s = spec();
        let (_, c) = build(&s, &Project::default(), &[]);
        let doc = Document::load_mem(&c.bytes).unwrap();
        let (_, pid) = doc.get_pages().into_iter().next().unwrap();
        let content = String::from_utf8_lossy(&doc.get_page_content(pid)).into_owned();
        // Expected: the metric bar for this frame.
        let layout = Layout::new(&s).unwrap();
        let (len_m, div, _) = nice_bar(layout.frame.w / 1000.0 * s.scale_denominator * 0.27, false);
        let bar = scale_bar(len_m, div, s.scale_denominator);
        let seg_pt = bar.len_mm / div as f64 * 72.0 / 25.4;
        let found = content
            .lines()
            .filter(|l| l.ends_with(" re"))
            .filter_map(|l| {
                l.split_whitespace()
                    .nth(2)
                    .and_then(|w| w.parse::<f64>().ok())
            })
            .any(|w| (w - seg_pt).abs() < 0.01);
        assert!(
            found,
            "no scale bar segment of {seg_pt} pt in the content stream"
        );
        // And the numbers themselves: 1 km at 1:24,000 is 41.667 mm.
        assert!((scale_bar(1000.0, 4, 24_000.0).len_mm - 41.6667).abs() < 1e-3);
        assert!(
            (scale_bar(1609.344, 4, 24_000.0).len_mm - 67.056).abs() < 1e-3,
            "1 mile at 1:24,000 is 67.056 mm"
        );
    }

    #[test]
    fn utm_grid_labels_the_central_meridian_line() {
        let (_, c) = build(&spec(), &Project::default(), &[]);
        let doc = Document::load_mem(&c.bytes).unwrap();
        let text = doc.extract_text(&[1]).unwrap();
        // E 500000 -> "500" km label; the frame spans ~4.7 km so the interval is 1 km (labels 498..502).
        assert!(text.contains("500"), "{text:?}");
        assert_eq!(utm_zone(-93.0), 15);
        assert_eq!(utm_zone(-90.1), 15);
        assert_eq!(utm_zone(-89.9), 16);
        assert_eq!(utm_zone(179.9), 60);
    }

    #[test]
    fn grid_intervals_scale_with_the_map() {
        assert_eq!(utm_interval(4_700.0), 1_000.0);
        assert_eq!(utm_interval(47_000.0), 10_000.0);
        assert_eq!(utm_interval(900.0), 100.0);
        assert_eq!(utm_interval(901.0), 200.0);
        assert!((deg_interval(0.05) - 1.0 / 120.0).abs() < 1e-12);
        assert_eq!(deg_label(38.5, true, 1.0 / 60.0), "38\u{b0}30'N");
        assert_eq!(deg_label(-92.0, false, 1.0), "92\u{b0}W");
        assert_eq!(
            deg_label(38.0 + 1.0 / 360.0 * 7.0, true, 1.0 / 360.0),
            "38\u{b0}01'10\"N"
        );
    }

    #[test]
    fn latlon_grid_and_options_toggle_cleanly() {
        let mut s = spec();
        s.grid = GridKind::LatLon;
        s.legend = false;
        s.north_arrow = false;
        s.scale_bar = false;
        let (_, c) = build(&s, &project(), &[]);
        let text = Document::load_mem(&c.bytes)
            .unwrap()
            .extract_text(&[1])
            .unwrap();
        assert!(text.contains('\u{b0}'), "degree labels: {text:?}");
        assert!(!text.contains("Legend"));
        assert!(!text.contains("UTM zone"), "UTM note only with a UTM grid");
        s.grid = GridKind::None;
        let (_, c2) = build(&s, &project(), &[]);
        assert!(!c2.bytes.is_empty());
    }

    #[test]
    fn declination_adds_a_magnetic_north_note() {
        let mut s = spec();
        s.declination_deg = Some(-3.4);
        let (_, c) = build(&s, &Project::default(), &[]);
        let text = Document::load_mem(&c.bytes)
            .unwrap()
            .extract_text(&[1])
            .unwrap();
        assert!(text.contains("MN 3.4"), "{text:?}");
        assert!(text.contains('W'));
    }

    #[test]
    fn invisible_features_are_not_printed_and_long_names_do_not_break_layout() {
        let mut e = Editor::default();
        let id = e.add(
            Geometry::Marker(LngLat::new(-93.0, 38.5)),
            Some("Secret".into()),
        );
        let mut f = e.project.feature(id).unwrap().clone();
        f.visible = false;
        e.replace(f);
        e.add(
            Geometry::Marker(LngLat::new(-93.0, 38.5)),
            Some("x".repeat(400)),
        );
        let (_, c) = build(&spec(), &e.project, &[]);
        let text = Document::load_mem(&c.bytes)
            .unwrap()
            .extract_text(&[1])
            .unwrap();
        assert!(!text.contains("Secret"));
    }

    #[test]
    fn footer_lines_never_overflow_the_frame() {
        let mut s = spec();
        s.attribution =
            "USGS The National Map; Terrain: Mapzen, USGS 3DEP/SRTM and others; ".repeat(4);
        let (_, c) = build(&s, &Project::default(), &[]);
        let doc = Document::load_mem(&c.bytes).unwrap();
        let text = doc.extract_text(&[1]).unwrap();
        // Both footer lines are present and separate (no "kmShowMeFire" run-on).
        assert!(text.contains("labels in km"), "{text:?}");
        assert!(text.contains("ShowMeFire Field"), "{text:?}");
        assert!(!text.contains("kmShowMeFire"));
        let layout = Layout::new(&s).unwrap();
        let long = s.attribution.clone();
        assert!(
            fit_size(&long, 6.0, layout.frame.w) < 6.0
                && fit_size(&long, 6.0, layout.frame.w) >= 3.5
        );
        assert_eq!(fit_size("short", 6.0, 100.0), 6.0);
    }

    #[test]
    fn legend_rows_do_not_inherit_each_others_opacity() {
        // Regression: a polygon swatch's 60% opacity once leaked into every later legend row because the
        // opacity was set outside any saved graphics state. Invariant: every `gs` sits inside q ... Q.
        let (_, c) = build(
            &spec(),
            &project(),
            &[LegendOverlay {
                name: "Overlay".into(),
                opacity: 0.5,
                color: None,
            }],
        );
        let doc = Document::load_mem(&c.bytes).unwrap();
        let (_, pid) = doc.get_pages().into_iter().next().unwrap();
        let content = String::from_utf8_lossy(&doc.get_page_content(pid)).into_owned();
        let (mut depth, mut gs_count) = (0i32, 0);
        for line in content.lines().map(str::trim) {
            if line == "q" {
                depth += 1;
            } else if line == "Q" {
                depth -= 1;
                assert!(depth >= 0, "restore without save");
            } else if line.ends_with(" gs") {
                gs_count += 1;
                assert!(
                    depth > 0,
                    "opacity change outside a saved graphics state: {line}"
                );
            }
        }
        assert_eq!(depth, 0, "unbalanced graphics state");
        assert!(
            gs_count >= 4,
            "expected several opacity changes, saw {gs_count}"
        );
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(24_000.0), "24,000");
        assert_eq!(thousands(500.0), "500");
        assert_eq!(thousands(1_000_000.0), "1,000,000");
    }
}
