//! Draws project features, in-progress shapes and measurements on top of the map
//! image using egui's painter. Positions are computed in f64 by `MapView` (2D) or
//! `Camera3D` (3D) so they stay exact at any zoom.

use eframe::egui::{self, Align2, Color32, FontId, Mesh, Painter, Pos2, Shape, Stroke, Vec2};
use smf_features::{Editor, Feature, Geometry, Tool};
use smf_geo::{LngLat, MapView, destination, format_area, format_distance, geodesic_circle};
use smf_terrain::{Camera3D, HeightField};

/// Longest straight segment drawn in 3D before it is split so lines follow the ground.
const DRAPE_STEP_M: f64 = 120.0;
const MAX_DRAPE_POINTS: usize = 3000;

/// Converts geographic positions to egui screen positions for the active view mode.
pub enum Projector<'a> {
    Flat {
        view: &'a MapView,
        origin: Pos2,
        ppp: f32,
    },
    Terrain {
        cam: &'a Camera3D,
        hf: &'a HeightField,
        origin: Pos2,
        ppp: f32,
    },
}

impl Projector<'_> {
    fn to_pos(&self, s: [f64; 2], origin: Pos2, ppp: f32) -> Pos2 {
        Pos2::new(origin.x + s[0] as f32 / ppp, origin.y + s[1] as f32 / ppp)
    }

    /// None when the point is behind the 3D camera.
    pub fn pos(&self, p: LngLat) -> Option<Pos2> {
        match self {
            Projector::Flat { view, origin, ppp } => {
                Some(self.to_pos(view.lnglat_to_screen(p), *origin, *ppp))
            }
            Projector::Terrain {
                cam,
                hf,
                origin,
                ppp,
            } => {
                let elev = hf.sample(p).unwrap_or(0.0) as f64;
                cam.project(p, elev).map(|s| self.to_pos(s, *origin, *ppp))
            }
        }
    }

    fn is_3d(&self) -> bool {
        matches!(self, Projector::Terrain { .. })
    }

    /// Project a geographic polyline, splitting where it passes behind the camera and
    /// (in 3D) adding intermediate points so it hugs the terrain.
    pub fn polyline(&self, pts: &[LngLat]) -> Vec<Vec<Pos2>> {
        let dense;
        let pts = if self.is_3d() {
            dense = densify(pts, DRAPE_STEP_M);
            &dense[..]
        } else {
            pts
        };
        let mut out: Vec<Vec<Pos2>> = vec![];
        let mut cur: Vec<Pos2> = vec![];
        for p in pts {
            match self.pos(*p) {
                Some(s) => cur.push(s),
                None => {
                    if cur.len() > 1 {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                }
            }
        }
        if cur.len() > 1 {
            out.push(cur);
        }
        out
    }
}

/// Insert points so no segment exceeds `max_m` (linear in lng/lat, fine at these lengths).
pub fn densify(pts: &[LngLat], max_m: f64) -> Vec<LngLat> {
    if pts.len() < 2 {
        return pts.to_vec();
    }
    let total: f64 = pts
        .windows(2)
        .map(|w| smf_geo::distance_m(w[0], w[1]))
        .sum();
    // Raise the step for very long paths so a continent-long line can't produce millions of points.
    let step = max_m.max(total / MAX_DRAPE_POINTS as f64);
    let mut out = vec![pts[0]];
    for w in pts.windows(2) {
        let n = (smf_geo::distance_m(w[0], w[1]) / step).ceil().max(1.0) as usize;
        for i in 1..=n {
            let t = i as f64 / n as f64;
            out.push(LngLat::new(
                w[0].lng + (w[1].lng - w[0].lng) * t,
                w[0].lat + (w[1].lat - w[0].lat) * t,
            ));
        }
    }
    out
}

fn color(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Text with a translucent plate so it stays legible over any basemap.
pub fn label(painter: &Painter, pos: Pos2, anchor: Align2, text: &str, size: f32, fg: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(size), fg);
    let r = anchor
        .anchor_size(pos, galley.size())
        .expand2(Vec2::new(4.0, 2.0));
    painter.rect_filled(r, 3.0, Color32::from_white_alpha(205));
    painter.galley(r.min + Vec2::new(4.0, 2.0), galley, fg);
}

/// Triangulate a (possibly concave) screen-space polygon; egui can only fill convex shapes itself.
fn fill_polygon(painter: &Painter, pts: &[Pos2], fill: Color32) {
    if pts.len() < 3 || fill.a() == 0 {
        return;
    }
    let flat: Vec<f64> = pts.iter().flat_map(|p| [p.x as f64, p.y as f64]).collect();
    let Ok(tris) = earcutr::earcut(&flat, &[], 2) else {
        return;
    };
    let mut mesh = Mesh::default();
    for p in pts {
        mesh.colored_vertex(*p, fill);
    }
    for t in tris.chunks_exact(3) {
        mesh.add_triangle(t[0] as u32, t[1] as u32, t[2] as u32);
    }
    painter.add(Shape::mesh(mesh));
}

fn pin(painter: &Painter, tip: Pos2, col: Color32, selected: bool) {
    let head = tip + Vec2::new(0.0, -18.0);
    if selected {
        painter.circle_filled(head, 10.5, Color32::WHITE);
    }
    painter.add(Shape::convex_polygon(
        vec![tip, head + Vec2::new(-6.0, 4.0), head + Vec2::new(6.0, 4.0)],
        col,
        Stroke::new(1.0, Color32::BLACK),
    ));
    painter.circle_filled(head, 8.0, col);
    painter.circle_stroke(head, 8.0, Stroke::new(1.5, Color32::BLACK));
    painter.circle_filled(head, 2.8, Color32::WHITE);
}

fn draw_feature(painter: &Painter, pj: &Projector, f: &Feature, selected: bool) {
    let col = color(f.style.color);
    let stroke = Stroke::new(f.style.width, col);
    let halo = Stroke::new(f.style.width + 4.0, Color32::from_white_alpha(230));
    let draw_line = |path: &[LngLat]| {
        for seg in pj.polyline(path) {
            if selected {
                painter.add(Shape::line(seg.clone(), halo));
            }
            painter.add(Shape::line(seg, stroke));
        }
    };
    match &f.geometry {
        Geometry::Marker(p) => {
            if let Some(tip) = pj.pos(*p) {
                pin(painter, tip, col, selected);
                if !f.name.is_empty() {
                    label(
                        painter,
                        tip + Vec2::new(0.0, 6.0),
                        Align2::CENTER_TOP,
                        &f.name,
                        12.0,
                        Color32::BLACK,
                    );
                }
            }
        }
        Geometry::Label(p) => {
            let Some(pos) = pj.pos(*p) else { return };
            let galley = painter.layout_no_wrap(f.name.clone(), FontId::proportional(15.0), col);
            let r = Align2::CENTER_CENTER
                .anchor_size(pos, galley.size())
                .expand2(Vec2::new(5.0, 3.0));
            painter.rect_filled(r, 3.0, Color32::from_white_alpha(225));
            if selected {
                painter.rect_stroke(
                    r.expand(2.0),
                    4.0,
                    Stroke::new(2.0, Color32::from_rgb(0, 120, 255)),
                    egui::StrokeKind::Outside,
                );
            }
            painter.galley(r.min + Vec2::new(5.0, 3.0), galley, col);
        }
        Geometry::Line(v) => draw_line(v),
        Geometry::Polygon(v) => {
            let mut ring = v.clone();
            ring.push(v[0]);
            let segs = pj.polyline(&ring);
            // Fill only when the whole outline is visible; a partial fill would be wrong.
            if let [one] = &segs[..] {
                let mut pts = one.clone();
                pts.pop();
                fill_polygon(
                    painter,
                    &pts,
                    Color32::from_rgba_unmultiplied(
                        f.style.color[0],
                        f.style.color[1],
                        f.style.color[2],
                        f.style.fill_alpha,
                    ),
                );
            }
            for seg in segs {
                if selected {
                    painter.add(Shape::line(seg.clone(), halo));
                }
                painter.add(Shape::line(seg, stroke));
            }
        }
        Geometry::RangeRing { center, radii_m } => {
            for r in radii_m {
                draw_line(&geodesic_circle(*center, *r, 96));
                if let Some(p) = pj.pos(destination(*center, 0.0, *r)) {
                    label(
                        painter,
                        p,
                        Align2::CENTER_BOTTOM,
                        &format_distance(*r),
                        11.0,
                        Color32::BLACK,
                    );
                }
            }
            if let Some(c) = pj.pos(*center) {
                painter.circle_filled(c, 4.0, col);
            }
        }
    }
    if selected {
        let handle = Color32::from_rgb(0, 120, 255);
        for v in f.geometry.vertices() {
            if let Some(p) = pj.pos(v) {
                painter.circle_filled(p, 5.5, Color32::WHITE);
                painter.circle_stroke(p, 5.5, Stroke::new(2.0, handle));
            }
        }
    }
}

pub fn draw_project(painter: &Painter, pj: &Projector, editor: &Editor) {
    for f in editor
        .project
        .features
        .iter()
        .filter(|f| editor.project.is_visible(f))
    {
        draw_feature(painter, pj, f, editor.selection == Some(f.id));
    }
}

/// In-progress shape with a rubber-band segment to the cursor, plus the measure tool.
pub fn draw_drafts(painter: &Painter, pj: &Projector, editor: &Editor, cursor: Option<LngLat>) {
    let accent = color(editor.current_style.color);
    let draft = &editor.draft;
    if !draft.is_empty() {
        let mut all = draft.clone();
        if let Some(c) = cursor
            && editor.tool != Tool::Freehand
        {
            all.push(c);
        }
        let pts: Vec<Pos2> = draft.iter().filter_map(|p| pj.pos(*p)).collect();
        if editor.tool == Tool::Polygon && draft.len() >= 3 {
            fill_polygon(
                painter,
                &pts,
                Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 40),
            );
        }
        for seg in pj.polyline(&all) {
            painter.add(Shape::line(
                seg,
                Stroke::new(editor.current_style.width, accent),
            ));
        }
        for p in &pts {
            painter.circle_filled(*p, 4.0, Color32::WHITE);
            painter.circle_stroke(*p, 4.0, Stroke::new(1.5, accent));
        }
        if let Some(last) = all.last().and_then(|p| pj.pos(*p)) {
            label(
                painter,
                last + Vec2::new(14.0, -14.0),
                Align2::LEFT_BOTTOM,
                &format_distance(smf_geo::path_length_m(&all)),
                12.0,
                Color32::BLACK,
            );
        }
    }

    let m = &editor.measure;
    if !m.is_empty() {
        let blue = Color32::from_rgb(0, 100, 220);
        let pts: Vec<Pos2> = m.iter().filter_map(|p| pj.pos(*p)).collect();
        if m.len() >= 3 {
            fill_polygon(
                painter,
                &pts,
                Color32::from_rgba_unmultiplied(0, 100, 220, 40),
            );
        }
        let mut ring = m.clone();
        if m.len() >= 3 {
            ring.push(m[0]);
        }
        for seg in pj.polyline(&ring) {
            painter.add(Shape::line(seg, Stroke::new(2.5, blue)));
        }
        for w in m.windows(2) {
            let mid = LngLat::new((w[0].lng + w[1].lng) / 2.0, (w[0].lat + w[1].lat) / 2.0);
            if let Some(p) = pj.pos(mid) {
                label(
                    painter,
                    p,
                    Align2::CENTER_CENTER,
                    &format_distance(smf_geo::distance_m(w[0], w[1])),
                    11.0,
                    blue,
                );
            }
        }
        for p in &pts {
            painter.circle_filled(*p, 4.0, blue);
        }
    }
}

/// One-line measurement summary for the status bar.
pub fn measure_summary(editor: &Editor) -> Option<String> {
    if editor.measure.len() < 2 {
        return None;
    }
    let (len, area) = editor.measurement();
    Some(match area {
        Some(a) => format!(
            "Length {}  |  Area {}",
            format_distance(len),
            format_area(a)
        ),
        None => format!("Length {}", format_distance(len)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn densify_bounds_segment_length_and_keeps_endpoints() {
        let pts = [LngLat::new(-92.0, 38.0), LngLat::new(-91.9, 38.0)]; // ~8.7 km
        let d = densify(&pts, 120.0);
        assert_eq!(d.first(), pts.first());
        assert_eq!(d.last(), pts.last());
        for w in d.windows(2) {
            assert!(smf_geo::distance_m(w[0], w[1]) <= 121.0);
        }
        assert!(d.len() > 70 && d.len() < 80, "{}", d.len());
    }

    #[test]
    fn densify_caps_point_count_on_huge_paths() {
        let pts = [LngLat::new(-120.0, 30.0), LngLat::new(-70.0, 45.0)];
        let d = densify(&pts, 120.0);
        assert!(d.len() <= MAX_DRAPE_POINTS + 10, "{}", d.len());
    }

    #[test]
    fn densify_leaves_single_points_alone() {
        let p = [LngLat::new(1.0, 2.0)];
        assert_eq!(densify(&p, 100.0), p.to_vec());
    }
}
