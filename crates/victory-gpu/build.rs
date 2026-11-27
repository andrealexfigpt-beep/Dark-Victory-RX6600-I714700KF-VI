//! Fingerprints the shader translator, so that cached translations
//! (`src/shader_cache.rs`) last until the translator itself changes and not
//! only until the next build of anything.

use std::path::{Path, PathBuf};

fn mesh_launcher(here: &Path) {
    let source = here.join("src/launch.hlsl");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=VICTORY_PC_DXC");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("launch.dxil");
    let dxc = std::env::var_os("VICTORY_PC_DXC").map(PathBuf::from).or_else(|| {
        std::fs::read_dir("C:/Program Files (x86)/Windows Kits/10/bin")
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let version: Vec<u32> = entry.file_name().to_str()?.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
                let dxc = entry.path().join("x64/dxc.exe");
                dxc.is_file().then_some((version, dxc))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, dxc)| dxc)
    });
    let Some(dxc) = dxc else {
        // Core builds work without the SDK. The experimental path then stays off.
        std::fs::write(out, []).expect("write empty mesh launcher");
        return;
    };
    let status = std::process::Command::new(&dxc)
        .args(["-nologo", "-T", "as_6_6", "-E", "main", "-Fo"])
        .arg(out)
        .arg(source)
        .status()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", dxc.display()));
    assert!(status.success(), "could not compile the mesh launcher with {}", dxc.display());
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    let here = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    mesh_launcher(&here);
    let crates = here.join("..");
    let root = crates.join("..");
    // The translator, what it is built from, and the form entries are kept in.
    let mut all = vec![
        crates.join("victory-shader/Cargo.toml"),
        here.join("src/shader_cache.rs"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let source = crates.join("victory-shader/src");
    println!("cargo:rerun-if-changed={}", source.display());
    files(&source, &mut all);
    all.sort();
    let mut hash = blake3::Hasher::new();
    for path in &all {
        println!("cargo:rerun-if-changed={}", path.display());
        // Names and line endings as they are in every checkout.
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bytes: Vec<u8> = std::fs::read(path).unwrap_or_default().into_iter().filter(|&b| b != 13).collect();
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=VICTORY_TRANSLATOR={}", hash.finalize().to_hex());
}
