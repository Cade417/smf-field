//! Draws project features, in-progress shapes and measurements on top of the map
//! image using egui's painter. Positions are computed in f64 by `MapView` so they
//! stay exact at any zoom.

use eframe::egui::{self, Align2, Color32, FontId, Mesh, Painter, Pos2, Shape, Stroke, Vec2};
use smf_features::{Editor, Feature, Geometry, Tool, screen_paths};
use smf_geo::{LngLat, MapView, destination, format_area, format_distance};

pub struct Projector<'a> {
    pub view: &'a MapView,
    pub origin: Pos2,
    pub ppp: f32,
}

impl Projector<'_> {
    pub fn pos(&self, p: LngLat) -> Pos2 {
        let s = self.view.lnglat_to_screen(p);
        Pos2::new(
            self.origin.x + s[0] as f32 / self.ppp,
            self.origin.y + s[1] as f32 / self.ppp,
        )
    }
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
    let to_pts = |path: &Vec<[f64; 2]>| -> Vec<Pos2> {
        path.iter()
            .map(|s| {
                Pos2::new(
                    pj.origin.x + s[0] as f32 / pj.ppp,
                    pj.origin.y + s[1] as f32 / pj.ppp,
                )
            })
            .collect()
    };
    let paths = screen_paths(&f.geometry, &|p| pj.view.lnglat_to_screen(p));
    match &f.geometry {
        Geometry::Marker(p) => {
            pin(painter, pj.pos(*p), col, selected);
            if !f.name.is_empty() {
                label(
                    painter,
                    pj.pos(*p) + Vec2::new(0.0, 6.0),
                    Align2::CENTER_TOP,
                    &f.name,
                    12.0,
                    Color32::BLACK,
                );
            }
        }
        Geometry::Label(p) => {
            let pos = pj.pos(*p);
            let fg = col;
            let galley = painter.layout_no_wrap(f.name.clone(), FontId::proportional(15.0), fg);
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
            painter.galley(r.min + Vec2::new(5.0, 3.0), galley, fg);
        }
        Geometry::Line(_) => {
            let pts = to_pts(&paths[0]);
            if selected {
                painter.add(Shape::line(pts.clone(), halo));
            }
            painter.add(Shape::line(pts, stroke));
        }
        Geometry::Polygon(_) => {
            let mut pts = to_pts(&paths[0]);
            pts.pop(); // drop the duplicated closing point for triangulation
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
            pts.push(pts[0]);
            if selected {
                painter.add(Shape::line(pts.clone(), halo));
            }
            painter.add(Shape::line(pts, stroke));
        }
        Geometry::RangeRing { center, radii_m } => {
            for (path, r) in paths.iter().zip(radii_m) {
                let pts = to_pts(path);
                if selected {
                    painter.add(Shape::line(pts.clone(), halo));
                }
                painter.add(Shape::line(pts, stroke));
                label(
                    painter,
                    pj.pos(destination(*center, 0.0, *r)),
                    Align2::CENTER_BOTTOM,
                    &format_distance(*r),
                    11.0,
                    Color32::BLACK,
                );
            }
            painter.circle_filled(pj.pos(*center), 4.0, col);
        }
    }
    if selected {
        let handle = Color32::from_rgb(0, 120, 255);
        for v in f.geometry.vertices() {
            let p = pj.pos(v);
            painter.circle_filled(p, 5.5, Color32::WHITE);
            painter.circle_stroke(p, 5.5, Stroke::new(2.0, handle));
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
        let mut pts: Vec<Pos2> = draft.iter().map(|p| pj.pos(*p)).collect();
        if editor.tool == Tool::Polygon && draft.len() >= 3 {
            fill_polygon(
                painter,
                &pts,
                Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 40),
            );
        }
        if let Some(c) = cursor
            && editor.tool != Tool::Freehand
        {
            pts.push(pj.pos(c));
        }
        painter.add(Shape::line(
            pts.clone(),
            Stroke::new(editor.current_style.width, accent),
        ));
        for p in draft.iter().map(|p| pj.pos(*p)) {
            painter.circle_filled(p, 4.0, Color32::WHITE);
            painter.circle_stroke(p, 4.0, Stroke::new(1.5, accent));
        }
        let mut all = draft.clone();
        if let Some(c) = cursor {
            all.push(c);
        }
        let len = smf_geo::path_length_m(&all);
        if let Some(last) = pts.last() {
            label(
                painter,
                *last + Vec2::new(14.0, -14.0),
                Align2::LEFT_BOTTOM,
                &format_distance(len),
                12.0,
                Color32::BLACK,
            );
        }
    }

    let m = &editor.measure;
    if !m.is_empty() {
        let blue = Color32::from_rgb(0, 100, 220);
        let pts: Vec<Pos2> = m.iter().map(|p| pj.pos(*p)).collect();
        if m.len() >= 3 {
            fill_polygon(
                painter,
                &pts,
                Color32::from_rgba_unmultiplied(0, 100, 220, 40),
            );
        }
        let mut line = pts.clone();
        if m.len() >= 3 {
            line.push(pts[0]);
        }
        painter.add(Shape::line(line, Stroke::new(2.5, blue)));
        for w in m.windows(2) {
            let mid = pj.pos(LngLat::new(
                (w[0].lng + w[1].lng) / 2.0,
                (w[0].lat + w[1].lat) / 2.0,
            ));
            label(
                painter,
                mid,
                Align2::CENTER_CENTER,
                &format_distance(smf_geo::distance_m(w[0], w[1])),
                11.0,
                blue,
            );
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
