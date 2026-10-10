//! Repo automation. `cargo run -p xtask -- check-shaders` validates every
//! WGSL file under crates/smf-render/src/shaders with naga so shader typos fail
//! in CI instead of at runtime on someone's GPU.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("check-shaders") => check_shaders(),
        _ => bail!("usage: cargo run -p xtask -- check-shaders"),
    }
}

fn check_shaders() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/smf-render/src/shaders");
    let mut files: Vec<PathBuf> = Vec::new();
    if root.exists() {
        for entry in std::fs::read_dir(&root)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "wgsl") {
                files.push(p);
            }
        }
    }
    for f in &files {
        let src = std::fs::read_to_string(f)?;
        let module = naga::front::wgsl::parse_str(&src)
            .with_context(|| format!("parsing {}", f.display()))?;
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .map_err(|e| anyhow::anyhow!("{}: {e:?}", f.display()))?;
        println!("ok  {}", f.display());
    }
    println!("{} shader(s) validated", files.len());
    Ok(())
}
