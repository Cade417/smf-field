# ShowMeFire Field

Native Rust (wgpu + egui) CalTopo-style mapping app with Show Me Fire data built in.
Separate repo from the other ShowMeFire folders (they are intentionally not a monorepo).

See the crate roles in `crates/*`. Only `smf-app` depends on egui; tokio/reqwest/fs stay
behind traits so geo/features/render/terrain can later compile to WASM.

```
cargo run -p smf-app                  # desktop app
cargo run -p xtask -- check-shaders   # validate WGSL
```

Show Me Fire API: prod `https://api.showmefire.org`, dev `http://localhost:8000` (reuse a running dev server).
