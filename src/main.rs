use std::env;
use tracing::{debug, error, info, warn};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use docker_reaper::{
    reap_containers, reap_networks, reap_volumes, Docker, Filter, ReapContainersConfig,
    ReapNetworksConfig, ReapVolumesConfig,
};
use tokio::time::{sleep, Duration};

#[derive(Debug, Parser)]
#[command(
    about,
    version,
    after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Run repeatedly, waiting this long between removal attempts.
    #[arg(long, value_name = "duration", value_parser = parse_duration, global = true)]
    every: Option<Duration>,
    /// Log output without actually removing resources.
    #[arg(long, short = 'd', global = true)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Reap matching containers.
    Containers(ContainersArgs),
    /// Reap matching networks.
    Networks(NetworksArgs),
    /// Reap matching volumes.
    Volumes(VolumesArgs),
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

#[derive(Debug, Args)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct NetworksArgs {
    /// Only reap networks older than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    min_age: Option<Duration>,
    /// Only reap networks younger than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    max_age: Option<Duration>,
    #[arg(
        name = "filter",
        long,
        short = 'f',
        help = "Only reap networks matching a Docker Engine-supported filter (https://docs.docker.com/engine/reference/commandline/network_ls/#filter). Can be specified multiple times",
        value_name = "name=value",
        value_parser = parse_filter
    )]
    filters: Vec<Filter>,
}

#[derive(Debug, Args)]
#[command(after_help = "Note: <duration> values accept Go-style duration strings (e.g. 1m30s)")]
struct VolumesArgs {
    /// Only reap volumes older than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    min_age: Option<Duration>,
    /// Only reap volumes younger than this duration.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    max_age: Option<Duration>,
    #[arg(
        name = "filter",
        long,
        short = 'f',
        help = "Only reap volumes matching a Docker Engine-supported filter (https://docs.docker.com/engine/reference/commandline/volume_ls/#filter). Can be specified multiple times",
        value_name = "name=value",
        value_parser = parse_filter
    )]
    filters: Vec<Filter>,
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

    if let Some(duration) = global_args.every {
        info!("Reaping resources every {} seconds", duration.as_secs());
    } else {
        info!("Reaping resources once");
    }

    loop {
        info!("Starting new run");
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
            Commands::Networks(ref args) => {
                let config = ReapNetworksConfig {
                    dry_run: global_args.dry_run,
                    min_age: args.min_age,
                    max_age: args.max_age,
                    filters: &args.filters,
                };
                reap_networks(&docker, &config).await
            }
            Commands::Volumes(ref args) => {
                let config = ReapVolumesConfig {
                    dry_run: global_args.dry_run,
                    min_age: args.min_age,
                    max_age: args.max_age,
                    filters: &args.filters,
                };
                reap_volumes(&docker, &config).await
            }
        };
        match result {
            Ok(removed_resources) => {
                info!("Found {} matching resources", removed_resources.len());
                if !removed_resources.is_empty() {
                    use tabled::{
                        settings::{object::Columns, Style, Width},
                        Table,
                    };
                    let mut table = Table::new(removed_resources);
                    info!(
                        "\n{}",
                        table
                            .with(Style::sharp())
                            .modify(Columns::last(), Width::wrap(80))
                            .to_string()
                    );
                }
            }
            Err(e) => {
                error!("{}", e.to_string());
            }
        }
        if let Some(duration) = global_args.every {
            debug!("Sleeping for {:?}", global_args.every);
            sleep(duration).await;
        } else {
            break Ok(());
        }
    }
}
