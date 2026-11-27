//! Repository maintenance. Run with `cargo xtask <command>`.

mod crumbs;
mod policy;
#[cfg(test)]
mod switches;
mod wine;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Linux host: cross-build for Windows and run the tests under Wine
    /// (clang-cl, lld-link and llvm-lib on PATH, an `xwin` splat).
    WineTest {
        /// The `xwin splat` output folder (default: ~/.xwin).
        #[arg(long)]
        xwin: Option<PathBuf>,
        /// Folder with vkd3d-proton's x64 d3d12.dll and d3d12core.dll, used
        /// instead of Wine's own D3D12 (which lacks shader model 6.6).
        #[arg(long)]
        vkd3d_proton: Option<PathBuf>,
        /// Arguments for `cargo test` (default: the workspace but xtask).
        #[arg(last = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Read a breadcrumb file (a diagnostics build run with
    /// `VICTORY_CRUMBS=1` writes `victory-crumbs.bin`): the submissions the
    /// card had not finished when the records end, and every record that
    /// names bytes outside their buffer.
    Crumbs {
        /// The file.
        file: PathBuf,
        /// Every record of the lists not finished, not only their counts.
        #[arg(long)]
        records: bool,
        /// The last records of the file, whatever they are of.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
        Cmd::WineTest {
            xwin,
            vkd3d_proton,
            args,
        } => wine::run(
            &root,
            wine::Options {
                xwin,
                vkd3d_proton,
                args,
            },
        ),
        Cmd::Crumbs { file, records, tail } => {
            let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            print!("{}", crumbs::read(&bytes, records, tail.unwrap_or(0))?);
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
