//! `#[psync::graph]` on real-world code shapes from the concurrency audit.
//!
//! Every example reconstructs a function from a popular Rust repository (named in its header),
//! with each I/O call replaced by a timer of realistic latency. For each one the suite prints the
//! original code and its runtime, the code psync generates and its runtime, and the speedup, and
//! checks that both versions return the same result. Both versions run on psync's executor.
//!
//! ```sh
//! cargo run --example graph_suite --no-default-features            # everything
//! cargo run --example graph_suite --no-default-features -- --brief # timings only
//! cargo run --example graph_suite --no-default-features -- kube    # matching examples
//! ```

mod runtime;
mod show;

mod chains;
mod late_start;
mod loops;
mod sequential_awaits;

use show::{indent, ms};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let brief = args.iter().any(|a| a == "--brief");
    let filter: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    let examples: Vec<_> = [
        sequential_awaits::examples(),
        loops::examples(),
        chains::examples(),
        late_start::examples(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut rows = Vec::new();
    for example in examples
        .iter()
        .filter(|e| filter.is_empty() || filter.iter().any(|f| e.origin.contains(f.as_str())))
    {
        let (original, rewritten, graph) = show::rewrite(example);
        let (before, t_before) = runtime::time((example.sequential)());
        let (after, t_after) = runtime::time((example.graph)());
        assert_eq!(
            before, after,
            "{}: the rewrite changed the result",
            example.origin
        );
        let speedup = t_before.as_secs_f64() / t_after.as_secs_f64();

        println!("{}", "=".repeat(100));
        println!(
            "{}\n{}: {}",
            example.origin, example.pattern, example.summary
        );
        if !brief {
            println!("\n--- initial code ---\n{}", indent(&original));
        }
        println!("initial runtime: {}   (result: {before})", ms(t_before));
        if !brief {
            let attribute = if example.options.is_empty() {
                "#[psync::graph]".to_string()
            } else {
                format!("#[psync::graph({})]", example.options)
            };
            println!("\n--- rewrite ({attribute}) ---\n{}", indent(&rewritten));
            println!("--- dependency graph ---\n{graph}");
        }
        println!("rewrite runtime: {}   (result: {after})", ms(t_after));
        println!("speedup: {speedup:.2}x");
        rows.push((example.pattern, example.origin, t_before, t_after, speedup));
    }

    println!(
        "{}\n{:<46} {:>10} {:>10} {:>8}  origin",
        "=".repeat(100),
        "pattern",
        "initial",
        "rewrite",
        "speedup"
    );
    for (pattern, origin, before, after, speedup) in &rows {
        let origin = origin.split("  ").next().unwrap_or(origin);
        println!(
            "{pattern:<46} {:>10} {:>10} {speedup:>7.2}x  {origin}",
            ms(*before),
            ms(*after)
        );
    }
    let geo = (rows.iter().map(|r| r.4.ln()).sum::<f64>() / rows.len().max(1) as f64).exp();
    println!(
        "geometric mean speedup over {} examples: {geo:.2}x",
        rows.len()
    );
}
