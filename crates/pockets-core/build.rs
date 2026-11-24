fn main() {
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
