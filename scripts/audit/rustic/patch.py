#!/usr/bin/env python3
"""Annotate rustic's RepoInfoCmd::inner_run and DiffCmd::inner_run for psync (Phase II).

The edits a user would make, and nothing else:
- repoinfo: the struct literal `Infos { files: .., index: .. }` becomes two `let`s marked
  `#[psync::par]` (psync schedules statements, not struct fields), and `repo.open(..)` becomes
  `Repo(repo.0.clone()).open(..)`, since `open` takes the repository by value and the other
  statement borrows it at the same time. The inner `Repository` is `Clone` (its backends are
  shared); rustic's `Repo` wrapper is not.
- diff: the two snapshot lookups are marked `#[psync::par]`.
"""
import pathlib, sys

root = pathlib.Path(sys.argv[1])


def edit(rel, pairs):
    p = root / rel
    s = p.read_text()
    for old, new in pairs:
        assert s.count(old) == 1, (rel, old[:60])
        s = s.replace(old, new)
    p.write_text(s)


edit("Cargo.toml", [("[dependencies]\n", '[dependencies]\npsync = { path = "/mnt/ssrhdd/psync/work", default-features = false, features = ["std"] }\n')])

edit("src/commands/repoinfo.rs", [(
    """    fn inner_run(&self, repo: Repo) -> Result<()> {
        let config = RUSTIC_APP.config();
        let infos = Infos {
            files: (!self.only_index)
                .then(|| -> Result<_> { Ok(repo.infos_files()?) })
                .transpose()?,
            index: (!self.only_files)
                .then(|| -> Result<_> {
                    Ok(repo
                        .open(&config.repository.credential_opts)?
                        .infos_index()?)
                })
                .transpose()?,
        };
""",
    """    #[psync::graph]
    fn inner_run(&self, repo: Repo) -> Result<()> {
        let config = RUSTIC_APP.config();
        #[psync::par]
        let files = (!self.only_index)
            .then(|| -> Result<_> { Ok(repo.infos_files()?) })
            .transpose()?;
        #[psync::par]
        let index = (!self.only_files)
            .then(|| -> Result<_> {
                Ok(Repo(repo.0.clone())
                    .open(&config.repository.credential_opts)?
                    .infos_index()?)
            })
            .transpose()?;
        let infos = Infos { files, index };
""")])

edit("src/commands/diff.rs", [
    ("    fn inner_run(&self, repo: IndexedRepo) -> Result<()> {",
     "    #[psync::graph]\n    fn inner_run(&self, repo: IndexedRepo) -> Result<()> {"),
    ("""                let node1 = repo.node_from_snapshot_and_path(snap1, path1)?;
                let node2 = repo.node_from_snapshot_and_path(snap2, path2)?;""",
     """                #[psync::par]
                let node1 = repo.node_from_snapshot_and_path(snap1, path1)?;
                #[psync::par]
                let node2 = repo.node_from_snapshot_and_path(snap2, path2)?;"""),
])
print("patched", root)
