use std::{collections::HashMap, time::Duration};

use anyhow::Context;
use clap::Parser;
use docker_reaper::{reap_containers, ReapContainersConfig};
use tokio::time::sleep;

#[derive(Debug, Parser)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct Args {
    /// Interval to wait after reaping containers.
    #[arg(long, value_name = "duration", value_parser = parse_duration, default_value = "60s")]
    every: Duration,
    /// Only reap containers once. Conflicts with "--every".
    #[arg(long, conflicts_with = "every")]
    once: bool,
    /// Log output without actually removing containers or networks.
    #[arg(long, short = 'd')]
    dry_run: bool,
    /// Only containers older than this duration will be eligible for reaping.
    #[arg(long, value_name = "duration")]
    min_age: Option<String>,
    /// Only containers younger than this duration will be eligible for reaping.
    #[arg(long, value_name = "duration")]
    max_age: Option<String>,
    /// Additional Docker Engine-supported [container filters](https://docs.docker.com/engine/reference/commandline/ps/#filter). Can be specified multiple times.
    #[arg(
        name = "filter",
        long,
        short = 'f',
        // todo: https://github.com/clap-rs/clap/issues/2389
        help = "Additional Docker-engine supported container filters (https://docs.docker.com/engine/reference/commandline/ps/#filter). Can be specified multiple times",
        value_name = "name=value",
        value_parser = parse_filter
    )]
    filters: Vec<Filter>,
    /// Also attempt to remove the networks associated with reaped containers.
    #[arg(long)]
    reap_networks: bool,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();
    let config = ReapContainersConfig {
        dry_run: args.dry_run,
        min_age: args.min_age,
        max_age: args.max_age,
        filters: {
            args.filters.iter().fold(HashMap::new(), |mut acc, f| {
                acc.entry(f.name.clone()).or_default().push(f.value.clone());
                acc
            })
        },
        reap_networks: args.reap_networks,
    };
    if args.once {
        reap_containers(&config).await;
    } else {
        loop {
            reap_containers(&config).await;
            sleep(args.every).await;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct Filter {
    name: String,
    value: String,
}

impl Filter {
    pub(crate) fn new(name: &str, value: &str) -> Self {
        Self {
            name: String::from(name),
            value: String::from(value),
        }
    }
}

fn parse_filter(value: &str) -> Result<Filter, anyhow::Error> {
    let err_msg = "filters must be in NAME=VALUE(=VALUE) format";
    let (name, value) = value.split_once('=').context(err_msg)?;
    if name.is_empty() || value.is_empty() {
        return Err(anyhow::anyhow!(err_msg));
    }
    Ok(Filter::new(name, value))
}

fn parse_duration(value: &str) -> Result<Duration, anyhow::Error> {
    let sleep_ns = match go_parse_duration::parse_duration(value) {
        Ok(ns) if ns < 1 => {
            anyhow::bail!("must be a positive duration: {}", value);
        }
        Ok(ns) => ns,
        Err(_) => anyhow::bail!("failed to parse duration: {}", value),
    };
    let sleep_ns: u64 = sleep_ns.try_into()?;
    Ok(Duration::from_nanos(sleep_ns))
}
