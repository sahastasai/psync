#!/usr/bin/env python3
"""Prepare tauri-bundler's `prepare_tools` (AppImage tools: three downloads) for the Phase II audit.

    patch.py TAURI_DIR seq|par|spec|hinted

Every mode adds a benchmark test (`psync_bench::prepare_tools_bench`) that runs `prepare_tools` on
an empty tools directory PSYNC_RUNS times, prints the median wall time and a digest of the files
it wrote. The downloads go to a local mirror through tauri's own `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR`.

- seq: tauri as released;
- par: `#[psync::graph]` on the function, `#[psync::par]` on the three `if !exists { download }`
  statements (what a user marks first);
- spec: `par` and `#[psync::speculate]` on the second and third, built with `cargo psync build`:
  the compiler's facts say which file each helper writes (per-argument effects);
- hinted: `spec` and `#[psync::independent]` on the three (each writes only its own cache file, which
  psync cannot see through `download` and `write_and_make_executable`) and `#[psync::speculate]`
  on the second and third (filling a tool cache is harmless even if an earlier download fails).
"""
import pathlib, sys

root, mode = pathlib.Path(sys.argv[1]), sys.argv[2]
assert mode in ("seq", "par", "spec", "hinted")
src = root / "crates/tauri-bundler/src/bundle/linux/appimage/linuxdeploy.rs"
s = src.read_text()


def rep(old, new):
    global s
    assert s.count(old) == 1, old[:60]
    s = s.replace(old, new)


if mode != "seq":
    cargo = root / "crates/tauri-bundler/Cargo.toml"
    c = cargo.read_text()
    if "psync =" not in c:
        c = c.replace("[dependencies]\n", '[dependencies]\npsync = { path = "/mnt/ssrhdd/psync/work", default-features = false, features = ["std"] }\n', 1)
        cargo.write_text(c)
    ind = "  #[psync::independent]\n" if mode == "hinted" else ""
    spec = ind + "  #[psync::speculate]\n" if mode in ("spec", "hinted") else ""
    rep("fn prepare_tools(tools_path: &Path", "#[psync::graph]\nfn prepare_tools(tools_path: &Path")
    rep("""  let apprun = tools_path.join(format!("AppRun-{arch}"));
  if !apprun.exists() {""", """  let apprun = tools_path.join(format!("AppRun-{arch}"));
  #[psync::par]
""" + ind + """  if !apprun.exists() {""")
    rep("""  if !linuxdeploy.exists() {""", "  #[psync::par]\n" + spec + "  if !linuxdeploy.exists() {")
    rep("""  if !appimage.exists() {""", "  #[psync::par]\n" + spec + "  if !appimage.exists() {")

s += r'''
#[cfg(test)]
mod psync_bench {
  #[test]
  fn prepare_tools_bench() {
    let runs: usize = std::env::var("PSYNC_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(11);
    let mut times = Vec::new();
    for i in 0..runs {
      let dir = std::env::temp_dir().join(format!("psync-tauri-{}-{i}", std::process::id()));
      let _ = std::fs::remove_dir_all(&dir);
      std::fs::create_dir_all(&dir).unwrap();
      let t = std::time::Instant::now();
      super::prepare_tools(&dir, "x86_64", false).unwrap();
      times.push(t.elapsed().as_secs_f64() * 1000.0);
      if i == 0 {
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        for f in files {
          let data = std::fs::read(&f).unwrap();
          let sum = data.iter().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(*b as u64));
          println!("FILE {} {} {:016x}", f.file_name().unwrap().to_string_lossy(), data.len(), sum);
        }
      }
      std::fs::remove_dir_all(&dir).unwrap();
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("MEDIAN_MS {:.1}", times[runs / 2]);
  }
}
'''
src.write_text(s)
print("patched", mode)
