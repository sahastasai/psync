//! `cargo psync`: the front end of psync's compiler-level checks.
//!
//! ```text
//! cargo psync check [--deny] [--notes] [CARGO CHECK ARGS...]
//! cargo psync summaries [CRATE] [--all]
//! ```
//!
//! `check` runs `cargo check` with `psync-driver` as `RUSTC_WRAPPER`, in its own target directory
//! (`target/psync`), so the normal build is untouched. Every crate in the build gets effect
//! summaries; crates with `#[psync::graph]` rewrites are checked against them.
//! `summaries` prints the summaries the last `check` computed.

use psync_check::{CrateFindings, CrateSummaries, Severity, env};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
cargo psync: check #[psync::graph] rewrites against the compiled bodies of callees

USAGE:
    cargo psync check [--deny] [--notes] [CARGO CHECK ARGS...]
    cargo psync summaries [CRATE] [--all]

check       run `cargo check` with psync's driver; warn where a rewrite runs conflicting
            effects at once, writes before an earlier exit, or prefetches loop writes
  --deny    make findings errors (exit status 1)
  --notes   also report output order and shared interior-mutable state
summaries   print each function's effects as computed by the last `check`
  CRATE     only this crate (default: every crate with findings or rewrites)
  --all     include functions without effects but with opaque calls
";

fn main() -> ExitCode {
    // `cargo psync ...` runs `cargo-psync psync ...`.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "psync") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        Some("check") => check(&args[1..]),
        Some("summaries") => summaries(&args[1..]),
        Some("-h" | "--help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("cargo psync: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `target/psync` under the project's target directory.
fn psync_dir() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .or_else(target_dir_from_metadata)
        .unwrap_or_else(|| PathBuf::from("target"));
    target.join("psync")
}

fn target_dir_from_metadata() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    value["target_directory"].as_str().map(PathBuf::from)
}

fn check(args: &[String]) -> ExitCode {
    let deny = args.iter().any(|a| a == "--deny");
    let notes = args.iter().any(|a| a == "--notes");
    let passthrough: Vec<&String> = args
        .iter()
        .filter(|a| *a != "--deny" && *a != "--notes")
        .collect();
    let driver = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("psync-driver"))
        .filter(|p| p.exists());
    let Some(driver) = driver else {
        eprintln!("cargo psync: `psync-driver` must be installed next to `cargo-psync`");
        return ExitCode::from(2);
    };
    let dir = psync_dir();
    let findings_dir = dir.join("findings");
    let _ = std::fs::remove_dir_all(&findings_dir);
    let sysroot = env!("PSYNC_SYSROOT");
    let lib = Path::new(sysroot).join("lib");
    // The driver links the toolchain's `librustc_driver`. On NixOS, the libraries that library
    // needs (zlib) come from nix-ld's path, which only binaries using nix-ld's loader see.
    let mut paths = vec![lib.clone()];
    for var in ["LD_LIBRARY_PATH", "NIX_LD_LIBRARY_PATH"] {
        if let Some(old) = std::env::var_os(var) {
            paths.extend(std::env::split_paths(&old));
        }
    }
    let library_path = std::env::join_paths(paths).expect("library path");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .arg("check")
        .args(passthrough)
        .env("RUSTC_WRAPPER", &driver)
        .env("CARGO_TARGET_DIR", &dir)
        .env("LD_LIBRARY_PATH", &library_path)
        .env("DYLD_FALLBACK_LIBRARY_PATH", &library_path)
        .env(env::SUMMARIES, dir.join("summaries"))
        .env(env::FINDINGS, &findings_dir)
        .env(env::DENY, if deny { "1" } else { "0" })
        .env(env::NOTES, if notes { "1" } else { "0" })
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cargo psync: could not run cargo: {e}");
            return ExitCode::from(2);
        }
    };
    let crates: Vec<CrateFindings> = psync_check::read_all(&findings_dir);
    let (levels, loops): (usize, usize) = crates.iter().fold((0, 0), |(l, p), c| {
        (l + c.checked_levels, p + c.checked_loops)
    });
    let warnings: usize = crates
        .iter()
        .flat_map(|c| &c.findings)
        .filter(|f| f.severity == Severity::Warning)
        .count();
    let notes_found: usize = crates
        .iter()
        .flat_map(|c| &c.findings)
        .filter(|f| f.severity == Severity::Note)
        .count();
    eprintln!(
        "psync check: {levels} concurrent level(s) and {loops} prefetched loop(s) checked in {} \
         crate(s): {warnings} warning(s), {notes_found} note(s){}",
        crates.len(),
        if crates.is_empty() {
            " (unchanged crates are not re-checked; touch a file or `cargo clean -p` to re-run)"
        } else {
            ""
        }
    );
    if !status.success() || (deny && warnings > 0) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn summaries(args: &[String]) -> ExitCode {
    let all = args.iter().any(|a| a == "--all");
    let wanted = args.iter().find(|a| !a.starts_with("--"));
    let dir = psync_dir();
    let mut crates: Vec<CrateSummaries> = psync_check::read_all(&dir.join("summaries"));
    if crates.is_empty() {
        eprintln!(
            "cargo psync: no summaries in {}; run `cargo psync check` first",
            dir.display()
        );
        return ExitCode::from(1);
    }
    let checked: Vec<String> = psync_check::read_all::<CrateFindings>(&dir.join("findings"))
        .into_iter()
        .map(|c| c.crate_name)
        .collect();
    crates.sort_by(|a, b| a.crate_name.cmp(&b.crate_name));
    for krate in crates {
        let show = match wanted {
            Some(w) => krate.crate_name == *w || krate.crate_name == w.replace('-', "_"),
            None => checked.contains(&krate.crate_name),
        };
        if !show {
            continue;
        }
        println!("crate {}", krate.crate_name);
        let mut functions: Vec<_> = krate.functions.into_values().collect();
        functions.sort_by(|a, b| a.path.cmp(&b.path));
        for f in functions {
            if f.effects.is_empty() && !all {
                continue;
            }
            println!("  {}", f.path);
            for e in &f.effects {
                println!("    {:<40} via {}", e.describe(), e.path());
            }
            if all {
                for o in &f.opaque {
                    println!("    opaque: {o}");
                }
            }
        }
    }
    ExitCode::SUCCESS
}
