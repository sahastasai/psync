//! `cargo psync`: the front end of psync's compiler-level checks.
//!
//! ```text
//! cargo psync check [--deny] [--notes] [CARGO CHECK ARGS...]
//! cargo psync build [--deny] [--notes] [CARGO BUILD ARGS...]
//! cargo psync summaries [CRATE] [--all]
//! ```
//!
//! `check` runs `cargo check` with `psync-driver` as `RUSTC_WRAPPER`, in its own target directory
//! (`target/psync`), so the normal build is untouched. Every crate in the build gets effect
//! summaries; crates with `#[psync::graph]` rewrites are checked against them.
//! After every check, a link step resolves `dyn` and generic trait calls over the whole build and
//! writes the facts the macro can schedule with; `build` does that, checks the schedule the macro
//! builds from the facts, and runs `cargo build` with them.
//! `summaries` prints the summaries the last `check` computed.

use psync_check::{CrateFindings, CrateSummaries, Severity, env};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
cargo psync: check #[psync::graph] rewrites against the compiled bodies of callees

USAGE:
    cargo psync check [--deny] [--notes] [--facts] [CARGO CHECK ARGS...]
    cargo psync build [--deny] [--notes] [CARGO BUILD ARGS...]
    cargo psync fix [CARGO CHECK ARGS...]
    cargo psync summaries [CRATE] [--all]

check       run `cargo check` with psync's driver; warn where a rewrite runs conflicting
            effects at once, writes before an earlier exit, or prefetches loop writes
  --deny    make findings errors (exit status 1)
  --notes   also report output order and shared interior-mutable state
  --facts   then also check the schedule the macro builds from the compiler's facts
build       compute the compiler's facts about every call in #[psync::graph] functions, let the
            macro schedule with them (instead of judging calls by their names), check that
            schedule, then `cargo build` with it
fix         check, then insert `#[psync::seq]` before every statement or loop with a warning
            (review the result: `#[psync::speculate]` may be the better hint for idempotent writes)
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
        Some("build") => build(&args[1..]),
        Some("fix") => fix(&args[1..]),
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

/// Options shared by `check` and `build`.
struct Opts<'a> {
    deny: bool,
    notes: bool,
    /// `check --facts`: also check the schedule the macro builds from the compiler's facts.
    facts: bool,
    passthrough: Vec<&'a String>,
}

fn opts(args: &[String]) -> Opts<'_> {
    Opts {
        deny: args.iter().any(|a| a == "--deny"),
        notes: args.iter().any(|a| a == "--notes"),
        facts: args.iter().any(|a| a == "--facts"),
        passthrough: args
            .iter()
            .filter(|a| !matches!(a.as_str(), "--deny" | "--notes" | "--facts"))
            .collect(),
    }
}

/// A stable short hash of a file's contents (empty file: `none`).
fn stamp(path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let Ok(bytes) = std::fs::read(path) else {
        return "none".into();
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// One `cargo check` with the driver. `facts` makes the macro schedule with the compiler's facts.
fn pass(opts: &Opts<'_>, dir: &Path, facts: bool) -> Result<bool, ExitCode> {
    let driver = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("psync-driver"))
        .filter(|p| p.exists());
    let Some(driver) = driver else {
        eprintln!("cargo psync: `psync-driver` must be installed next to `cargo-psync`");
        return Err(ExitCode::from(2));
    };
    let findings_dir = dir.join("findings");
    let _ = std::fs::remove_dir_all(&findings_dir);
    let lib = Path::new(env!("PSYNC_SYSROOT")).join("lib");
    // The driver links the toolchain's `librustc_driver`. On NixOS, the libraries that library
    // needs (zlib) come from nix-ld's path, which only binaries using nix-ld's loader see.
    let mut paths = vec![lib];
    for var in ["LD_LIBRARY_PATH", "NIX_LD_LIBRARY_PATH"] {
        if let Some(old) = std::env::var_os(var) {
            paths.extend(std::env::split_paths(&old));
        }
    }
    let library_path = std::env::join_paths(paths).expect("library path");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let linked = dir.join("linked.json");
    let mut command = Command::new(cargo);
    command
        .arg("check")
        .args(&opts.passthrough)
        .env("RUSTC_WRAPPER", &driver)
        .env("CARGO_TARGET_DIR", dir)
        .env("LD_LIBRARY_PATH", &library_path)
        .env("DYLD_FALLBACK_LIBRARY_PATH", &library_path)
        .env(env::SUMMARIES, dir.join("summaries"))
        .env(env::FINDINGS, &findings_dir)
        .env(env::LINKED, &linked)
        .env("PSYNC_LINKED_STAMP", stamp(&linked))
        .env(env::DENY, if opts.deny { "1" } else { "0" })
        .env(env::NOTES, if opts.notes { "1" } else { "0" });
    if facts {
        let file = dir.join("facts.tsv");
        command
            .env(env::FACTS, &file)
            .env(env::FACTS_STAMP, stamp(&file));
    } else {
        command.env_remove(env::FACTS).env(env::FACTS_STAMP, "none");
    }
    match command.status() {
        Ok(s) => Ok(s.success()),
        Err(e) => {
            eprintln!("cargo psync: could not run cargo: {e}");
            Err(ExitCode::from(2))
        }
    }
}

/// Link every crate's summaries: resolve `dyn` and generic trait calls over the whole build and
/// write `linked.json` (for the driver) and `facts.tsv` (for the macro). Returns whether the
/// resolution changed since the last run.
fn link(dir: &Path) -> bool {
    let crates: Vec<CrateSummaries> = psync_check::read_all(&dir.join("summaries"));
    let linked = psync_check::link(&crates);
    let linked_file = dir.join("linked.json");
    let before = stamp(&linked_file);
    let _ = std::fs::write(
        &linked_file,
        serde_json::to_string(&linked).unwrap_or_default(),
    );
    let _ = std::fs::write(
        dir.join("facts.tsv"),
        psync_check::facts_text(&crates, &linked),
    );
    stamp(&linked_file) != before
}

/// Print the findings of the last pass; returns the number of warnings.
fn report(dir: &Path, label: &str) -> usize {
    let crates: Vec<CrateFindings> = psync_check::read_all(&dir.join("findings"));
    let (levels, loops): (usize, usize) = crates.iter().fold((0, 0), |(l, p), c| {
        (l + c.checked_levels, p + c.checked_loops)
    });
    let count = |sev| {
        crates
            .iter()
            .flat_map(|c| &c.findings)
            .filter(|f| f.severity == sev)
            .count()
    };
    let (warnings, notes) = (count(Severity::Warning), count(Severity::Note));
    eprintln!(
        "psync {label}: {levels} concurrent level(s) and {loops} prefetched loop(s) checked in {} \
         crate(s): {warnings} warning(s), {notes} note(s){}",
        crates.len(),
        if crates.is_empty() {
            " (unchanged crates are not re-checked; touch a file or `cargo clean -p` to re-run)"
        } else {
            ""
        }
    );
    warnings
}

/// Check, link, and check again if linking resolved new `dyn` calls (the driver recompiles the
/// workspace's crates when `linked.json` changes).
fn check_and_link(opts: &Opts<'_>, dir: &Path) -> Result<bool, ExitCode> {
    let mut ok = pass(opts, dir, false)?;
    if link(dir) && ok {
        ok = pass(opts, dir, false)?;
        link(dir);
    }
    Ok(ok)
}

fn check(args: &[String]) -> ExitCode {
    let opts = opts(args);
    let dir = psync_dir();
    let mut ok = match check_and_link(&opts, &dir) {
        Ok(ok) => ok,
        Err(code) => return code,
    };
    let mut warnings = report(&dir, "check");
    if opts.facts && ok {
        ok = match pass(&opts, &dir, true) {
            Ok(ok) => ok,
            Err(code) => return code,
        };
        warnings = report(&dir, "check (with compiler facts)");
    }
    if !ok || (opts.deny && warnings > 0) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// `cargo psync build`: compute the compiler's facts, check the schedule the macro builds from
/// them, then run the real `cargo build` with them.
fn build(args: &[String]) -> ExitCode {
    let opts = opts(args);
    let dir = psync_dir();
    match check_and_link(&opts, &dir) {
        Ok(true) => {}
        Ok(false) => return ExitCode::from(1),
        Err(code) => return code,
    }
    // Schedule with the facts, and check that schedule.
    match pass(&opts, &dir, true) {
        Ok(true) => {}
        Ok(false) => return ExitCode::from(1),
        Err(code) => return code,
    }
    let warnings = report(&dir, "build (with compiler facts)");
    if opts.deny && warnings > 0 {
        return ExitCode::from(1);
    }
    let facts = dir.join("facts.tsv");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .arg("build")
        .args(&opts.passthrough)
        .env(env::FACTS, &facts)
        .env(env::FACTS_STAMP, stamp(&facts))
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

/// `cargo psync fix`: check, then apply every warning's fix.
fn fix(args: &[String]) -> ExitCode {
    let opts = opts(args);
    let dir = psync_dir();
    if let Err(code) = check_and_link(&opts, &dir) {
        return code;
    }
    report(&dir, "check");
    let crates: Vec<CrateFindings> = psync_check::read_all(&dir.join("findings"));
    let mut fixes: Vec<psync_check::Fix> = crates
        .iter()
        .flat_map(|c| &c.findings)
        .filter_map(|f| f.fix.clone())
        .collect();
    fixes.sort_by(|a, b| (&a.file, b.offset).cmp(&(&b.file, a.offset)));
    fixes.dedup();
    let mut applied = 0;
    let mut files: std::collections::BTreeMap<String, String> = Default::default();
    for f in &fixes {
        let text = files
            .entry(f.file.clone())
            .or_insert_with(|| std::fs::read_to_string(&f.file).unwrap_or_default());
        // Skip statements already marked.
        let before = &text[..f.offset.min(text.len())];
        if before.trim_end().ends_with("#[psync::seq]") || f.offset > text.len() {
            continue;
        }
        text.insert_str(f.offset, &f.insert);
        applied += 1;
    }
    for (file, text) in &files {
        if let Err(e) = std::fs::write(file, text) {
            eprintln!("cargo psync: could not write {file}: {e}");
            return ExitCode::from(1);
        }
    }
    eprintln!(
        "psync fix: inserted {applied} `#[psync::seq]` hint(s) in {} file(s); run `cargo psync \
         check` again to confirm",
        files.len()
    );
    ExitCode::SUCCESS
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
