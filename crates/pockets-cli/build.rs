use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("../../apps/desktop/dist");
    println!("cargo:rerun-if-changed={}", dist_dir.display());

    let mut entries = Vec::new();
    if dist_dir.is_dir() {
        collect(&dist_dir, &dist_dir, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut generated = String::from("pub const WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    for (rel, full) in entries {
        let abs = full.to_str().expect("UTF-8 asset path");
        generated.push_str(&format!("    ({rel:?}, include_bytes!({abs:?})),\n"));
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out_dir.join("web_assets.rs"), generated).expect("write web_assets.rs");
}

fn collect(root: &Path, dir: &Path, entries: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).expect("read web dist directory") {
        let entry = entry.expect("read dist entry");
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        let kind = entry.file_type().expect("dist entry type");
        if kind.is_dir() {
            collect(root, &path, entries);
        } else if kind.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .expect("UTF-8 relative path")
                .replace('\\', "/");
            entries.push((rel, path));
        }
    }
}
