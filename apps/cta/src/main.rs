//! `cta`: CTA's front end. Ours: only the three commands a player needs
//! (`run`, `shaders`, `warm`). The pieces behind them are cloned with credit
//! (see PIECES.md): `run.rs`, `warm.rs`, `translate*`, `shader*` and
//! `agility.rs` from Vice-Tay (TaynanGT, GPL-3.0).

#[cfg(agility_sdk)]
mod agility;
mod run;
#[allow(dead_code)] // cloned whole; cta uses only `shaders`
mod shader;
#[allow(dead_code)]
mod translate;
#[cfg(windows)]
mod warm;

use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "cta", version, about = "ClaudeTheftAuto: run an Xbox Series title on Windows")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through.
        #[arg(long)]
        strict: bool,
        /// Actually start the title.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees.
        #[arg(long, value_enum, default_value_t = Console::SeriesS)]
        console: Console,
        /// Arguments for the title, after `--`.
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Translate every shader found into the shader cache.
    Shaders {
        paths: Vec<PathBuf>,
        #[arg(long, conflicts_with = "list")]
        if_needed: bool,
        #[arg(long, value_parser = parse_jobs)]
        jobs: Option<usize>,
        #[arg(long)]
        list: Option<PathBuf>,
    },
    /// Make the host pipelines earlier runs wrote down, before a run needs them.
    #[cfg(windows)]
    Warm {
        #[arg(long, value_parser = parse_jobs)]
        jobs: Option<usize>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=256))]
        batch_size: Option<u16>,
        #[arg(long)]
        normal_priority: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        again: bool,
        #[arg(long)]
        used: bool,
        #[arg(long, hide = true)]
        these: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn parse_jobs(value: &str) -> Result<usize, &'static str> {
    value.parse::<NonZeroUsize>().map(NonZeroUsize::get).map_err(|_| "expected a worker count of 1 or more")
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    find_bcpack_tables();
    let cli = Cli::parse();
    #[cfg(agility_sdk)]
    agility::check_runtime()?;
    match cli.command {
        Command::Run { exe, unimplemented, strict, execute, console, title_args } => {
            victory_hle::runtime::set_title_args(title_args);
            run::run(&exe, unimplemented, strict, execute, console)
        }
        Command::Shaders { paths, jobs, list, if_needed } => translate::precompile(paths, jobs, list, if_needed),
        #[cfg(windows)]
        Command::Warm { jobs, batch_size, normal_priority, limit, again, used, these } => {
            warm::run(jobs, batch_size, normal_priority, limit, again, used, these)
        }
    }
}

/// Without the BCPack format tables every compressed texture the title
/// streams fails and the world draws black. When `VICTORY_BCPACK_TABLES` is
/// unset, look for `bcpack-tables.bin` beside `cta.exe`, then in
/// `%LOCALAPPDATA%\6ixpack`, and say plainly when there is none.
fn find_bcpack_tables() {
    const VAR: &str = "VICTORY_BCPACK_TABLES";
    if std::env::var_os(VAR).is_some() {
        return;
    }
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("bcpack-tables.bin")));
    let local = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("6ixpack").join("bcpack-tables.bin"));
    match beside.into_iter().chain(local).find(|p| p.is_file()) {
        Some(found) => {
            tracing::info!(path = %found.display(), "BCPack format tables found");
            // SAFETY: called first thing in main, before any other thread starts.
            unsafe { std::env::set_var(VAR, found) };
        }
        None => tracing::warn!("no BCPack format tables: compressed textures will fail and the world will draw black; set {VAR}"),
    }
}

/// Paths given on the command line, or the configured game directory.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(!all.is_empty(), "no paths given and none configured");
    Ok(all)
}
