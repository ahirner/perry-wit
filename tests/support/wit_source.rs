use anyhow::Result;

pub(crate) fn check_sdk_source(wit: &str, source: &str) -> Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("component.ts"), source)?;
    let wit_dir = project.path().join("wit");
    std::fs::create_dir(&wit_dir)?;
    std::fs::write(wit_dir.join("world.wit"), wit)?;
    let sdk = perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir,
        world: Some("boundary".into()),
        out_dir: project.path().join("types"),
        project_root: Some(project.path().into()),
        initialize_tsconfig: true,
        entry: "component.ts".into(),
    })?;
    let output = std::process::Command::new("tsc")
        .args([
            "--ignoreConfig",
            "--noEmit",
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "esnext",
        ])
        .arg(sdk.check_path)
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
