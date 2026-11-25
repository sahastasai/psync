//! Phase II measurements: synchronous code under `#[psync::graph]`, against its sequential
//! original (`#[psync::graph(off)]`), on real sockets and files.
//!
//! A local server answers each request after `LATENCY`, standing in for a remote backend (S3,
//! GitHub releases). Run with `cargo run --release --example phase2 --no-default-features
//! --features std`.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

const LATENCY: Duration = Duration::from_millis(50);

/// Answers `GET <path>` with the path repeated, after `LATENCY`.
fn server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            std::thread::spawn(move || {
                let mut line = [0u8; 64];
                let n = conn.read(&mut line).unwrap();
                std::thread::sleep(LATENCY);
                let body = line[4..n].repeat(1000);
                let _ = conn.write_all(&body);
            });
        }
    });
    addr
}

/// One request to the server, as tauri's `http_utils::download` makes one with `ureq`.
fn download(addr: SocketAddr, what: &str) -> std::io::Result<Vec<u8>> {
    let mut s = TcpStream::connect(addr)?;
    s.write_all(format!("GET {what}").as_bytes())?;
    let mut v = Vec::new();
    s.read_to_end(&mut v)?;
    Ok(v)
}

/// A repository behind a remote backend, as rustic's `Repository` is.
pub struct Repo(SocketAddr);

impl Repo {
    pub fn list_files(&self) -> std::io::Result<usize> {
        Ok(download(self.0, "list")?.len())
    }
    pub fn read_index(&self) -> std::io::Result<usize> {
        Ok(download(self.0, "index")?.len())
    }
}

macro_rules! shapes {
    ($($mode:ident = $attr:meta;)*) => {$(
        mod $mode {
            use super::*;

            /// tauri `prepare_tools` (app-16): three downloads to distinct paths, then the writes.
            #[$attr]
            pub fn prepare_tools(addr: SocketAddr, dir: &str) -> std::io::Result<usize> {
                #[psync::par]
                let apprun = download(addr, "apprun")?;
                #[psync::par]
                let linuxdeploy = download(addr, "linuxdeploy")?;
                #[psync::par]
                let plugin = download(addr, "plugin")?;
                std::fs::write(format!("{dir}/AppRun"), &apprun)?;
                std::fs::write(format!("{dir}/linuxdeploy"), &linuxdeploy)?;
                std::fs::write(format!("{dir}/plugin"), &plugin)?;
                Ok(apprun.len() + linuxdeploy.len() + plugin.len())
            }

            /// The same as tauri writes it: each download and its write in one `if`.
            #[$attr]
            pub fn prepare_tools_guarded(addr: SocketAddr, dir: &str) -> std::io::Result<()> {
                #[psync::par]
                if !std::path::Path::new(&format!("{dir}/g-AppRun")).exists() {
                    let data = download(addr, "apprun")?;
                    std::fs::write(format!("{dir}/g-AppRun"), data)?;
                }
                #[psync::par]
                if !std::path::Path::new(&format!("{dir}/g-linuxdeploy")).exists() {
                    let data = download(addr, "linuxdeploy")?;
                    std::fs::write(format!("{dir}/g-linuxdeploy"), data)?;
                }
                #[psync::par]
                if !std::path::Path::new(&format!("{dir}/g-plugin")).exists() {
                    let data = download(addr, "plugin")?;
                    std::fs::write(format!("{dir}/g-plugin"), data)?;
                }
                Ok(())
            }

            /// The same, with the later writes allowed to happen even if an earlier download
            /// fails (a complete file in a tool cache is harmless).
            #[$attr]
            pub fn prepare_tools_speculate(addr: SocketAddr, dir: &str) -> std::io::Result<()> {
                #[psync::par]
                if !std::path::Path::new(&format!("{dir}/s-AppRun")).exists() {
                    let data = download(addr, "apprun")?;
                    std::fs::write(format!("{dir}/s-AppRun"), data)?;
                }
                #[psync::par]
                #[psync::speculate]
                if !std::path::Path::new(&format!("{dir}/s-linuxdeploy")).exists() {
                    let data = download(addr, "linuxdeploy")?;
                    std::fs::write(format!("{dir}/s-linuxdeploy"), data)?;
                }
                #[psync::par]
                #[psync::speculate]
                if !std::path::Path::new(&format!("{dir}/s-plugin")).exists() {
                    let data = download(addr, "plugin")?;
                    std::fs::write(format!("{dir}/s-plugin"), data)?;
                }
                Ok(())
            }

            /// rustic `repoinfo` (tools-29): list the repository and read its index.
            #[$attr]
            pub fn repoinfo(repo: &Repo) -> std::io::Result<(usize, usize)> {
                #[psync::par]
                let files = repo.list_files()?;
                #[psync::par]
                let index = repo.read_index()?;
                Ok((files, index))
            }

            /// A miss: two cached `stat`s, marked `#[psync::par]` anyway.
            #[$attr]
            pub fn two_stats(a: &str, b: &str) -> std::io::Result<u64> {
                #[psync::par]
                let x = std::fs::metadata(a)?.len();
                #[psync::par]
                let y = std::fs::metadata(b)?.len();
                Ok(x + y)
            }
        }
    )*};
}

shapes! {
    sequential = psync::graph(off);
    graph = psync::graph;
}

fn median(mut f: impl FnMut(), runs: usize) -> Duration {
    let mut t: Vec<Duration> = (0..runs)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed()
        })
        .collect();
    t.sort();
    t[runs / 2]
}

fn row(name: &str, runs: usize, mut seq: impl FnMut(), mut par: impl FnMut()) {
    let (s, p) = (median(&mut seq, runs), median(&mut par, runs));
    println!(
        "{name:<34} {:>10.1?} {:>10.1?} {:>6.2}x",
        s,
        p,
        s.as_secs_f64() / p.as_secs_f64()
    );
}

fn main() {
    let addr = server();
    let repo = Repo(addr);
    let dir = std::env::temp_dir().join(format!("psync-phase2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap().to_string();
    assert_eq!(
        sequential::prepare_tools(addr, &d).unwrap(),
        graph::prepare_tools(addr, &d).unwrap()
    );
    assert_eq!(
        sequential::repoinfo(&repo).unwrap(),
        graph::repoinfo(&repo).unwrap()
    );
    println!(
        "{:<34} {:>10} {:>10} {:>7}",
        "shape", "sequential", "psync", "speedup"
    );
    row(
        "tauri prepare_tools (3 GETs)",
        21,
        || {
            sequential::prepare_tools(addr, &d).unwrap();
        },
        || {
            graph::prepare_tools(addr, &d).unwrap();
        },
    );
    row(
        "rustic repoinfo (list + index)",
        21,
        || {
            sequential::repoinfo(&repo).unwrap();
        },
        || {
            graph::repoinfo(&repo).unwrap();
        },
    );
    let clean = |d: &str| {
        for f in ["g-AppRun", "g-linuxdeploy", "g-plugin"] {
            let _ = std::fs::remove_file(format!("{d}/{f}"));
        }
    };
    row(
        "tauri, as written (cold cache)",
        21,
        || {
            clean(&d);
            sequential::prepare_tools_guarded(addr, &d).unwrap();
        },
        || {
            clean(&d);
            graph::prepare_tools_guarded(addr, &d).unwrap();
        },
    );
    let clean_s = |d: &str| {
        for f in ["s-AppRun", "s-linuxdeploy", "s-plugin"] {
            let _ = std::fs::remove_file(format!("{d}/{f}"));
        }
    };
    row(
        "  + #[psync::speculate]",
        21,
        || {
            clean_s(&d);
            sequential::prepare_tools_speculate(addr, &d).unwrap();
        },
        || {
            clean_s(&d);
            graph::prepare_tools_speculate(addr, &d).unwrap();
        },
    );
    let (a, b) = ("Cargo.toml", "README.md");
    row(
        "two cached stats (a miss)",
        2001,
        || {
            sequential::two_stats(a, b).unwrap();
        },
        || {
            graph::two_stats(a, b).unwrap();
        },
    );
    let mut x = 0u64;
    let handoff = median(
        || {
            let (mut p, mut q) = (0u64, 0u64);
            psync::__private::par_run(&mut [&mut || p = std::hint::black_box(1), &mut || q = 2]);
            x += p + q;
        },
        2001,
    );
    println!("par_run, two empty jobs: {handoff:.1?} (median)");
    std::hint::black_box(x);
    let _ = std::fs::remove_dir_all(&dir);
}
