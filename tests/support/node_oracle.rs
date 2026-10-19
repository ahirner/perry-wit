use anyhow::{Context, Result, ensure};
use std::{io::Write, process::Command};
use tempfile::TempPath;

pub(crate) enum NodeOracle<'a> {
    Inline(&'a str),
    TypeScript(TempPath),
}

impl NodeOracle<'_> {
    pub(crate) fn typescript(source: &str, invocation: &str) -> Result<Self> {
        let mut script = tempfile::Builder::new().suffix(".mts").tempfile()?;
        writeln!(script, "{source}\n{invocation}")?;
        Ok(Self::TypeScript(script.into_temp_path()))
    }

    pub(crate) fn run(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new("node");
        match self {
            Self::Inline(script) => command.args(["-e", script]),
            Self::TypeScript(path) => command.arg("--experimental-strip-types").arg(path),
        };
        let output = command.args(args).output().context("Node oracle")?;
        ensure!(
            output.status.success(),
            "Node oracle exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).context("Node oracle stdout is not UTF-8")
    }
}
