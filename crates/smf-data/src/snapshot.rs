//! Turn live data into ordinary map features so it can be edited, exported, printed and used offline.

use crate::layers::{DataFeature, DataLayerKind, summary};
use smf_features::{Feature, Style};

/// Features for `kind`, ready for `Editor::add_features`, plus the folder name to file them under.
pub fn snapshot_features(
    features: &[DataFeature],
    kind: DataLayerKind,
    date: &str,
) -> (String, Vec<Feature>) {
    let folder = format!("{} snapshot {date}", kind.label());
    let out = features
        .iter()
        .filter(|f| f.kind == kind)
        .map(|f| Feature {
            id: 0,
            name: f.title.clone(),
            geometry: f.geometry.clone(),
            style: Style {
                color: f.style.color,
                width: f.style.width.max(1.0),
                fill_alpha: if matches!(f.geometry, smf_features::Geometry::Polygon(_)) {
                    f.style.fill_alpha.min(120)
                } else {
                    255
                },
            },
            folder: None,
            visible: true,
            notes: summary(f)
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect();
    (folder, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::parse_layer;
    use smf_features::{Editor, Geometry};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn snapshots_carry_names_notes_and_colours() {
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        let (folder, feats) =
            snapshot_features(&p.features, DataLayerKind::Incidents, "2026-10-10");
        assert_eq!(folder, "Fire incidents snapshot 2026-10-10");
        assert_eq!(feats.len(), 3);
        assert!(feats[0].name.starts_with("Fire incident, St Clair County"));
        assert!(
            feats[0]
                .notes
                .contains("Confidence this is a fire: 76% (high)"),
            "{}",
            feats[0].notes
        );
        assert_eq!(feats[0].style.color, p.features[0].style.color);
        assert!(matches!(feats[0].geometry, Geometry::Marker(_)));
    }

    #[test]
    fn snapshots_import_as_one_undo_step_into_a_project() {
        let p = parse_layer(DataLayerKind::BurnBans, &fixture("burn_bans.geojson")).unwrap();
        let (_, feats) = snapshot_features(&p.features, DataLayerKind::BurnBans, "d");
        let mut e = Editor::default();
        let ids = e.add_features(feats);
        assert_eq!(ids.len(), 2);
        assert!(matches!(
            e.project.feature(ids[0]).unwrap().geometry,
            Geometry::Polygon(_)
        ));
        e.undo();
        assert!(e.project.features.is_empty());
    }

    #[test]
    fn only_the_requested_kind_is_snapshotted() {
        let p = parse_layer(DataLayerKind::Incidents, &fixture("incidents.geojson")).unwrap();
        let (_, none) = snapshot_features(&p.features, DataLayerKind::BurnBans, "d");
        assert!(none.is_empty());
    }
}
