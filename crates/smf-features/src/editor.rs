use crate::model::*;
use smf_geo::{LngLat, path_length_m, polygon_area_m2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Marker,
    Line,
    Polygon,
    RangeRing,
    Label,
    Measure,
    Freehand,
}

/// Undoable edits. `Replace` stores whole before/after features, which keeps
/// every property edit (name, style, vertices, radii) a single uniform command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Add {
        feature: Feature,
        index: usize,
    },
    Remove {
        feature: Feature,
        index: usize,
    },
    Replace {
        before: Feature,
        after: Feature,
    },
    /// Several commands undone/redone as one step (imports).
    Batch(Vec<Command>),
}

impl Command {
    fn apply(&self, p: &mut Project) {
        match self {
            Command::Batch(cmds) => cmds.iter().for_each(|c| c.apply(p)),
            Command::Add { feature, index } => {
                let i = (*index).min(p.features.len());
                p.features.insert(i, feature.clone());
            }
            Command::Remove { feature, .. } => p.features.retain(|f| f.id != feature.id),
            Command::Replace { after, .. } => {
                if let Some(f) = p.features.iter_mut().find(|f| f.id == after.id) {
                    *f = after.clone();
                }
            }
        }
    }

    fn inverse(&self) -> Command {
        match self {
            Command::Batch(cmds) => {
                Command::Batch(cmds.iter().rev().map(Command::inverse).collect())
            }
            Command::Add { feature, index } => Command::Remove {
                feature: feature.clone(),
                index: *index,
            },
            Command::Remove { feature, index } => Command::Add {
                feature: feature.clone(),
                index: *index,
            },
            Command::Replace { before, after } => Command::Replace {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

pub struct Editor {
    pub project: Project,
    pub tool: Tool,
    /// In-progress line/polygon/freehand vertices.
    pub draft: Vec<LngLat>,
    pub measure: Vec<LngLat>,
    pub selection: Option<Id>,
    /// Style applied to newly drawn features.
    pub current_style: Style,
    undo: Vec<Command>,
    redo: Vec<Command>,
    /// Snapshot taken when a drag-edit starts so one drag is one undo step.
    editing: Option<Feature>,
    /// While true, consecutive `replace` calls on one feature merge into a single
    /// undo step (typing in a text box, dragging a slider).
    pub coalescing: bool,
    /// Bumped on every change so the app knows when to autosave/redraw.
    pub revision: u64,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(Project::default())
    }
}

impl Editor {
    pub fn new(project: Project) -> Self {
        Self {
            project,
            tool: Tool::Select,
            draft: vec![],
            measure: vec![],
            selection: None,
            current_style: Style::default(),
            undo: vec![],
            redo: vec![],
            editing: None,
            coalescing: false,
            revision: 0,
        }
    }

    pub fn replace_project(&mut self, project: Project) {
        *self = Self {
            tool: self.tool,
            current_style: self.current_style,
            ..Self::new(project)
        };
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn run(&mut self, cmd: Command) {
        cmd.apply(&mut self.project);
        self.undo.push(cmd);
        self.redo.clear();
        self.revision += 1;
    }

    pub fn undo(&mut self) {
        if let Some(cmd) = self.undo.pop() {
            cmd.inverse().apply(&mut self.project);
            self.redo.push(cmd);
            self.revision += 1;
            self.fix_selection();
        }
    }

    pub fn redo(&mut self) {
        if let Some(cmd) = self.redo.pop() {
            cmd.apply(&mut self.project);
            self.undo.push(cmd);
            self.revision += 1;
            self.fix_selection();
        }
    }

    fn fix_selection(&mut self) {
        if self
            .selection
            .is_some_and(|id| self.project.feature(id).is_none())
        {
            self.selection = None;
        }
    }

    pub fn add(&mut self, geometry: Geometry, name: Option<String>) -> Id {
        let id = self.project.alloc_id();
        let name = name.unwrap_or_else(|| {
            let n = self
                .project
                .features
                .iter()
                .filter(|f| f.geometry.kind_name() == geometry.kind_name())
                .count()
                + 1;
            format!("{} {n}", geometry.kind_name())
        });
        let feature = Feature {
            id,
            name,
            geometry,
            style: self.current_style,
            folder: None,
            visible: true,
            notes: String::new(),
        };
        let index = self.project.features.len();
        self.run(Command::Add { feature, index });
        id
    }

    /// Insert an already-built feature (imports), assigning a fresh id.
    pub fn add_feature(&mut self, mut feature: Feature) -> Id {
        feature.id = self.project.alloc_id();
        let id = feature.id;
        let index = self.project.features.len();
        self.run(Command::Add { feature, index });
        id
    }

    /// Add many features as a single undo step, assigning fresh ids. Returns the new ids.
    pub fn add_features(&mut self, features: Vec<Feature>) -> Vec<Id> {
        let mut ids = Vec::with_capacity(features.len());
        let mut cmds = Vec::with_capacity(features.len());
        for mut feature in features {
            feature.id = self.project.alloc_id();
            ids.push(feature.id);
            let index = self.project.features.len() + cmds.len();
            cmds.push(Command::Add { feature, index });
        }
        if !cmds.is_empty() {
            self.run(Command::Batch(cmds));
        }
        ids
    }

    pub fn remove(&mut self, id: Id) {
        if let Some(i) = self.project.index_of(id) {
            let feature = self.project.features[i].clone();
            self.run(Command::Remove { feature, index: i });
            self.fix_selection();
        }
    }

    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selection {
            self.remove(id);
        }
    }

    /// Record a whole-feature edit as one undo step (no-op if unchanged).
    pub fn replace(&mut self, after: Feature) {
        if let Some(before) = self.project.feature(after.id).cloned()
            && before != after
        {
            if self.coalescing
                && self.redo.is_empty()
                && let Some(Command::Replace {
                    after: top_after, ..
                }) = self.undo.last_mut()
                && top_after.id == after.id
            {
                *top_after = after.clone();
                if let Some(f) = self.project.features.iter_mut().find(|f| f.id == after.id) {
                    *f = after;
                }
                self.revision += 1;
                return;
            }
            self.run(Command::Replace { before, after });
        }
    }

    pub fn begin_edit(&mut self, id: Id) {
        self.editing = self.project.feature(id).cloned();
    }

    /// Live (not yet undoable) vertex move during a drag.
    pub fn drag_vertex(&mut self, id: Id, index: usize, p: LngLat) {
        if let Some(f) = self.project.features.iter_mut().find(|f| f.id == id) {
            f.geometry.set_vertex(index, p);
            self.revision += 1;
        }
    }

    pub fn end_edit(&mut self) {
        if let Some(before) = self.editing.take()
            && let Some(after) = self.project.feature(before.id).cloned()
            && before != after
        {
            // Already applied live; just record it for undo.
            self.undo.push(Command::Replace { before, after });
            self.redo.clear();
            self.revision += 1;
        }
    }

    pub fn delete_vertex(&mut self, id: Id, index: usize) {
        if let Some(before) = self.project.feature(id).cloned() {
            let mut after = before;
            if after.geometry.remove_vertex(index) {
                self.replace(after);
            }
        }
    }

    /// A map click with the active tool. Select is handled by the caller via `pick`.
    pub fn click(&mut self, p: LngLat) {
        match self.tool {
            Tool::Select => {}
            Tool::Marker => {
                let id = self.add(Geometry::Marker(p), None);
                self.selection = Some(id);
            }
            Tool::Label => {
                let id = self.add(Geometry::Label(p), Some("Label".into()));
                self.selection = Some(id);
            }
            Tool::RangeRing => {
                let id = self.add(
                    Geometry::RangeRing {
                        center: p,
                        radii_m: default_radii(),
                    },
                    None,
                );
                self.selection = Some(id);
            }
            Tool::Line | Tool::Polygon | Tool::Freehand => self.draft.push(p),
            Tool::Measure => self.measure.push(p),
        }
    }

    /// Freehand sampling: caller throttles by screen distance.
    pub fn freehand_point(&mut self, p: LngLat) {
        if self.tool == Tool::Freehand {
            self.draft.push(p);
        }
    }

    /// Complete the in-progress shape (double-click / Enter / mouse release for freehand).
    pub fn finish(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        let geom = match self.tool {
            Tool::Line if draft.len() >= 2 => Some(Geometry::Line(draft)),
            Tool::Freehand if draft.len() >= 2 => Some(Geometry::Line(simplify(&draft, 3.0))),
            Tool::Polygon if draft.len() >= 3 => Some(Geometry::Polygon(draft)),
            _ => None,
        };
        if let Some(g) = geom {
            let id = self.add(g, None);
            self.selection = Some(id);
        }
    }

    pub fn cancel(&mut self) {
        self.draft.clear();
        self.measure.clear();
    }

    /// Total length and (for 3+ points) enclosed area of the measure tool.
    pub fn measurement(&self) -> (f64, Option<f64>) {
        let len = path_length_m(&self.measure);
        let area = (self.measure.len() >= 3).then(|| polygon_area_m2(&self.measure));
        (len, area)
    }
}

/// Douglas-Peucker in a local equirectangular frame; `tol_m` is metres.
pub fn simplify(pts: &[LngLat], tol_m: f64) -> Vec<LngLat> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let lat0 = pts[0].lat.to_radians().cos();
    let xy = |p: &LngLat| (p.lng * 111_320.0 * lat0, p.lat * 110_574.0);
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        let (ax, ay) = xy(&pts[a]);
        let (bx, by) = xy(&pts[b]);
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        let mut worst = (0.0, a);
        for (i, p) in pts.iter().enumerate().take(b).skip(a + 1) {
            let (px, py) = xy(p);
            let t = if len2 > 0.0 {
                (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let d = ((px - (ax + t * dx)).powi(2) + (py - (ay + t * dy)).powi(2)).sqrt();
            if d > worst.0 {
                worst = (d, i);
            }
        }
        if worst.0 > tol_m {
            keep[worst.1] = true;
            stack.push((a, worst.1));
            stack.push((worst.1, b));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| *p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ll(x: f64, y: f64) -> LngLat {
        LngLat::new(x, y)
    }

    #[test]
    fn draw_line_and_undo_redo() {
        let mut e = Editor {
            tool: Tool::Line,
            ..Editor::default()
        };
        e.click(ll(0.0, 0.0));
        e.click(ll(1.0, 1.0));
        assert!(e.project.features.is_empty(), "nothing until finished");
        e.finish();
        assert_eq!(e.project.features.len(), 1);
        assert_eq!(e.selection, Some(1));
        e.undo();
        assert!(e.project.features.is_empty());
        assert_eq!(e.selection, None);
        e.redo();
        assert_eq!(e.project.features.len(), 1);
    }

    #[test]
    fn too_few_points_makes_nothing() {
        let mut e = Editor {
            tool: Tool::Polygon,
            ..Editor::default()
        };
        e.click(ll(0.0, 0.0));
        e.click(ll(1.0, 0.0));
        e.finish();
        assert!(e.project.features.is_empty());
        assert!(e.draft.is_empty());
    }

    #[test]
    fn one_drag_is_one_undo_step() {
        let mut e = Editor::default();
        let id = e.add(Geometry::Marker(ll(0.0, 0.0)), None);
        e.begin_edit(id);
        for i in 1..=10 {
            e.drag_vertex(id, 0, ll(i as f64, 0.0));
        }
        e.end_edit();
        assert_eq!(
            e.project.feature(id).unwrap().geometry,
            Geometry::Marker(ll(10.0, 0.0))
        );
        e.undo();
        assert_eq!(
            e.project.feature(id).unwrap().geometry,
            Geometry::Marker(ll(0.0, 0.0))
        );
        e.undo();
        assert!(e.project.features.is_empty());
    }

    #[test]
    fn new_edit_clears_redo_and_delete_restores_position() {
        let mut e = Editor::default();
        let a = e.add(Geometry::Marker(ll(0.0, 0.0)), None);
        let b = e.add(Geometry::Marker(ll(1.0, 0.0)), None);
        let _c = e.add(Geometry::Marker(ll(2.0, 0.0)), None);
        e.remove(b);
        e.undo();
        assert_eq!(
            e.project.features.iter().map(|f| f.id).collect::<Vec<_>>(),
            vec![a, b, 3]
        );
        e.undo();
        assert!(e.can_redo());
        e.add(Geometry::Marker(ll(5.0, 5.0)), None);
        assert!(!e.can_redo());
    }

    #[test]
    fn measure_length_and_area() {
        let mut e = Editor {
            tool: Tool::Measure,
            ..Editor::default()
        };
        e.click(ll(0.0, 0.0));
        e.click(ll(1.0, 0.0));
        let (len, area) = e.measurement();
        assert!((len - 111_319.49).abs() < 1.0);
        assert!(area.is_none());
        e.click(ll(1.0, 1.0));
        assert!(e.measurement().1.unwrap() > 0.0);
        e.cancel();
        assert_eq!(e.measurement().0, 0.0);
    }

    #[test]
    fn vertex_delete_keeps_shapes_valid() {
        let mut e = Editor::default();
        let id = e.add(Geometry::Line(vec![ll(0.0, 0.0), ll(1.0, 0.0)]), None);
        e.delete_vertex(id, 0);
        assert_eq!(e.project.feature(id).unwrap().geometry.vertices().len(), 2);
        let pid = e.add(
            Geometry::Polygon(vec![ll(0.0, 0.0), ll(1.0, 0.0), ll(1.0, 1.0), ll(0.0, 1.0)]),
            None,
        );
        e.delete_vertex(pid, 3);
        assert_eq!(e.project.feature(pid).unwrap().geometry.vertices().len(), 3);
        e.delete_vertex(pid, 0);
        assert_eq!(e.project.feature(pid).unwrap().geometry.vertices().len(), 3);
    }

    #[test]
    fn coalesced_edits_are_one_undo_step() {
        let mut e = Editor::default();
        let id = e.add(Geometry::Marker(ll(0.0, 0.0)), None);
        e.coalescing = true;
        for name in ["A", "Ab", "Abc"] {
            let mut f = e.project.feature(id).unwrap().clone();
            f.name = name.into();
            e.replace(f);
        }
        e.coalescing = false;
        assert_eq!(e.project.feature(id).unwrap().name, "Abc");
        e.undo();
        assert_eq!(e.project.feature(id).unwrap().name, "Marker 1");
        assert!(e.can_undo(), "the add is still undoable separately");
    }

    #[test]
    fn batch_add_is_one_undo_step_and_keeps_order() {
        let mut e = Editor::default();
        let mk = |n: &str| Feature {
            id: 0,
            name: n.into(),
            geometry: Geometry::Marker(ll(0.0, 0.0)),
            style: Style::default(),
            folder: None,
            visible: true,
            notes: String::new(),
        };
        let ids = e.add_features(vec![mk("a"), mk("b"), mk("c")]);
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(
            e.project
                .features
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        e.undo();
        assert!(e.project.features.is_empty());
        e.redo();
        assert_eq!(e.project.features.len(), 3);
        assert!(e.add_features(vec![]).is_empty());
    }

    #[test]
    fn simplify_drops_collinear() {
        let pts: Vec<_> = (0..50).map(|i| ll(i as f64 * 1e-4, 0.0)).collect();
        assert_eq!(simplify(&pts, 3.0).len(), 2);
        let bent = vec![ll(0.0, 0.0), ll(0.01, 0.01), ll(0.02, 0.0)];
        assert_eq!(simplify(&bent, 3.0).len(), 3);
    }

    #[test]
    fn folder_visibility() {
        let mut p = Project::default();
        let fid = p.add_folder("Ops");
        let mut e = Editor::new(p);
        let id = e.add(Geometry::Marker(ll(0.0, 0.0)), None);
        let mut f = e.project.feature(id).unwrap().clone();
        f.folder = Some(fid);
        e.replace(f);
        assert!(e.project.is_visible(e.project.feature(id).unwrap()));
        e.project.folders[0].visible = false;
        assert!(!e.project.is_visible(e.project.feature(id).unwrap()));
    }
}
