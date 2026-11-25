//! Runs `cargo psync check` on `tests/fixtures/hidden` and checks what it finds.
use psync_check::{CrateFindings, Severity};
use std::path::Path;
use std::process::Command;

#[test]
fn finds_effects_hidden_in_callees() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hidden");
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fixture-hidden");
    // Re-check from scratch: cargo does not re-run the driver on unchanged crates.
    let _ = std::fs::remove_dir_all(target.join("psync"));
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-psync"))
        .args(["psync", "check", "--notes"])
        .current_dir(&fixture)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("run cargo psync");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let crates: Vec<CrateFindings> = psync_check::read_all(&target.join("psync").join("findings"));
    let app = crates
        .iter()
        .find(|c| c.crate_name == "app")
        .expect("app was checked");
    let at = |kind: &str, line: &str| {
        app.findings
            .iter()
            .any(|f| f.kind == kind && f.location.contains(line))
    };
    // `warm` writes the file `load` reads, behind a name with no verb.
    assert!(at("conflict", "lib.rs:24"), "{:#?}", app.findings);
    // `get_token` writes although its name reads, after a `?`.
    assert!(at("speculative-write", "lib.rs:43"), "{:#?}", app.findings);
    // A dependency's `stamp` writes; its summary came from compiling `helpers`.
    assert!(
        app.findings
            .iter()
            .any(|f| f.kind == "loop-speculative-write" && f.message.contains("helpers::stamp")),
        "{:#?}",
        app.findings
    );
    // Two chains share a `RefCell`.
    assert!(
        app.findings
            .iter()
            .any(|f| f.kind == "interior-mutability" && f.severity == Severity::Note)
    );
    // A `dyn Store` whose implementation writes the file the next call reads: resolved by the
    // link step over every implementation in the build, then checked in a second pass.
    assert!(at("conflict", "lib.rs:111"), "{:#?}", app.findings);
    // `visit` writes `walk.log` through the mutual recursion with `walk`, although `walk` was
    // summarized first (for `walk_then_read`): summaries are fixed points over call cycles.
    assert!(
        app.findings.iter().any(|f| f.kind == "conflict"
            && f.function == "tour"
            && f.message.contains("visit -> walk -> std::fs::write")),
        "{:#?}",
        app.findings
    );
    // `fs::copy` writes its second path, which the next statement reads.
    assert!(
        app.findings.iter().any(|f| f.kind == "conflict"
            && f.function == "release"
            && f.message.contains("write file \"site.html\"")),
        "{:#?}",
        app.findings
    );
    // A callee that may panic is an exit: the write after it is speculative.
    assert!(
        app.findings.iter().any(|f| f.kind == "speculative-write"
            && f.function == "boot"
            && f.message.contains("may panic")),
        "{:#?}",
        app.findings
    );
    // `sizes` reads two files, `walk_then_read` reads another file after a write: nothing to
    // report.
    assert!(!app.findings.iter().any(|f| f.function == "sizes"));
    assert!(!app.findings.iter().any(|f| f.function == "walk_then_read"));
    // `stash` writes the file it is given: the same file twice conflicts, two files do not.
    assert!(
        app.findings
            .iter()
            .any(|f| f.kind == "conflict" && f.function == "stash_same"),
        "{:#?}",
        app.findings
    );
    assert!(!app.findings.iter().any(|f| f.function == "stash_both"));
    assert_eq!(app.findings.len(), 9, "{:#?}", app.findings);
    assert!(stderr.contains("concurrent level(s)"));

    // A second run finds nothing to recompile. Its findings must still be the same, and
    // `--deny` must still fail: they persist per crate across runs.
    let summary = |stderr: &str| {
        stderr
            .lines()
            .rev()
            .find(|l| l.starts_with("psync check:"))
            .map(str::to_string)
    };
    let again = Command::new(env!("CARGO_BIN_EXE_cargo-psync"))
        .args(["psync", "check", "--notes", "--deny"])
        .current_dir(&fixture)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("run cargo psync again");
    let again_stderr = String::from_utf8_lossy(&again.stderr);
    assert_eq!(summary(&again_stderr), summary(&stderr), "{again_stderr}");
    assert!(
        !again.status.success(),
        "--deny passed on a cached run: {again_stderr}"
    );

    // With the compiler's facts, the macro schedules safely by itself and gains concurrency
    // where names were too pessimistic.
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-psync"))
        .args(["psync", "check", "--facts"])
        .current_dir(&fixture)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("run cargo psync check --facts");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let crates: Vec<CrateFindings> = psync_check::read_all(&target.join("psync").join("findings"));
    let app = crates
        .iter()
        .find(|c| c.crate_name == "app")
        .expect("app was checked");
    assert!(
        app.findings.iter().all(|f| f.severity != Severity::Warning),
        "{:#?}",
        app.findings
    );
    // `warm`/`load`, `check`/`get_token` and `checkpoint`/`inspect_db` now run in order; the
    // loop whose prefix writes is no longer prefetched.
    for sequential in [
        "start",
        "login",
        "snapshot",
        "sync_dirs",
        "tour",
        "release",
        "boot",
        "stash_same",
    ] {
        assert!(
            !app.functions.contains_key(sequential),
            "{:?}",
            app.functions
        );
    }
    // `create_view` only reads: its two calls now overlap.
    assert_eq!(
        app.functions.get("views"),
        Some(&(1, 0)),
        "{:?}",
        app.functions
    );
    // Each `stash` writes the file passed to it: the two different files overlap.
    assert_eq!(
        app.functions.get("stash_both"),
        Some(&(1, 0)),
        "{:?}",
        app.functions
    );
    assert!(stderr.contains("with compiler facts"));
}

/// Copies `dir` to `to`, without its `target` directory.
fn copy_dir(dir: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create");
    for entry in std::fs::read_dir(dir).expect("read fixture") {
        let entry = entry.expect("entry");
        let (from, dest) = (entry.path(), to.join(entry.file_name()));
        if entry.file_name() == "target" {
            continue;
        }
        if from.is_dir() {
            copy_dir(&from, &dest);
        } else {
            std::fs::copy(&from, &dest).expect("copy");
        }
    }
}

#[test]
fn fix_inserts_seq_hints_until_clean() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = root.join("target/fixture-fix");
    let _ = std::fs::remove_dir_all(&work);
    copy_dir(&root.join("tests/fixtures/hidden"), &work);
    // The copy is elsewhere: point it at psync, and at the pinned toolchain.
    let manifest = work.join("app/Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("manifest");
    let psync = root.parent().expect("repository").display().to_string();
    std::fs::write(&manifest, text.replace("../../../../..", &psync)).expect("manifest");
    std::fs::copy(
        root.join("rust-toolchain.toml"),
        work.join("rust-toolchain.toml"),
    )
    .expect("toolchain");
    let run = |command: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-psync"))
            .args(["psync", command])
            .current_dir(&work)
            .env("CARGO_TARGET_DIR", work.join("target"))
            .output()
            .expect("run cargo psync");
        String::from_utf8_lossy(&output.stderr).to_string()
    };
    let fixed = run("fix");
    assert!(
        fixed.contains("inserted 8 `#[psync::seq]` hint(s)"),
        "{fixed}"
    );
    let source = std::fs::read_to_string(work.join("app/src/lib.rs")).expect("source");
    assert_eq!(source.matches("#[psync::seq]").count(), 8);
    assert!(
        source.contains("#[psync::seq]\n    for d in dirs"),
        "{source}"
    );
    let checked = run("check");
    assert!(checked.contains("0 warning(s)"), "{checked}");
}
