use std::{fs, path::PathBuf};

use perry_wit::component::wit::resolve_wit;

#[test]
fn local_dependencies_are_combined_with_missing_ambient_wasi_packages() {
    let root = std::env::temp_dir().join(format!("perry-wit-resolution-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("deps")).unwrap();
    fs::write(
        root.join("world.wit"),
        "package test:resolution; world test { import wasi:http/client@0.3.0; }",
    )
    .unwrap();
    assert!(
        resolve_wit(&root).is_ok(),
        "an empty deps directory must still load ambient WASI"
    );

    fs::create_dir_all(root.join("deps/custom")).unwrap();
    fs::write(root.join("deps/custom/package.wit"), "package test:custom; interface api { use wasi:clocks/types@0.3.0.{duration}; wait: async func(delay: duration); }").unwrap();
    fs::write(root.join("world.wit"), "package test:resolution; world test { import test:custom/api; import wasi:http/client@0.3.0; }").unwrap();
    let ambient = std::env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("wit/deps"));
    fs::create_dir_all(root.join("deps/clocks")).unwrap();
    for file in fs::read_dir(ambient.join("clocks")).unwrap() {
        let path = file.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "wit") {
            fs::copy(
                &path,
                root.join("deps/clocks").join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }
    fs::write(
        root.join("deps/clocks/local.wit"),
        "package wasi:clocks@0.3.0; interface local-marker {}",
    )
    .unwrap();
    let (resolve, _) = resolve_wit(&root).unwrap();
    let clocks_package = resolve
        .packages
        .iter()
        .find(|(_, package)| package.name.namespace == "wasi" && package.name.name == "clocks")
        .unwrap()
        .1;
    assert!(
        clocks_package.interfaces.contains_key("local-marker"),
        "local packages must take precedence over ambient packages"
    );
    assert_eq!(
        resolve
            .packages
            .iter()
            .filter(|(_, package)| package.name.namespace == "wasi" && package.name.name == "clocks")
            .count(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}
