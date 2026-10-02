use std::{fs, path::PathBuf};

use perry_wit::component::wit::resolve_wit;

#[test]
fn local_dependencies_are_combined_with_missing_ambient_wasi_packages() {
    let root = std::env::temp_dir().join(format!("perry-wit-resolution-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("deps")).unwrap();
    fs::write(
        root.join("world.wit"),
        "package test:resolution; world test { import wasi:http/outgoing-handler@0.2.6; }",
    )
    .unwrap();
    assert!(
        resolve_wit(&root).is_ok(),
        "an empty deps directory must still load ambient WASI"
    );

    fs::create_dir_all(root.join("deps/custom")).unwrap();
    fs::write(root.join("deps/custom/package.wit"), "package test:custom; interface api { use wasi:io/poll@0.2.6.{pollable}; wait: func(p: borrow<pollable>); }").unwrap();
    fs::write(root.join("world.wit"), "package test:resolution; world test { import test:custom/api; import wasi:http/outgoing-handler@0.2.6; }").unwrap();
    let ambient = std::env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("wit/deps"));
    fs::create_dir_all(root.join("deps/io")).unwrap();
    let mut io = String::new();
    for file in fs::read_dir(ambient.join("io")).unwrap() {
        let path = file.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "wit") {
            io.push_str(&fs::read_to_string(path).unwrap());
        }
    }
    io.push_str("\ninterface local-marker {}\n");
    fs::write(root.join("deps/io/package.wit"), io).unwrap();
    let (resolve, _) = resolve_wit(&root).unwrap();
    let io_package = resolve
        .packages
        .iter()
        .find(|(_, package)| package.name.namespace == "wasi" && package.name.name == "io")
        .unwrap()
        .1;
    assert!(
        io_package.interfaces.contains_key("local-marker"),
        "local packages must take precedence over ambient packages"
    );
    assert_eq!(
        resolve
            .packages
            .iter()
            .filter(|(_, package)| package.name.namespace == "wasi" && package.name.name == "io")
            .count(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}
