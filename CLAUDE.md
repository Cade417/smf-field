# ShowMeFire Field conventions
- Pin egui/eframe/wgpu as a set; use only `eframe::wgpu`.
- World math in f64 on the CPU; send camera-relative f32 to the GPU.
- Keep egui, tokio, reqwest and std::fs out of smf-geo/features/render/terrain.
- Comments explain *why* (match the other ShowMeFire repos). Group imports.
- Every tile source has an `offline_allowed` flag; never bulk-download restricted sources.
- Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- eframe 0.36 API: `App::ui(&mut Ui, &mut Frame)`, `CentralPanel::show(ui, ..)`.
