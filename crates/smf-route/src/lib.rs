//! Trail routing for "snap to trails": OpenStreetMap paths are loaded from the public
//! Overpass service, turned into a graph, and the shortest walk between two clicked points
//! is returned. Pure logic plus one blocking fetch; no UI.

use serde_json::Value;
use smf_geo::{LngLat, distance_m};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::Duration;

/// Public Overpass servers, tried in order: any one of them is often busy.
const OVERPASS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
];
/// Highway kinds a person can walk and that CalTopo-style routing follows.
const TRAIL_KINDS: &str = "path|footway|track|bridleway|steps|cycleway";
const M_PER_DEG: f64 = 111_320.0;
/// A routed walk that is far longer than the straight line is almost certainly the wrong trail.
const DETOUR_FACTOR: f64 = 4.0;
const DETOUR_SLACK_M: f64 = 500.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Way {
    /// OSM node ids when the source provided them; shared ids connect ways at junctions.
    pub nodes: Vec<i64>,
    pub pts: Vec<LngLat>,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub pts: Vec<LngLat>,
    pub length_m: f64,
}

/// The nearest point on a trail to a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snap {
    pub pos: LngLat,
    pub dist_m: f64,
    way: usize,
    seg: usize,
    t: f64,
}

pub fn overpass_query(bbox: [f64; 4]) -> String {
    format!(
        "[out:json][timeout:25];way[\"highway\"~\"^({TRAIL_KINDS})$\"]({:.6},{:.6},{:.6},{:.6});out geom;",
        bbox[1], bbox[0], bbox[3], bbox[2]
    )
}

pub fn parse_overpass(text: &str) -> Result<Vec<Way>, String> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("unreadable trail data ({e})"))?;
    let els = v
        .get("elements")
        .and_then(Value::as_array)
        .ok_or("the trail service returned no element list (it may be busy)")?;
    let mut out = vec![];
    for e in els {
        if e.get("type").and_then(Value::as_str) != Some("way") {
            continue;
        }
        let Some(geom) = e.get("geometry").and_then(Value::as_array) else {
            continue;
        };
        let pts: Vec<LngLat> = geom
            .iter()
            .filter_map(|p| {
                Some(LngLat::new(
                    p.get("lon")?.as_f64()?,
                    p.get("lat")?.as_f64()?,
                ))
            })
            .collect();
        if pts.len() < 2 || pts.len() != geom.len() {
            continue;
        }
        let ids: Vec<i64> = e
            .get("nodes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .filter(|ids: &Vec<i64>| ids.len() == pts.len())
            .unwrap_or_else(|| pts.iter().map(coord_key).collect());
        let tag = |k: &str| {
            e.pointer(&format!("/tags/{k}"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        out.push(Way {
            nodes: ids,
            pts,
            name: tag("name"),
            kind: tag("highway"),
        });
    }
    Ok(out)
}

/// Stand-in node id from coordinates (1e-7 degrees, about 1 cm) when ids are missing.
fn coord_key(p: &LngLat) -> i64 {
    let x = (p.lng * 1e7).round() as i64;
    let y = (p.lat * 1e7).round() as i64;
    // Negative ids can't collide with real OSM ids.
    -((x.wrapping_mul(0x9E37_79B1).wrapping_add(y)).abs() + 1)
}

pub fn fetch_trails(bbox: [f64; 4]) -> Result<Vec<Way>, String> {
    let http = reqwest::blocking::Client::builder()
        // The public Overpass servers ask clients to identify themselves.
        .user_agent(concat!("ShowMeFireField/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(40))
        .build()
        .map_err(|e| e.to_string())?;
    let mut last = String::from("no trail service is configured");
    for url in OVERPASS {
        match http
            .get(url)
            .query(&[("data", overpass_query(bbox))])
            .send()
        {
            Ok(r) if r.status().is_success() => {
                match r
                    .text()
                    .map_err(|e| e.to_string())
                    .and_then(|t| parse_overpass(&t))
                {
                    Ok(w) => return Ok(w),
                    Err(e) => last = e,
                }
            }
            Ok(r) => last = format!("the trail service answered {}", r.status().as_u16()),
            Err(e) => last = format!("could not reach the trail service ({})", e.without_url()),
        }
    }
    Err(last)
}

struct Edge {
    to: usize,
    len: f64,
}

pub struct TrailGraph {
    ways: Vec<Way>,
    pos: Vec<LngLat>,
    adj: Vec<Vec<Edge>>,
    /// For each way, the graph node index of each of its points.
    way_nodes: Vec<Vec<usize>>,
}

#[derive(PartialEq)]
struct Item(f64, usize);
impl Eq for Item {}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> Ordering {
        // Reversed so the smallest distance pops first.
        o.0.partial_cmp(&self.0).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl TrailGraph {
    pub fn new(ways: Vec<Way>) -> Self {
        let mut index: HashMap<i64, usize> = HashMap::new();
        let mut pos: Vec<LngLat> = vec![];
        let mut adj: Vec<Vec<Edge>> = vec![];
        let mut way_nodes = vec![];
        for w in &ways {
            let mut ids = Vec::with_capacity(w.pts.len());
            for (id, p) in w.nodes.iter().zip(&w.pts) {
                let i = *index.entry(*id).or_insert_with(|| {
                    pos.push(*p);
                    adj.push(vec![]);
                    pos.len() - 1
                });
                ids.push(i);
            }
            for pair in ids.windows(2) {
                if pair[0] == pair[1] {
                    continue;
                }
                let len = distance_m(pos[pair[0]], pos[pair[1]]);
                adj[pair[0]].push(Edge { to: pair[1], len });
                adj[pair[1]].push(Edge { to: pair[0], len });
            }
            way_nodes.push(ids);
        }
        Self {
            ways,
            pos,
            adj,
            way_nodes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ways.is_empty()
    }

    pub fn way_count(&self) -> usize {
        self.ways.len()
    }

    /// Nearest trail point within `max_m` metres of `p`.
    pub fn snap(&self, p: LngLat, max_m: f64) -> Option<Snap> {
        let kx = p.lat.to_radians().cos() * M_PER_DEG;
        let mut best: Option<Snap> = None;
        for (wi, w) in self.ways.iter().enumerate() {
            for (si, pair) in w.pts.windows(2).enumerate() {
                let a = (
                    (pair[0].lng - p.lng) * kx,
                    (pair[0].lat - p.lat) * M_PER_DEG,
                );
                let b = (
                    (pair[1].lng - p.lng) * kx,
                    (pair[1].lat - p.lat) * M_PER_DEG,
                );
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let len2 = dx * dx + dy * dy;
                let t = if len2 < 1e-12 {
                    0.0
                } else {
                    ((-a.0 * dx - a.1 * dy) / len2).clamp(0.0, 1.0)
                };
                let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
                let d = qx.hypot(qy);
                if d <= max_m && best.is_none_or(|s| d < s.dist_m) {
                    best = Some(Snap {
                        pos: LngLat::new(
                            pair[0].lng + (pair[1].lng - pair[0].lng) * t,
                            pair[0].lat + (pair[1].lat - pair[0].lat) * t,
                        ),
                        dist_m: d,
                        way: wi,
                        seg: si,
                        t,
                    });
                }
            }
        }
        best
    }

    /// Shortest walk along trails between the trail points nearest `a` and `b` (each within
    /// `max_snap_m`). The returned line starts and ends on trail, not on the raw clicks.
    pub fn route(&self, a: LngLat, b: LngLat, max_snap_m: f64) -> Option<Route> {
        let (sa, sb) = (self.snap(a, max_snap_m)?, self.snap(b, max_snap_m)?);
        let n = self.pos.len();
        // Virtual nodes n (start) and n+1 (end) sit on their segments.
        let (va, vb) = (n, n + 1);
        let mut extra: HashMap<usize, Vec<Edge>> = HashMap::new();
        let mut add = |v: usize, s: &Snap| {
            let nodes = &self.way_nodes[s.way];
            let (p, q) = (nodes[s.seg], nodes[s.seg + 1]);
            let w = &self.ways[s.way];
            let seg_len = distance_m(w.pts[s.seg], w.pts[s.seg + 1]);
            extra.entry(v).or_default().push(Edge {
                to: p,
                len: seg_len * s.t,
            });
            extra.entry(v).or_default().push(Edge {
                to: q,
                len: seg_len * (1.0 - s.t),
            });
            extra.entry(p).or_default().push(Edge {
                to: v,
                len: seg_len * s.t,
            });
            extra.entry(q).or_default().push(Edge {
                to: v,
                len: seg_len * (1.0 - s.t),
            });
            seg_len
        };
        let seg_a = add(va, &sa);
        let seg_b = add(vb, &sb);
        // Same segment: walk straight along it without going through its end nodes.
        if sa.way == sb.way && sa.seg == sb.seg {
            let len = seg_a.max(seg_b) * (sa.t - sb.t).abs();
            return Some(Route {
                pts: vec![sa.pos, sb.pos],
                length_m: len,
            });
        }

        let at = |i: usize| -> LngLat {
            if i == va {
                sa.pos
            } else if i == vb {
                sb.pos
            } else {
                self.pos[i]
            }
        };
        let limit = distance_m(sa.pos, sb.pos) * DETOUR_FACTOR + DETOUR_SLACK_M;
        let mut dist = vec![f64::INFINITY; n + 2];
        let mut prev = vec![usize::MAX; n + 2];
        let mut heap = BinaryHeap::new();
        dist[va] = 0.0;
        heap.push(Item(0.0, va));
        while let Some(Item(d, u)) = heap.pop() {
            if u == vb {
                break;
            }
            if d > dist[u] || d > limit {
                continue;
            }
            let base: &[Edge] = if u < n { &self.adj[u] } else { &[] };
            let ex: &[Edge] = extra.get(&u).map(Vec::as_slice).unwrap_or(&[]);
            for e in base.iter().chain(ex) {
                let nd = d + e.len;
                if nd < dist[e.to] {
                    dist[e.to] = nd;
                    prev[e.to] = u;
                    heap.push(Item(nd, e.to));
                }
            }
        }
        if !dist[vb].is_finite() || dist[vb] > limit {
            return None;
        }
        let mut path = vec![vb];
        while *path.last()? != va {
            let p = prev[*path.last()?];
            if p == usize::MAX {
                return None;
            }
            path.push(p);
        }
        path.reverse();
        Some(Route {
            pts: path.into_iter().map(at).collect(),
            length_m: dist[vb],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/overpass_trails.json");

    fn way(id0: i64, pts: &[(f64, f64)]) -> Way {
        Way {
            nodes: (id0..id0 + pts.len() as i64).collect(),
            pts: pts.iter().map(|p| LngLat::new(p.0, p.1)).collect(),
            name: String::new(),
            kind: "path".into(),
        }
    }

    /// An "H": two parallel north-south trails joined by a cross trail, with the junction
    /// nodes shared by id (the cross trail's first/last ids are the verticals' mid nodes).
    fn h_graph() -> TrailGraph {
        let left = way(1, &[(0.0, 0.0), (0.0, 0.001), (0.0, 0.002)]);
        let right = way(10, &[(0.001, 0.0), (0.001, 0.001), (0.001, 0.002)]);
        let cross = Way {
            nodes: vec![2, 11],
            pts: vec![LngLat::new(0.0, 0.001), LngLat::new(0.001, 0.001)],
            name: String::new(),
            kind: "path".into(),
        };
        TrailGraph::new(vec![left, right, cross])
    }

    #[test]
    fn parses_the_real_overpass_reply() {
        let ways = parse_overpass(FIXTURE).unwrap();
        assert_eq!(ways.len(), 12);
        assert!(
            ways.iter()
                .all(|w| w.nodes.len() == w.pts.len() && w.pts.len() >= 2)
        );
        assert!(ways.iter().all(|w| w.kind == "footway"));
        assert!(parse_overpass("{}").is_err());
        assert!(parse_overpass("<html>busy</html>").is_err());
        let q = overpass_query([-92.2, 38.5, -92.1, 38.6]);
        assert!(
            q.contains("(38.500000,-92.200000,38.600000,-92.100000)"),
            "{q}"
        );
    }

    #[test]
    fn real_trails_snap_and_route() {
        let g = TrailGraph::new(parse_overpass(FIXTURE).unwrap());
        assert_eq!(g.way_count(), 12);
        // A point on the first way snaps to itself at zero distance.
        let w = parse_overpass(FIXTURE).unwrap();
        let on = w[2].pts[3];
        let s = g.snap(on, 5.0).unwrap();
        assert!(s.dist_m < 0.5, "{}", s.dist_m);
        // Far from any trail: nothing, rather than a wild snap.
        assert!(g.snap(LngLat::new(-92.5, 38.0), 50.0).is_none());
        // Two points on the same long way route along it and are no shorter than the chord.
        let (a, b) = (w[2].pts[1], w[2].pts[18]);
        let r = g.route(a, b, 5.0).unwrap();
        assert!(r.length_m + 0.5 >= distance_m(a, b));
        assert!(r.pts.len() >= 10, "{}", r.pts.len());
    }

    #[test]
    fn routes_through_junctions_and_picks_the_short_way() {
        let g = h_graph();
        // From the bottom of the left trail to the top of the right trail: up, across, up.
        let a = LngLat::new(0.0, 0.0);
        let b = LngLat::new(0.001, 0.002);
        let r = g.route(a, b, 10.0).unwrap();
        let straight = distance_m(a, b);
        assert!(r.length_m > straight, "must follow trails, not cut across");
        // The only way is 0.002 deg north + 0.001 deg east of walking.
        let expect = distance_m(LngLat::new(0.0, 0.0), LngLat::new(0.0, 0.002))
            + distance_m(LngLat::new(0.0, 0.001), LngLat::new(0.001, 0.001));
        assert!(
            (r.length_m - expect).abs() < 1.0,
            "{} vs {expect}",
            r.length_m
        );
        assert_eq!(r.pts.first().copied(), Some(a));
        assert_eq!(r.pts.last().copied(), Some(b));
        // Every hop of the result lies on a trail.
        for p in &r.pts {
            assert!(g.snap(*p, 0.5).is_some(), "{p:?} is off-trail");
        }
    }

    #[test]
    fn clicks_off_trail_are_snapped_and_far_ones_refused() {
        let g = h_graph();
        // 5 m east of the left trail still snaps onto it.
        let near = LngLat::new(0.00005, 0.0005);
        let s = g.snap(near, 20.0).unwrap();
        assert!(s.dist_m < 6.0 && s.pos.lng.abs() < 1e-9);
        assert!(g.route(near, LngLat::new(0.001, 0.0015), 20.0).is_some());
        // Too far from the trail for the allowed snap distance.
        assert!(
            g.route(LngLat::new(0.01, 0.0), LngLat::new(0.001, 0.0015), 20.0)
                .is_none()
        );
    }

    #[test]
    fn same_segment_and_disconnected_trails() {
        let g = h_graph();
        let r = g
            .route(LngLat::new(0.0, 0.0002), LngLat::new(0.0, 0.0008), 5.0)
            .unwrap();
        let want = distance_m(LngLat::new(0.0, 0.0002), LngLat::new(0.0, 0.0008));
        assert!((r.length_m - want).abs() < 0.5, "{} vs {want}", r.length_m);
        // Two trails that never meet: no route, so the caller falls back to a straight line.
        let g2 = TrailGraph::new(vec![
            way(1, &[(0.0, 0.0), (0.0, 0.001)]),
            way(100, &[(0.0005, 0.0), (0.0005, 0.001)]),
        ]);
        assert!(
            g2.route(LngLat::new(0.0, 0.0), LngLat::new(0.0005, 0.001), 5.0)
                .is_none()
        );
        assert!(
            TrailGraph::new(vec![])
                .route(LngLat::new(0.0, 0.0), LngLat::new(0.0, 0.1), 5.0)
                .is_none()
        );
    }

    #[test]
    fn absurd_detours_are_refused() {
        // Two points 100 m apart across a gap, connected only by a trail that loops 5 km away.
        let far = 0.05;
        let g = TrailGraph::new(vec![way(
            1,
            &[(0.0, 0.0), (0.0, far), (0.001, far), (0.001, 0.0)],
        )]);
        assert!(
            g.route(LngLat::new(0.0, 0.0), LngLat::new(0.001, 0.0), 5.0)
                .is_none()
        );
    }
}
