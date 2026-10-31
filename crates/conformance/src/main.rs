use anyhow::{Context, Result, bail};
use perry_conformance::{
    registry,
    runner::{self, RunOptions, Settings},
};
use std::{env, path::PathBuf};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let action = args.next().unwrap_or_else(|| "check".into());
    match action.as_str() {
        "replay" => {
            let path = PathBuf::from(args.next().context("replay requires a saved case")?);
            let mut output = None;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--output-dir" => {
                        output = Some(PathBuf::from(
                            args.next().context("--output-dir needs a value")?,
                        ))
                    }
                    _ => bail!("unknown option {flag}"),
                }
            }
            runner::replay_with_output(&path, output.as_deref())
        }
        "list" => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &registry::contracts(0)?
                        .iter()
                        .map(|c| c.catalog())
                        .collect::<Result<Vec<_>>>()?
                )?
            );
            Ok(())
        }
        "doc" => {
            print!("{}", registry::markdown()?);
            Ok(())
        }
        "check" => {
            let mut settings = Settings::default();
            let mut options = RunOptions::default();
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
                    "--shard" => options.shard = Some(value.parse()?),
                    "--output-dir" => options.output_dir = Some(value.into()),
                    _ => bail!("unknown option {flag}"),
                }
            }
            runner::campaign_with_options(settings, options)?;
            Ok(())
        }
        _ => bail!(
            "Usage: perry-conformance list | doc | check [--select ID] [--cases N] [--depth N] [--seed N] [--shrink-limit N] [--fuel N] [--memory BYTES] [--timeout SECONDS] [--shard INDEX/COUNT] [--output-dir PATH] | replay CASE.json [--output-dir PATH]"
        ),
    }
}
