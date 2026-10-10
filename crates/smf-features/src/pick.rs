//! Screen-space hit testing and snapping. Features are projected through a
//! caller-supplied closure so this stays independent of the camera type.

use crate::model::*;
use smf_geo::{LngLat, geodesic_circle};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: Id,
    /// Set when the pointer is on a vertex handle rather than the body.
    pub vertex: Option<usize>,
}

/// Projected polylines for a feature (closed shapes repeat the first point).
pub fn screen_paths(g: &Geometry, proj: &dyn Fn(LngLat) -> [f64; 2]) -> Vec<Vec<[f64; 2]>> {
    match g {
        Geometry::Marker(p) | Geometry::Label(p) => vec![vec![proj(*p)]],
        Geometry::Line(v) => vec![v.iter().map(|p| proj(*p)).collect()],
        Geometry::Polygon(v) => {
            let mut path: Vec<_> = v.iter().map(|p| proj(*p)).collect();
            if let Some(f) = path.first().copied() {
                path.push(f);
            }
            vec![path]
        }
        Geometry::RangeRing { center, radii_m } => radii_m
            .iter()
            .map(|r| {
                geodesic_circle(*center, *r, 96)
                    .iter()
                    .map(|p| proj(*p))
                    .collect()
            })
            .collect(),
    }
}

fn dist_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
}

fn point_in_polygon(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = poly.len().wrapping_sub(1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Topmost feature under `pt` (screen px): the selected feature's vertex handles
/// first, then edges/markers (later-drawn features win), then polygon interiors.
pub fn pick(
    project: &Project,
    selected: Option<Id>,
    proj: &dyn Fn(LngLat) -> [f64; 2],
    pt: [f64; 2],
    tol: f64,
) -> Option<Hit> {
    if let Some(sel) = selected.and_then(|id| project.feature(id))
        && project.is_visible(sel)
    {
        for (i, v) in sel.geometry.vertices().iter().enumerate() {
            let s = proj(*v);
            if (s[0] - pt[0]).hypot(s[1] - pt[1]) <= tol * 1.4 {
                return Some(Hit {
                    id: sel.id,
                    vertex: Some(i),
                });
            }
        }
    }
    let mut interior: Option<Hit> = None;
    for f in project
        .features
        .iter()
        .rev()
        .filter(|f| project.is_visible(f))
    {
        let paths = screen_paths(&f.geometry, proj);
        let on_edge = match &f.geometry {
            Geometry::Marker(_) | Geometry::Label(_) => {
                let s = paths[0][0];
                // Markers draw as pins above the point; accept the pin body too.
                (s[0] - pt[0]).hypot(s[1] - pt[1]) <= tol * 1.5
                    || ((s[0] - pt[0]).abs() <= tol && pt[1] < s[1] && pt[1] > s[1] - 24.0)
            }
            _ => paths.iter().any(|path| {
                path.windows(2)
                    .any(|w| dist_to_segment(pt, w[0], w[1]) <= tol)
            }),
        };
        if on_edge {
            return Some(Hit {
                id: f.id,
                vertex: None,
            });
        }
        if interior.is_none()
            && matches!(f.geometry, Geometry::Polygon(_))
            && point_in_polygon(pt, &paths[0])
        {
            interior = Some(Hit {
                id: f.id,
                vertex: None,
            });
        }
    }
    interior
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoHit {
    /// On a marker, a line, or a polygon outline.
    Edge,
    /// Inside a polygon but not near its outline.
    Interior,
}

/// How the pointer relates to a single geometry (used to pick live data layers, which are not project features).
pub fn geometry_hit(
    g: &Geometry,
    proj: &dyn Fn(LngLat) -> [f64; 2],
    pt: [f64; 2],
    tol: f64,
) -> Option<GeoHit> {
    let paths = screen_paths(g, proj);
    match g {
        Geometry::Marker(_) | Geometry::Label(_) => {
            let s = paths[0][0];
            ((s[0] - pt[0]).hypot(s[1] - pt[1]) <= tol * 1.5).then_some(GeoHit::Edge)
        }
        _ => {
            if paths.iter().any(|path| {
                path.windows(2)
                    .any(|w| dist_to_segment(pt, w[0], w[1]) <= tol)
            }) {
                Some(GeoHit::Edge)
            } else if matches!(g, Geometry::Polygon(_)) && point_in_polygon(pt, &paths[0]) {
                Some(GeoHit::Interior)
            } else {
                None
            }
        }
    }
}

/// Nearest existing vertex within `tol` px, for snapping new points to old ones.
pub fn snap(
    project: &Project,
    proj: &dyn Fn(LngLat) -> [f64; 2],
    pt: [f64; 2],
    tol: f64,
) -> Option<LngLat> {
    let mut best: Option<(f64, LngLat)> = None;
    for f in project.features.iter().filter(|f| project.is_visible(f)) {
        for v in f.geometry.vertices() {
            let s = proj(v);
            let d = (s[0] - pt[0]).hypot(s[1] - pt[1]);
            if d <= tol && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, v));
            }
        }
    }
    best.map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Editor;

    // 1 degree == 100 px keeps the maths easy to check by eye.
    fn proj(p: LngLat) -> [f64; 2] {
        [p.lng * 100.0, -p.lat * 100.0]
    }

    fn setup() -> (Editor, Id, Id, Id) {
        let mut e = Editor::default();
        let m = e.add(Geometry::Marker(LngLat::new(5.0, 5.0)), None);
        let l = e.add(
            Geometry::Line(vec![LngLat::new(0.0, 0.0), LngLat::new(2.0, 0.0)]),
            None,
        );
        let p = e.add(
            Geometry::Polygon(vec![
                LngLat::new(10.0, 0.0),
                LngLat::new(12.0, 0.0),
                LngLat::new(12.0, 2.0),
                LngLat::new(10.0, 2.0),
            ]),
            None,
        );
        (e, m, l, p)
    }

    #[test]
    fn picks_each_kind() {
        let (e, m, l, p) = setup();
        let pr = &proj;
        assert_eq!(
            pick(&e.project, None, pr, [500.0, -500.0], 6.0).unwrap().id,
            m
        );
        assert_eq!(pick(&e.project, None, pr, [100.0, 3.0], 6.0).unwrap().id, l);
        assert_eq!(
            pick(&e.project, None, pr, [1100.0, -100.0], 6.0)
                .unwrap()
                .id,
            p,
            "polygon interior"
        );
        assert!(pick(&e.project, None, pr, [700.0, 300.0], 6.0).is_none());
    }

    #[test]
    fn selected_vertex_handle_wins() {
        let (e, _, l, _) = setup();
        let h = pick(&e.project, Some(l), &proj, [201.0, 1.0], 6.0).unwrap();
        assert_eq!(
            h,
            Hit {
                id: l,
                vertex: Some(1)
            }
        );
    }

    #[test]
    fn hidden_features_are_not_pickable() {
        let (mut e, m, _, _) = setup();
        let mut f = e.project.feature(m).unwrap().clone();
        f.visible = false;
        e.replace(f);
        assert!(pick(&e.project, None, &proj, [500.0, -500.0], 6.0).is_none());
    }

    #[test]
    fn snaps_to_nearest_vertex() {
        let (e, _, _, _) = setup();
        let s = snap(&e.project, &proj, [203.0, 2.0], 8.0).unwrap();
        assert_eq!(s, LngLat::new(2.0, 0.0));
        assert!(snap(&e.project, &proj, [350.0, 200.0], 8.0).is_none());
    }

    #[test]
    fn geometry_hit_distinguishes_edge_interior_and_miss() {
        let poly = Geometry::Polygon(vec![
            LngLat::new(0.0, 0.0),
            LngLat::new(4.0, 0.0),
            LngLat::new(4.0, 4.0),
            LngLat::new(0.0, 4.0),
        ]);
        let pr = &proj;
        assert_eq!(
            geometry_hit(&poly, pr, [200.0, 0.0], 5.0),
            Some(GeoHit::Edge),
            "on the outline"
        );
        assert_eq!(
            geometry_hit(&poly, pr, [200.0, -200.0], 5.0),
            Some(GeoHit::Interior)
        );
        assert_eq!(geometry_hit(&poly, pr, [900.0, -200.0], 5.0), None);
        let m = Geometry::Marker(LngLat::new(1.0, 1.0));
        assert_eq!(
            geometry_hit(&m, pr, [104.0, -98.0], 5.0),
            Some(GeoHit::Edge)
        );
        assert_eq!(geometry_hit(&m, pr, [150.0, -100.0], 5.0), None);
    }

    #[test]
    fn range_ring_edge_is_pickable() {
        let mut e = Editor::default();
        let c = LngLat::new(-92.0, 38.0);
        let id = e.add(
            Geometry::RangeRing {
                center: c,
                radii_m: vec![5000.0],
            },
            None,
        );
        let east = smf_geo::destination(c, 90.0, 5000.0);
        let pr = |p: LngLat| [(p.lng + 92.0) * 5000.0, -(p.lat - 38.0) * 5000.0];
        let s = pr(east);
        assert_eq!(pick(&e.project, None, &pr, s, 6.0).unwrap().id, id);
    }
}
