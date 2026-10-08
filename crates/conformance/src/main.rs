use anyhow::{Context, Result, bail};
use perry_conformance::{
    execution, registry,
    runner::{self, Settings},
};
use std::{env, path::PathBuf};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let action = args.next().unwrap_or_else(|| "check".into());
    match action.as_str() {
        "worker" => {
            let request = PathBuf::from(args.next().context("worker request")?);
            let destination = PathBuf::from(args.next().context("worker destination")?);
            execution::worker(&request, &destination)
        }
        "replay" => runner::replay(
            &PathBuf::from(args.next().context("replay requires a saved case")?),
            env::current_exe()?,
        ),
        "list" => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &registry::contracts(0)?
                        .iter()
                        .map(|c| c.catalog())
                        .collect::<Vec<_>>()
                )?
            );
            Ok(())
        }
        "check" => {
            let mut settings = Settings::default();
            while let Some(flag) = args.next() {
                let value = args
                    .next()
                    .with_context(|| format!("{flag} needs a value"))?;
                match flag.as_str() {
                    "--select" => settings.select = value,
                    "--cases" => settings.cases = value.parse()?,
                    "--depth" => settings.depth = value.parse()?,
                    "--seed" => settings.seed = value.parse()?,
                    "--shrink-limit" => settings.shrink_limit = value.parse()?,
                    "--fuel" => settings.limits.fuel = value.parse()?,
                    "--memory" => settings.limits.memory_bytes = value.parse()?,
                    "--timeout" => settings.limits.timeout_seconds = value.parse()?,
                    _ => bail!("unknown option {flag}"),
                }
            }
            runner::campaign(settings, env::current_exe()?)?;
            Ok(())
        }
        _ => bail!(
            "Usage: perry-conformance list | check [--select ID] [--cases N] [--depth N] [--seed N] [--shrink-limit N] [--fuel N] [--memory BYTES] [--timeout SECONDS] | replay CASE.json"
        ),
    }
}
