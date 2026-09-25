//! Command-line parsing.

use crate::{Cli, Commands};
use clap::Parser;
use std::path::PathBuf;

fn parse(args: &[&str]) -> Commands {
    Cli::try_parse_from(args)
        .unwrap_or_else(|e| panic!("{args:?} did not parse: {e}"))
        .command
}

/// Deployments append these flags to commands an operator may already have given them in.
/// A repeat has to replace the earlier value: a parse error would fail the unit, and every
/// sweep after it in the same unit.
#[test]
fn a_repeated_path_flag_keeps_the_last_value() {
    let Commands::Containers(args) = parse(&[
        "docker-reaper",
        "containers",
        "--record-image-use",
        "/a",
        "--record-image-use",
        "/b",
    ]) else {
        panic!("not the containers subcommand");
    };
    assert_eq!(args.record_image_use, Some(PathBuf::from("/b")));

    let Commands::Images(args) = parse(&["docker-reaper", "images", "--lru", "/a", "--lru", "/b"])
    else {
        panic!("not the images subcommand");
    };
    assert_eq!(args.lru, Some(PathBuf::from("/b")));

    let Commands::Shims(args) = parse(&[
        "docker-reaper",
        "shims",
        "--data-root",
        "/a",
        "--data-root",
        "/b",
    ]) else {
        panic!("not the shims subcommand");
    };
    assert_eq!(args.data_root, Some(PathBuf::from("/b")));
}

/// Filters are the one flag meant to repeat, and each occurrence must still add one.
#[test]
fn filters_still_accumulate() {
    let Commands::Images(args) = parse(&[
        "docker-reaper",
        "images",
        "-f",
        "reference=a*",
        "-f",
        "reference=b*",
    ]) else {
        panic!("not the images subcommand");
    };
    assert_eq!(args.filters.len(), 2);
}
