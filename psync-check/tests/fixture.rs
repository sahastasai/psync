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
    // `sizes` reads two files: nothing to report.
    assert!(!app.findings.iter().any(|f| f.function == "sizes"));
    assert_eq!(app.findings.len(), 4, "{:#?}", app.findings);
    assert!(
        stderr.contains("psync check: 5 concurrent level(s)")
            || stderr.contains("concurrent level(s)")
    );
}
