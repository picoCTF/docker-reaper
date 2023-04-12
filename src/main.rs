use bollard::Docker;
use std::env;
use tabled::Table;
use tracing::{debug, error, info, warn};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use docker_reaper::{reap_containers, Filter, ReapContainersConfig};
use tokio::time::{sleep, Duration};

#[derive(Debug, Parser)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Interval to wait after reaping resources.
    #[arg(long, value_name = "duration", value_parser = parse_duration, default_value = "60s", global = true)]
    every: Duration,
    /// Only reap resources once. Conflicts with "--every".
    #[arg(long, conflicts_with = "every", global = true)]
    once: bool,
    /// Log output without actually removing resources.
    #[arg(long, short = 'd', global = true)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Reaps matching expired containers.
    Containers(ContainersArgs),
}

#[derive(Debug, Args)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct ContainersArgs {
    /// Only reap containers older than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    min_age: Option<Duration>,
    /// Only reap containers younger than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    max_age: Option<Duration>,
    #[arg(
        name = "filter",
        long,
        short = 'f',
        // TODO: https://github.com/clap-rs/clap/issues/2389
        help = "Only reap containers matching a Docker Engine-supported filter (https://docs.docker.com/engine/reference/commandline/ps/#filter). Can be specified multiple times",
        value_name = "name=value",
        value_parser = parse_filter
    )]
    filters: Vec<Filter>,
    /// Also attempt to remove the networks associated with reaped containers.
    #[arg(long)]
    reap_networks: bool,
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

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt::init();

    let global_args = Cli::parse();
    let docker = {
        if env::var("DOCKER_CERT_PATH").is_ok() {
            debug!("Environment variable DOCKER_CERT_PATH set. Connecting via TLS");
            Docker::connect_with_ssl_defaults()?
        } else if env::var("DOCKER_HOST").is_ok() {
            debug!("Environment variable DOCKER_HOST set, but not DOCKER_CERT_PATH. Connecting via HTTP");
            Docker::connect_with_http_defaults()?
        } else {
            debug!("Environment variable DOCKER_HOST not set, connecting to local machine");
            Docker::connect_with_local_defaults()?
        }
    };

    if global_args.once {
        info!("Reaping resources once");
    } else {
        info!(
            "Reaping resources every {} seconds",
            global_args.every.as_secs()
        );
    }

    loop {
        info!("Starting new run ({})", chrono::Utc::now().to_rfc3339());
        if global_args.dry_run {
            warn!("Dry run: no resources will be removed");
        }
        let result = match global_args.command {
            Commands::Containers(ref args) => {
                let config = ReapContainersConfig {
                    dry_run: global_args.dry_run,
                    min_age: args.min_age,
                    max_age: args.max_age,
                    filters: &args.filters,
                    reap_networks: args.reap_networks,
                };
                reap_containers(&docker, &config).await
            }
        };
        match result {
            Ok(removed_resources) => {
                info!("Found {} matching resources", removed_resources.len());
                if !removed_resources.is_empty() {
                    let mut table = Table::new(removed_resources);
                    info!(
                        "\n{}",
                        table
                            .with(tabled::Style::sharp())
                            .with(
                                tabled::Modify::new(tabled::object::Columns::last())
                                    .with(tabled::Width::wrap(80))
                            )
                            .to_string()
                    );
                }
            }
            Err(e) => {
                error!("{}", e.to_string());
            }
        }
        if global_args.once {
            break Ok(());
        } else {
            debug!("Sleeping for {:?}", global_args.every);
            sleep(global_args.every).await;
        }
    }
}
