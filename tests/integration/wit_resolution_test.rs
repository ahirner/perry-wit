use crate::world::Compiler::Standard;
use perry_wit::api::{self, Package, Wit};
use std::{fs, path::PathBuf};

use perry_wit::component::wit::{resolve_source, resolve_wit};

#[test]
fn local_dependencies_are_combined_with_missing_ambient_wasi_packages() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
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
}

pub(super) fn source_and_encoded(source: &str) -> anyhow::Result<[Package; 2]> {
    let group = wit_parser::UnresolvedPackageGroup::parse("fixture.wit", source)
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
    let mut resolve = wit_parser::Resolve::new();
    let package = resolve.push_groups(group, Vec::new())?;
    Ok([
        source.as_bytes().into(),
        Package::Encoded(wit_component::encode(&resolve, package, false)?),
    ])
}

#[test]
fn partial_supplied_packages_do_not_resolve_unrelated_standards() -> anyhow::Result<()> {
    let source = "package test:producer@1.0.0;
        interface api { use wasi:clocks/system-clock@0.3.0.{instant}; read: func() -> option<instant>; }
        world producer { export api; }
        package wasi:clocks@0.3.0 {
            interface system-clock { record instant { seconds: s64, nanoseconds: u32 } }
        }";
    for dependency in source_and_encoded(source)? {
        let generate = |interface| {
            api::generate_sdk(
                &Wit {
                    application: format!(
                        "package test:consumer; world consumer {{ import {interface}; }}"
                    )
                    .as_bytes()
                    .into(),
                    dependencies: vec![dependency.clone()],
                },
                Some("consumer"),
            )
        };
        generate("test:producer/api@1.0.0")?;
        let error = generate("wasi:clocks/types@0.3.0").unwrap_err();
        assert_eq!(
            error.diagnostics[0].code,
            perry_wit::DiagnosticCode::WitResolution
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("types") && message.contains("wasi:clocks"),
            "{message}"
        );
    }
    Ok(())
}

#[test]
fn ambient_packages_keep_explicit_and_transitive_precedence() -> anyhow::Result<()> {
    if let Some(directory) = std::env::var_os("PERRY_AMBIENT_WIT_PROBE") {
        let (resolve, _) = resolve_wit(&PathBuf::from(directory).join("application"))?;
        let mut random_packages = resolve
            .packages
            .iter()
            .map(|(_, package)| package)
            .filter(|package| package.name.to_string() == "wasi:random@0.3.0");
        let random = random_packages.next().expect("ambient random package");
        assert!(random_packages.next().is_none(), "duplicate random package");
        assert!(
            random.interfaces.contains_key("ambient-marker"),
            "the encoded ambient package must take precedence over embedded sources"
        );
        for (explicit, expected) in [
            ("", wit_parser::Type::U32),
            (
                "package test:n@1.0.0 { interface api { type value = u64; } }",
                wit_parser::Type::U64,
            ),
        ] {
            for imports in [
                "import test:n/api@1.0.0; import test:b/api@1.0.0;",
                "import test:b/api@1.0.0; import test:n/api@1.0.0;",
                "import test:b/api@1.0.0;",
            ] {
                let source =
                    format!("package test:consumer; world consumer {{ {imports} }} {explicit}");
                let (resolve, _) = resolve_source("consumer.wit", &source)?;
                let package = resolve
                    .packages
                    .iter()
                    .find(|(_, package)| package.name.to_string() == "test:n@1.0.0")
                    .expect("selected transitive package")
                    .1;
                let api = &resolve.interfaces[package.interfaces["api"]];
                let value = &resolve.types[api.types["value"]].kind;
                assert!(
                    matches!(value, wit_parser::TypeDefKind::Type(found) if *found == expected),
                    "{imports} (explicit={explicit}): {value:?}"
                );
            }
        }
        return Ok(());
    }

    let directory = tempfile::tempdir()?;
    let ambient = directory.path().join("ambient");
    let application = directory.path().join("application");
    fs::create_dir(&ambient)?;
    fs::create_dir(&application)?;
    fs::write(
        application.join("world.wit"),
        "package test:encoded-ambient; world test {
            import wasi:random/random@0.3.0;
            import wasi:random/ambient-marker@0.3.0;
        }",
    )?;
    let mut sources = wit_parser::SourceMap::new();
    let standard = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("wit/deps/random");
    for entry in fs::read_dir(standard)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "wit") {
            sources.push_str(&path.to_string_lossy(), fs::read_to_string(&path)?);
        }
    }
    sources.push_str(
        "ambient-marker.wit",
        "package wasi:random@0.3.0; interface ambient-marker {}",
    );
    let package = sources
        .parse()
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
    let mut resolve = wit_parser::Resolve::new();
    let package = resolve.push_groups(package, Vec::new())?;
    fs::write(
        ambient.join("random.wasm"),
        wit_component::encode(&resolve, package, false)?,
    )?;

    fs::write(
        ambient.join("a.wit"),
        "package test:n@1.0.0; interface api { type value = u32; }",
    )?;
    fs::write(
        ambient.join("b.wit"),
        "package test:b@1.0.0;
        interface api { use test:n/api@1.0.0.{value}; read: func() -> value; }
        package test:n@1.0.0 { interface api { type value = string; } }
        package test:unrelated@1.0.0 { interface api { use missing:dependency/api.{value}; read: func() -> value; } }",
    )?;

    let result = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "wit_resolution_test::ambient_packages_keep_explicit_and_transitive_precedence",
            "--nocapture",
        ])
        .env("PERRY_AMBIENT_WIT_PROBE", directory.path())
        .env("WASI_WIT_PATH", ambient)
        .output()?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

/// Local source types win over SDK name matches, and same-named records of different WIT
/// interfaces resolve in their own modules.
#[test]
fn source_and_interface_types_resolve_in_their_own_modules() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("domain.ts"),
        "export interface Options { label: string }",
    )?;
    let main = directory
        .path()
        .join("main.ts")
        .to_string_lossy()
        .into_owned();
    for (wit, path, source) in [
        (
            "package test:local; world run {record options {count:u32} export run:func(value:options)->u32;}",
            main.as_str(),
            "import type { Options } from './domain.ts'; import type { Options as WitOptions } from './types/world'; function size(value:Options):number{return value.label.length;} export function run(value:WitOptions):number{return size({label:'abc'}) + value.count;}",
        ),
        (
            "package test:scoped; interface left {record options {count:u32}} interface right {record options {label:string}} world run {use left.{options as left-options}; use right.{options as right-options}; export run:func(left:left-options,right:right-options)->u32;}",
            "scoped.ts",
            "import type { Options as Left } from 'test:scoped/left'; import type { Options as Right } from 'test:scoped/right'; export function run(left:Left,right:Right):number {return left.count + right.label.length;}",
        ),
    ] {
        Standard.compile(wit, "run", path, source)?;
    }
    Ok(())
}
