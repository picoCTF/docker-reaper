use std::time::Duration;

use clap::Parser;
use docker_reaper::{reap_containers, ReapContainersConfig};
use tokio::time::sleep;

#[derive(Debug, Parser)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct Args {
    /// Interval to wait after reaping containers.
    #[arg(long, value_name = "duration", default_value_t = String::from("60s"))]
    every: String,
    /// Only reap containers once. Conflicts with "--every".
    #[arg(long, conflicts_with = "every")]
    once: bool,
    /// Print output without actually removing containers or networks.
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
        value_name = "name=value"
    )]
    filters: Vec<String>,
    /// Also attempt to remove the networks associated with reaped containers.
    #[arg(long)]
    reap_networks: bool,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();
    let config = ReapContainersConfig {};
    if args.once {
        reap_containers(&config).await;
    } else {
        let sleep_ns = match go_parse_duration::parse_duration(&args.every) {
            Ok(ns) => ns,
            Err(_) => anyhow::bail!("failed to parse \"since\" value"),
        };
        if sleep_ns < 1 {
            anyhow::bail!("\"since\" must be a positive duration")
        }
        let sleep_ns: u64 = sleep_ns.try_into()?;
        let sleep_duration = Duration::from_nanos(sleep_ns);
        loop {
            reap_containers(&config).await;
            sleep(sleep_duration).await;
        }
    }
    Ok(())
}
