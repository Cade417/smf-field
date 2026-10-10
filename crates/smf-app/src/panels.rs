//! Right-hand panel: layer/feature tree and the properties editor for the selection.

use eframe::egui::{self, Color32, RichText};
use smf_features::{Editor, Feature, Geometry, Id};
use smf_geo::{format_area, format_distance, path_length_m, polygon_area_m2};

pub struct PanelAction {
    pub zoom_to: Option<Id>,
}

pub fn side_panel(ui: &mut egui::Ui, editor: &mut Editor) -> PanelAction {
    let mut action = PanelAction { zoom_to: None };
    ui.horizontal(|ui| {
        ui.heading("Map");
        if ui.small_button("+ Folder").clicked() {
            let n = editor.project.folders.len() + 1;
            editor.project.add_folder(&format!("Folder {n}"));
            editor.revision += 1;
        }
    });
    ui.add(egui::TextEdit::singleline(&mut editor.project.name).desired_width(f32::INFINITY));
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("features")
        .max_height(ui.available_height() * 0.5)
        .show(ui, |ui| {
            let folders: Vec<(Id, String, bool)> = editor
                .project
                .folders
                .iter()
                .map(|f| (f.id, f.name.clone(), f.visible))
                .collect();
            for (fid, name, visible) in folders {
                let mut vis = visible;
                let mut new_name = name.clone();
                egui::CollapsingHeader::new(RichText::new(&name).strong())
                    .id_salt(("folder", fid))
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut vis, "visible");
                            ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(120.0));
                        });
                        feature_rows(ui, editor, Some(fid), &mut action);
                    });
                if vis != visible || new_name != name {
                    if let Some(f) = editor.project.folders.iter_mut().find(|f| f.id == fid) {
                        f.visible = vis;
                        f.name = new_name;
                    }
                    editor.revision += 1;
                }
            }
            feature_rows(ui, editor, None, &mut action);
            if editor.project.features.is_empty() {
                ui.weak("Nothing yet. Pick a tool and click the map.");
            }
        });
    ui.separator();
    properties(ui, editor, &mut action);
    action
}

fn feature_rows(
    ui: &mut egui::Ui,
    editor: &mut Editor,
    folder: Option<Id>,
    action: &mut PanelAction,
) {
    let rows: Vec<(Id, String, bool, &'static str)> = editor
        .project
        .features
        .iter()
        .filter(|f| f.folder == folder)
        .map(|f| (f.id, f.name.clone(), f.visible, f.geometry.kind_name()))
        .collect();
    for (id, name, visible, kind) in rows {
        ui.horizontal(|ui| {
            let mut vis = visible;
            if ui.checkbox(&mut vis, "").changed()
                && let Some(mut f) = editor.project.feature(id).cloned()
            {
                f.visible = vis;
                editor.replace(f);
            }
            let text = if name.is_empty() {
                format!("({kind})")
            } else {
                name
            };
            let r = ui
                .selectable_label(editor.selection == Some(id), text)
                .on_hover_text(kind);
            if r.clicked() {
                editor.selection = Some(id);
            }
            if r.double_clicked() {
                action.zoom_to = Some(id);
            }
        });
    }
}

fn properties(ui: &mut egui::Ui, editor: &mut Editor, action: &mut PanelAction) {
    let Some(orig) = editor
        .selection
        .and_then(|id| editor.project.feature(id))
        .cloned()
    else {
        ui.weak("Select a feature to edit its properties.");
        return;
    };
    let mut f: Feature = orig.clone();
    let mut active = false; // any widget being typed in / dragged => coalesce undo
    let note = |r: &egui::Response, active: &mut bool| {
        *active |= r.has_focus() || r.dragged();
    };

    ui.label(RichText::new(f.geometry.kind_name()).weak());
    let r = ui.add(
        egui::TextEdit::singleline(&mut f.name)
            .desired_width(f32::INFINITY)
            .hint_text("Name"),
    );
    note(&r, &mut active);

    ui.horizontal(|ui| {
        ui.label("Color");
        let r = ui.color_edit_button_srgb(&mut f.style.color);
        note(&r, &mut active);
        ui.label("Width");
        let r = ui.add(egui::Slider::new(&mut f.style.width, 1.0..=10.0));
        note(&r, &mut active);
    });
    if matches!(f.geometry, Geometry::Polygon(_)) {
        ui.horizontal(|ui| {
            ui.label("Fill");
            let r = ui.add(egui::Slider::new(&mut f.style.fill_alpha, 0..=255));
            note(&r, &mut active);
        });
    }

    if let Geometry::RangeRing { radii_m, .. } = &mut f.geometry {
        ui.label("Radii (m)");
        let mut remove = None;
        for (i, r) in radii_m.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::DragValue::new(r)
                        .range(1.0..=2_000_000.0)
                        .speed(10.0)
                        .suffix(" m"),
                );
                note(&resp, &mut active);
                if ui.small_button("x").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove
            && radii_m.len() > 1
        {
            radii_m.remove(i);
        }
        if ui.small_button("+ ring").clicked() {
            let next = radii_m.last().copied().unwrap_or(1000.0) * 2.0;
            radii_m.push(next);
        }
    }

    let folder_name = |id: Option<Id>| {
        id.and_then(|i| editor.project.folders.iter().find(|x| x.id == i))
            .map_or("(none)".to_string(), |x| x.name.clone())
    };
    egui::ComboBox::from_label("Folder")
        .selected_text(folder_name(f.folder))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut f.folder, None, "(none)");
            for fo in &editor.project.folders {
                ui.selectable_value(&mut f.folder, Some(fo.id), &fo.name);
            }
        });

    ui.label("Notes");
    let r = ui.add(
        egui::TextEdit::multiline(&mut f.notes)
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    note(&r, &mut active);

    match &f.geometry {
        Geometry::Line(v) => {
            ui.label(format!("Length: {}", format_distance(path_length_m(v))));
        }
        Geometry::Polygon(v) => {
            let mut closed = v.clone();
            closed.push(v[0]);
            ui.label(format!(
                "Perimeter: {}",
                format_distance(path_length_m(&closed))
            ));
            ui.label(format!("Area: {}", format_area(polygon_area_m2(v))));
        }
        Geometry::Marker(p) | Geometry::Label(p) => {
            ui.label(smf_geo::format_dd(*p));
            if let Some(u) = smf_geo::format_utm(*p) {
                ui.label(u);
            }
        }
        Geometry::RangeRing { .. } => {}
    }

    ui.horizontal(|ui| {
        if ui.button("Zoom to").clicked() {
            action.zoom_to = Some(f.id);
        }
        if ui
            .add(egui::Button::new(
                RichText::new("Delete").color(Color32::from_rgb(220, 60, 60)),
            ))
            .clicked()
        {
            editor.remove(f.id);
        }
    });

    editor.coalescing = active;
    if editor.selection.is_some() {
        editor.replace(f);
    }
}
