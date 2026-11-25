fn main() {
    println!("cargo:rerun-if-env-changed=POCKETS_VERSION");
    let raw_version = std::env::var("POCKETS_VERSION")
        .ok()
        .map(|v| v.trim().trim_start_matches('v').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    println!("cargo:rustc-env=POCKETS_APP_VERSION={raw_version}");

    // Bundle the same reviewed text starter used by pocket-tools. No Node
    // runtime, repository checkout or template downloads are needed at runtime.
    let template = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../templates/basic-pocket");
    println!("cargo:rerun-if-changed={}", template.display());
    let mut files = Vec::new();
    collect(&template, &template, &mut files);
    files.sort();
    let mut generated = String::from("const TEMPLATE_FILES: &[(&str, &str)] = &[\n");
    for (path, contents) in files {
        generated.push_str(&format!("({path:?}, {contents:?}),\n"));
    }
    generated.push_str("];\n");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(output.join("basic_pocket.rs"), generated).unwrap();
}

fn collect(root: &std::path::Path, dir: &std::path::Path, files: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("read bundled starter") {
        let entry = entry.expect("read starter entry");
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        let kind = entry.file_type().expect("starter entry type");
        if kind.is_dir() {
            collect(root, &path, files);
        } else {
            assert!(
                kind.is_file(),
                "starter must contain only regular text files"
            );
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            files.push((
                relative,
                std::fs::read_to_string(&path).expect("starter text"),
            ));
        }
    }
}
