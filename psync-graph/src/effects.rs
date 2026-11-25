//! Effects on the world outside the function: files, the network, other services, processes and
//! the environment.
//!
//! The access analysis ([`crate::access`]) sees the function's own variables and fields. It cannot
//! see that `api.delete(name).await` and `api.create(obj).await` touch the same object on a
//! server, or that `fs::write("a", ..)` and `fs::read("a")` touch the same file: neither statement
//! writes a variable the other reads. This module gives every statement a list of [`Effect`]s,
//! each one an operation of some [`Kind`] on a target in a [`Domain`], recognized from the call:
//!
//! - **Files**: path-qualified calls into a file-system module (`std::fs::write`,
//!   `tokio::fs::read_to_string`, `fs::remove_file`), `File::create`/`File::open`, `OpenOptions`.
//!   The target is the path argument.
//! - **Network**: `TcpStream::connect`, `UdpSocket::bind`, `send_to`, ... on the `net` types.
//! - **Processes**: `Command::new`, `process::exit`, `kill`. A child process may touch anything,
//!   so a process effect conflicts with every other domain.
//! - **Environment**: `env::set_var`, `env::remove_var`, `set_current_dir` write it; `env::var`
//!   reads it.
//! - **Services**: any other awaited call, classified by the verb its name starts with
//!   (`delete_object`, `create`, `post`, `put`, `upload`, `send` write; `get`, `list`, `head`,
//!   `fetch` read). The target domain is the receiver (`self.s3`, `api`), and the target is the
//!   verb's first argument. An awaited free function (`delete_user(id).await`) reaches an unknown
//!   service, which may be any receiver.
//!
//! Two effects **conflict** when one writes, their domains may be the same resource, and their
//! targets may be the same: two distinct string literals (`"a.toml"`, `"b.toml"`) are known to
//! differ, and in a loop two targets computed from the loop item are assumed to differ between
//! iterations (each item names its own object); anything else may be the same. Conflicting
//! statements keep their order.
//!
//! A write is also **irreversible**: psync does not run it ahead of an earlier statement that may
//! leave the function or loop (`?`, `return`, `break`), because sequential code would then not
//! have performed it. `#[psync::speculate]` on the statement (or loop, or `speculate` on the
//! whole function) asserts that running it anyway is harmless, for idempotent operations.
//!
//! An awaited call whose name has no known verb (`client.request(req)`, `do_work(x)`) is
//! *unclassified*. By default it is assumed to read (the behavior before effects existed), and
//! reported. Under `#[psync::graph(strict)]` it is a write to an unknown resource, which
//! conflicts with every effect and never runs speculatively.

use crate::access::{Set, mentions, overlaps};
use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::{Expr, spanned::Spanned, visit::Visit};

/// Where an effect lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Domain {
    Fs,
    Net,
    Env,
    /// A child process, which may touch any other domain.
    Process,
    /// A service reached through a receiver path (`self.client`, `api`); `None` is a service
    /// reached by a free function, which may be behind any receiver.
    Service(Option<String>),
    /// An unclassified call under `strict`: anything.
    Unknown,
}

impl Domain {
    fn may_alias(&self, other: &Domain) -> bool {
        use Domain::*;
        match (self, other) {
            (Process | Unknown, _) | (_, Process | Unknown) => true,
            (Service(a), Service(b)) => match (a, b) {
                // A service the compiler named by its type (`type:kube::Api::<K>`) may be behind
                // any receiver variable; two types are the same service only if equal.
                (Some(a), Some(b)) if a.starts_with("type:") != b.starts_with("type:") => true,
                (Some(a), Some(b)) => overlaps(a, b),
                _ => true,
            },
            (a, b) => a == b,
        }
    }

    fn name(&self) -> String {
        match self {
            Domain::Fs => "file".into(),
            Domain::Net => "network".into(),
            Domain::Env => "environment".into(),
            Domain::Process => "process".into(),
            Domain::Service(Some(r)) => format!("`{r}`"),
            Domain::Service(None) => "service".into(),
            Domain::Unknown => "unknown".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Read,
    Write,
}

/// What an effect acts on inside its domain.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A string literal (a file path, a key).
    Literal(String),
    /// `format!("prefix{x}..")`: a computed string that starts with known text.
    Prefixed(String, Set),
    /// `format!("{dir}/cache.json")`: text with holes, each a plain expression (a name or a
    /// field), and the names it mentions. Within one region an expression has one value, so two
    /// templates that agree piece by piece and then differ in their text name different strings.
    Template(Vec<Piece>, Set),
    /// A computed value: its tokens and the names it mentions.
    Expr(String, Set),
    /// Unknown: the whole domain.
    Any,
}

/// One piece of a [`Target::Template`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Char(char),
    /// A placeholder: the expression and its format spec (`dir:?`).
    Hole(String),
}

/// A template's pieces, ended as a place ends: a file path with `/` (a directory overlaps the
/// files in it), any other string with an end marker (it overlaps only itself).
fn ended(pieces: &[Piece], fs: bool) -> Vec<Option<Piece>> {
    let mut v: Vec<Option<Piece>> = pieces.iter().cloned().map(Some).collect();
    if fs {
        while matches!(v.last(), Some(Some(Piece::Char('/')))) {
            v.pop();
        }
        v.push(Some(Piece::Char('/')));
    } else {
        v.push(None);
    }
    v
}

/// Two ended templates may name the same string: they agree until one runs out (one is a prefix
/// of the other), or they first differ where either has a hole (a hole may be any text).
fn templates_alias(a: &[Option<Piece>], b: &[Option<Piece>]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return matches!(x, Some(Piece::Hole(_))) || matches!(y, Some(Piece::Hole(_)));
        }
    }
    true
}

/// `format!("..", args..)` as a template, if every placeholder is a plain expression.
fn template(tokens: TokenStream) -> Option<Vec<Piece>> {
    use syn::punctuated::Punctuated;
    let args =
        syn::parse::Parser::parse2(Punctuated::<Expr, syn::Token![,]>::parse_terminated, tokens)
            .ok()?;
    let mut args = args.into_iter();
    let Some(Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(text),
        ..
    })) = args.next()
    else {
        return None;
    };
    /// A name or a field path, which has one value within a region.
    fn plain(e: &Expr) -> Option<String> {
        match e {
            Expr::Path(p) if p.qself.is_none() => Some(p.to_token_stream().to_string()),
            Expr::Field(f) => {
                plain(&f.base)?;
                Some(f.to_token_stream().to_string())
            }
            Expr::Reference(r) => plain(&r.expr),
            Expr::Paren(p) => plain(&p.expr),
            _ => None,
        }
    }
    let (mut positional, mut named) = (Vec::new(), std::collections::HashMap::new());
    for a in args {
        match a {
            Expr::Assign(x) => {
                let Expr::Path(name) = &*x.left else {
                    return None;
                };
                named.insert(name.to_token_stream().to_string(), plain(&x.right)?);
            }
            e => positional.push(plain(&e)?),
        }
    }
    let (mut pieces, mut next) = (Vec::new(), 0usize);
    let mut chars = text
        .value()
        .chars()
        .collect::<Vec<_>>()
        .into_iter()
        .peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' if chars.peek() == Some(&c) => {
                chars.next();
                pieces.push(Piece::Char(c));
            }
            '{' => {
                let inner: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let (arg, spec) = inner.split_once(':').unwrap_or((&inner, ""));
                // Widths and precisions taken from arguments are not followed.
                if spec.contains('$') || spec.contains('*') {
                    return None;
                }
                let expr = if arg.is_empty() {
                    next += 1;
                    positional.get(next - 1)?.clone()
                } else if let Ok(n) = arg.parse::<usize>() {
                    positional.get(n)?.clone()
                } else if let Some(e) = named.get(arg) {
                    e.clone()
                } else if arg.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    arg.to_string()
                } else {
                    return None;
                };
                pieces.push(Piece::Hole(format!("{expr}:{spec}")));
            }
            '}' => return None,
            c => pieces.push(Piece::Char(c)),
        }
    }
    Some(pieces)
}

impl Target {
    /// A template as the coarser target it refines: the text before its first hole.
    fn coarse(&self) -> Target {
        match self {
            Target::Template(pieces, names) => {
                let prefix: String = pieces
                    .iter()
                    .map_while(|p| match p {
                        Piece::Char(c) => Some(*c),
                        Piece::Hole(_) => None,
                    })
                    .collect();
                if prefix.is_empty() {
                    Target::Expr(String::new(), names.clone())
                } else {
                    Target::Prefixed(prefix, names.clone())
                }
            }
            t => t.clone(),
        }
    }

    fn may_alias(&self, other: &Target, fs: bool) -> bool {
        match (self, other) {
            (Target::Template(a, _), Target::Template(b, _)) => {
                return templates_alias(&ended(a, fs), &ended(b, fs));
            }
            (Target::Template(..), _) | (_, Target::Template(..)) => {
                return self.coarse().may_alias(&other.coarse(), fs);
            }
            _ => {}
        }
        match (self, other) {
            // Known text that cannot start the same way names different strings.
            (Target::Prefixed(p, _), Target::Literal(l))
            | (Target::Literal(l), Target::Prefixed(p, _)) => l.starts_with(p.as_str()),
            (Target::Prefixed(a, _), Target::Prefixed(b, _)) => {
                a.starts_with(b.as_str()) || b.starts_with(a.as_str())
            }
            (Target::Literal(a), Target::Literal(b)) => {
                if fs {
                    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
                    a == b
                        || b.starts_with(a) && b[a.len()..].starts_with('/')
                        || a.starts_with(b) && a[b.len()..].starts_with('/')
                } else {
                    a == b
                }
            }
            _ => true,
        }
    }

    /// The target is computed from one of `names` (per-iteration bindings).
    pub fn mentions_any(&self, names: &Set) -> bool {
        matches!(self, Target::Expr(_, m) | Target::Prefixed(_, m) | Target::Template(_, m) if m.iter().any(|n| names.contains(n)))
    }
}

/// One operation on the outside world.
#[derive(Clone, Debug)]
pub(crate) struct Effect {
    pub domain: Domain,
    pub kind: Kind,
    pub target: Target,
    /// The call, for reports (`fs::write`, `delete`).
    pub op: String,
    /// The order of these operations is observable even on different targets (`send`, `append`,
    /// `publish`): two of them on one domain never reorder.
    pub ordered: bool,
}

impl Effect {
    pub fn writes(&self) -> bool {
        self.kind == Kind::Write
    }

    /// The place this effect touches, for certificates: `w/<domain>/<target..>`. Places overlap
    /// by prefix, so where `conflict` is finer than a prefix (string-prefix and computed targets,
    /// services named by type, ordered writes that conflict on any target), the place is cut
    /// short, which only adds conflicts.
    pub fn place(&self) -> Vec<String> {
        let mut place = vec!["w".to_string()];
        let svc = |r: &str| -> Vec<String> {
            let mut v = vec!["svc".to_string()];
            if !r.starts_with("type:") {
                v.extend(r.split('.').map(str::to_string));
            }
            v
        };
        match &self.domain {
            Domain::Process | Domain::Unknown => return place,
            Domain::Fs => place.push("fs".into()),
            Domain::Net => place.push("net".into()),
            Domain::Env => place.push("env".into()),
            // An unknown service may be any receiver: stop at the domain.
            Domain::Service(None) => {
                place.push("svc".into());
                return place;
            }
            Domain::Service(Some(r)) => {
                // A service is the receiver; a typed service may be behind any receiver.
                place.extend(svc(r));
                if r.starts_with("type:") {
                    return place;
                }
            }
        }
        if self.ordered {
            return place;
        }
        // Targets as characters, so that a prefix of the place is a prefix of the string: a
        // literal ends with a marker (two different literals never overlap), a `format!` prefix
        // has none (it overlaps every string that starts with it), and a file path ends with `/`
        // (a directory overlaps the files in it).
        let chars = |t: &str| t.chars().map(String::from).collect::<Vec<_>>();
        match &self.target {
            Target::Literal(l) if self.domain == Domain::Fs => {
                place.push("t".into());
                place.extend(chars(&format!("{}/", l.trim_end_matches('/'))));
            }
            Target::Literal(l) => {
                place.push("t".into());
                place.extend(chars(l));
                place.push("$end".into());
            }
            Target::Prefixed(p, _) => {
                place.push("t".into());
                place.extend(chars(p));
            }
            // Holes are marked, for the checker: places that first differ at a hole overlap.
            Target::Template(pieces, _) => {
                place.push("t".into());
                for p in ended(pieces, self.domain == Domain::Fs) {
                    place.push(match p {
                        Some(Piece::Char(c)) => c.to_string(),
                        Some(Piece::Hole(h)) => format!("$hole:{h}"),
                        None => "$end".into(),
                    });
                }
            }
            Target::Expr(..) | Target::Any => {}
        }
        place
    }

    /// For reports: `write file "cfg.toml" (fs::write)`.
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            Kind::Read => "read",
            Kind::Write => "write",
        };
        let target = match &self.target {
            Target::Literal(l) => format!(" {l:?}"),
            Target::Expr(e, _) => format!(" `{}`", short(e)),
            Target::Prefixed(p, _) => format!(" {p:?}…"),
            Target::Template(pieces, _) => {
                let text: String = pieces
                    .iter()
                    .map(|p| match p {
                        Piece::Char(c) => c.to_string(),
                        Piece::Hole(h) => format!("{{{}}}", h.trim_end_matches(':')),
                    })
                    .collect();
                format!(" {text:?}")
            }
            Target::Any => String::new(),
        };
        format!("{kind} {}{target} ({})", self.domain.name(), self.op)
    }
}

fn short(s: &str) -> String {
    match s.char_indices().nth(30) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

/// Why two effects conflict, if they do.
pub(crate) fn conflict(a: &Effect, b: &Effect) -> Option<String> {
    if !(a.writes() || b.writes()) || !a.domain.may_alias(&b.domain) {
        return None;
    }
    let fs = a.domain == Domain::Fs && b.domain == Domain::Fs;
    // A process or unknown call may touch any target; ordered operations conflict on any target.
    let anything = |d: &Domain| matches!(d, Domain::Process | Domain::Unknown);
    let ordered = a.ordered && b.ordered;
    if !ordered
        && !anything(&a.domain)
        && !anything(&b.domain)
        && !a.target.may_alias(&b.target, fs)
    {
        return None;
    }
    let (w, o) = if a.writes() { (a, b) } else { (b, a) };
    Some(format!("{} vs {}", w.describe(), o.describe()))
}

/// The first conflict between two effect lists.
pub(crate) fn conflicts(a: &[Effect], b: &[Effect]) -> Option<String> {
    a.iter().find_map(|x| b.iter().find_map(|y| conflict(x, y)))
}

/// Function-wide settings from `#[psync::graph(..)]`.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Unclassified awaited calls are writes to an unknown resource.
    pub strict: bool,
    /// Writes may run ahead of earlier exits everywhere in the function.
    pub speculate: bool,
    /// Extra verbs (whole names or first words) that read, from `effects(read(..))`.
    pub reads: Vec<String>,
    /// Extra verbs that write, from `effects(write(..))`.
    pub writes: Vec<String>,
    /// Extra verbs whose order is observable, from `effects(ordered(..))`.
    pub ordered: Vec<String>,
}

/// First words of names that read.
const READ_VERBS: &[&str] = &[
    "get",
    "head",
    "list",
    "fetch",
    "read",
    "load",
    "query",
    "select",
    "find",
    "lookup",
    "describe",
    "stat",
    "exists",
    "search",
    "resolve",
    "download",
    "retrieve",
    "watch",
    "scan",
    "count",
    "check",
    "peek",
    "options",
    "metadata",
    "open",
    "inspect",
    "show",
    "info",
    "status",
    "health",
    "ping",
    "poll",
    "receive",
    "batch_get",
    "is",
    "has",
    "try_exists",
    "canonicalize",
    "verify",
    "validate",
    "connect",
    "probe",
    "discover",
];

/// First words of names that write.
const WRITE_VERBS: &[&str] = &[
    "create",
    "insert",
    "put",
    "post",
    "patch",
    "delete",
    "del",
    "remove",
    "rm",
    "update",
    "replace",
    "upsert",
    "set",
    "write",
    "rename",
    "copy",
    "upload",
    "execute",
    "exec",
    "commit",
    "rollback",
    "spawn",
    "kill",
    "apply",
    "merge",
    "truncate",
    "drop",
    "evict",
    "invalidate",
    "purge",
    "register",
    "unregister",
    "grant",
    "revoke",
    "restart",
    "scale",
    "cancel",
    "transfer",
    "incr",
    "increment",
    "decr",
    "decrement",
    "save",
    "store",
    "persist",
    "flush",
    "sync",
    "mark",
    "reset",
    "clear",
    "add",
    "modify",
    "edit",
    "approve",
    "reject",
    "submit",
    "deploy",
    "install",
    "uninstall",
    "mkdir",
    "rmdir",
    "unlink",
    "chmod",
    "chown",
    "touch",
    "link",
    "symlink",
    "compact",
    "migrate",
    "seed",
    "move",
    "mv",
    "claim",
    "release",
    "renew",
    "expire",
    "lease",
    "start",
    "stop",
    "terminate",
    "shutdown",
    "close",
    "run",
    "invoke",
    "call",
    "trigger",
    "charge",
    "refund",
    "pay",
    "mint",
    "burn",
    "sign",
    "issue",
    "reserve",
    "allocate",
    "free",
    "destroy",
    "provision",
    "attach",
    "detach",
    "mount",
    "unmount",
    "bind",
    "unbind",
];

/// First words of names whose relative order is observable on one domain.
const ORDERED_VERBS: &[&str] = &[
    "send",
    "append",
    "push",
    "publish",
    "enqueue",
    "emit",
    "notify",
    "ack",
    "nack",
    "pop",
    "dequeue",
    "broadcast",
    "log",
    "produce",
    "write_all",
    "feed",
];

/// File-system functions (last path segment) that only read.
const FS_READS: &[&str] = &[
    "read",
    "read_to_string",
    "read_dir",
    "metadata",
    "symlink_metadata",
    "canonicalize",
    "read_link",
    "try_exists",
    "exists",
    "open",
];

/// Methods of `std::path::Path` without arguments that read the file system at the path.
const PATH_FS_READS: &[&str] = &[
    "exists",
    "try_exists",
    "is_file",
    "is_dir",
    "is_symlink",
    "metadata",
    "symlink_metadata",
    "read_dir",
    "read_link",
    "canonicalize",
];

/// Network functions that write (send data); the rest of `net` only connects or reads.
const NET_WRITES: &[&str] = &["send_to", "send", "write", "write_all"];

/// What a function or method name says about the operation, for tools outside the macro
/// (`cargo psync check` classifies calls into other crates with the same lists).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Read,
    Write,
    /// A write whose order relative to others on the same resource is observable.
    Ordered,
}

/// The verb a name starts with, if psync knows it.
pub fn verb_of(name: &str) -> Option<Verb> {
    Config::default()
        .verb(name)
        .map(|(kind, ordered)| match (kind, ordered) {
            (Kind::Read, _) => Verb::Read,
            (Kind::Write, true) => Verb::Ordered,
            (Kind::Write, false) => Verb::Write,
        })
}

/// True for file-system functions that only read (`read_to_string`, `metadata`, `open`, ...).
pub fn fs_reads(name: &str) -> bool {
    FS_READS.contains(&name)
}

fn first_word(name: &str) -> &str {
    let name = name.trim_start_matches("try_").trim_start_matches("async_");
    name.split('_').next().unwrap_or(name)
}

impl Config {
    /// How a call named `name` acts on a service, if its name says.
    fn verb(&self, name: &str) -> Option<(Kind, bool)> {
        let word = first_word(name);
        let is = |list: &[&str]| list.contains(&name) || list.contains(&word);
        let user = |list: &[String]| list.iter().any(|v| v == name || v == word);
        if user(&self.ordered) {
            Some((Kind::Write, true))
        } else if user(&self.writes) {
            Some((Kind::Write, false))
        } else if user(&self.reads) {
            Some((Kind::Read, false))
        } else if is(ORDERED_VERBS) {
            Some((Kind::Write, true))
        } else if is(WRITE_VERBS) {
            Some((Kind::Write, false))
        } else if is(READ_VERBS) {
            Some((Kind::Read, false))
        } else {
            None
        }
    }
}

fn target_of(arg: Option<&Expr>) -> Target {
    match arg {
        Some(Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        })) => Target::Literal(s.value()),
        Some(Expr::Reference(r)) => target_of(Some(&r.expr)),
        Some(Expr::Paren(p)) => target_of(Some(&p.expr)),
        // `&format!("key.{id}")`: keep the literal text before the first placeholder.
        Some(Expr::Macro(m)) if m.mac.path.is_ident("format") => {
            let tokens = m.mac.tokens.clone();
            if let Some(pieces) = template(tokens.clone()) {
                return if pieces.iter().any(|p| matches!(p, Piece::Hole(_))) {
                    Target::Template(pieces, mentions(tokens))
                } else {
                    Target::Literal(
                        pieces
                            .iter()
                            .map(|p| match p {
                                Piece::Char(c) => *c,
                                Piece::Hole(_) => unreachable!(),
                            })
                            .collect(),
                    )
                };
            }
            let first = syn::parse2::<syn::LitStr>(tokens.clone().into_iter().take(1).collect());
            let names = mentions(tokens.clone());
            match first {
                Ok(lit) => {
                    let text = lit.value();
                    let prefix: String = text.split('{').next().unwrap_or_default().into();
                    if prefix.is_empty() {
                        Target::Expr(tokens.to_string(), names)
                    } else {
                        Target::Prefixed(prefix, names)
                    }
                }
                Err(_) => Target::Expr(tokens.to_string(), names),
            }
        }
        Some(e) => {
            let tokens = e.to_token_stream();
            Target::Expr(tokens.to_string(), mentions(tokens))
        }
        None => Target::Any,
    }
}

/// The target named by the value of a binding's initializer: `target_of`, and also
/// `base.join(name)` with `base` a name or field and `name` a string literal or `format!` whose
/// text starts with a character that cannot make it absolute: the path `base/name`, as a
/// template whose first hole is `base`. (`Path::join` with an absolute `name` replaces `base`;
/// `/`, `\` and a drive letter `C:` are refused.)
pub(crate) fn value_target(e: &Expr) -> Target {
    if let Expr::MethodCall(m) = e
        && m.method == "join"
        && m.args.len() == 1
        && let Some(base) = plain_place(&m.receiver)
    {
        let rest = match target_of(m.args.first()) {
            Target::Literal(l) => Some(l.chars().map(Piece::Char).collect::<Vec<_>>()),
            Target::Template(p, _) => Some(p),
            _ => None,
        };
        if let Some(rest) = rest
            && let Some(Piece::Char(c)) = rest.first()
            && !matches!(c, '/' | '\\')
            && rest.get(1) != Some(&Piece::Char(':'))
        {
            let mut pieces = vec![Piece::Hole(format!("{base}:")), Piece::Char('/')];
            pieces.extend(rest);
            let names = mentions(e.to_token_stream());
            return Target::Template(pieces, names);
        }
        return target_of(Some(e));
    }
    target_of(Some(e))
}

/// A name or a field path (`dir`, `self.root`), as tokens.
fn plain_place(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(p.to_token_stream().to_string())
        }
        Expr::Field(f) => {
            plain_place(&f.base)?;
            Some(f.to_token_stream().to_string())
        }
        Expr::Reference(r) if r.mutability.is_none() => plain_place(&r.expr),
        Expr::Paren(p) => plain_place(&p.expr),
        _ => None,
    }
}

impl Target {
    /// The binding a computed target is, when it is just a name (`path`, `&path`).
    pub fn single_name(&self) -> Option<&str> {
        match self {
            Target::Expr(tokens, names) if names.len() == 1 => {
                let name = names.iter().next()?;
                let t = tokens.replace(' ', "");
                (t == *name || t == format!("&{name}")).then_some(name.as_str())
            }
            _ => None,
        }
    }

    /// Says more than "anything": a literal, a known prefix or a template.
    pub fn informative(&self) -> bool {
        matches!(
            self,
            Target::Literal(_) | Target::Prefixed(..) | Target::Template(..)
        )
    }

    /// The names its value depends on.
    pub fn names(&self) -> Option<&Set> {
        match self {
            Target::Prefixed(_, n) | Target::Template(_, n) | Target::Expr(_, n) => Some(n),
            _ => None,
        }
    }
}

/// The dotted path of a receiver (`self.client`), or `None` when it is computed.
fn receiver_path(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) if p.path.segments.len() == 1 => Some(p.path.segments[0].ident.to_string()),
        Expr::Field(f) => {
            let base = receiver_path(&f.base)?;
            match &f.member {
                syn::Member::Named(n) => Some(format!("{base}.{n}")),
                syn::Member::Unnamed(i) => Some(format!("{base}.{}", i.index)),
            }
        }
        Expr::Reference(r) => receiver_path(&r.expr),
        Expr::Paren(p) => receiver_path(&p.expr),
        Expr::Unary(u) => receiver_path(&u.expr),
        // `self.client()`, `api.clone()`: a handle derived from the receiver.
        Expr::MethodCall(m) if m.args.is_empty() => receiver_path(&m.receiver),
        _ => None,
    }
}

/// The effects of a statement or expression, and the awaited calls psync could not classify.
#[derive(Default)]
pub(crate) struct Found {
    pub effects: Vec<Effect>,
    pub unclassified: Vec<String>,
    /// The compiler says a call here panics on purpose (only with facts, see [`crate::facts`]).
    pub panics: bool,
}

/// Find the effects in `node` (a statement's tokens, parsed by the caller).
pub(crate) fn of_stmt(stmt: &syn::Stmt, config: &Config) -> Found {
    of_stmt_in(stmt, config, false)
}

/// [`of_stmt`], in synchronous code when `sync` is set (Phase II): there a call blocks instead of
/// being awaited, so every method chain and free function is classified by its verb as an awaited
/// one is. A call without a known verb has no effect, and is not reported: in synchronous code
/// most calls are computations (`len`, `Vec::new`).
pub(crate) fn of_stmt_in(stmt: &syn::Stmt, config: &Config, sync: bool) -> Found {
    let mut v = Finder {
        config,
        found: Found::default(),
        awaited: 0,
        sync,
        quiet: false,
        chained: Default::default(),
    };
    v.visit_stmt(stmt);
    v.found
}

struct Finder<'a> {
    config: &'a Config,
    found: Found,
    /// Depth of `.await` bases being visited: method chains count as service calls only there.
    awaited: usize,
    /// Synchronous code: method chains and free functions count as service calls everywhere.
    sync: bool,
    /// Classifying a synchronous call: a call without a verb is no effect and no report.
    quiet: bool,
    /// Method calls already classified as part of a longer chain (by address).
    chained: std::collections::HashSet<usize>,
}

impl Finder<'_> {
    /// Use the compiler's facts for the call of `ident` at `span`, if there are any. Returns
    /// true when they are complete (the compiler saw through every callee), so the name's guess
    /// is not needed.
    /// `args` are the call's arguments as written, a method's receiver first: an effect the
    /// compiler found on the callee's parameter `j` acts on what `args[j]` names.
    fn fact(&mut self, span: proc_macro2::Span, ident: &str, args: &[&Expr]) -> bool {
        let Some(fact) = crate::facts::at(span, ident) else {
            return false;
        };
        for (e, arg) in fact.effects.iter().zip(&fact.on_arg) {
            let mut e = e.clone();
            if let Some(j) = arg {
                e.target = args.get(*j).map_or(Target::Any, |a| target_of(Some(a)));
            }
            self.found.effects.push(e);
        }
        self.found.panics |= fact.panics;
        !fact.opaque
    }

    /// Like [`Self::fact`], but distinguishes "no fact" (`None`) from "opaque fact"
    /// (`Some(false)`).
    fn fact_full(&mut self, span: proc_macro2::Span, ident: &str, args: &[&Expr]) -> Option<bool> {
        crate::facts::at(span, ident)?;
        Some(self.fact(span, ident, args))
    }

    /// The last identifier of a called path (`stamp` in `helpers::stamp(..)`).
    fn callee_name(c: &syn::ExprCall) -> Option<String> {
        match &*c.func {
            Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        }
    }

    fn push(&mut self, domain: Domain, kind: Kind, target: Target, op: String, ordered: bool) {
        self.found.effects.push(Effect {
            domain,
            kind,
            target,
            op,
            ordered,
        });
    }

    /// `std::fs::write(p, ..)`, `File::create(p)`, `env::set_var(..)`, `Command::new(..)`, and
    /// awaited free functions named by a verb.
    fn path_call(&mut self, c: &syn::ExprCall) -> bool {
        let Expr::Path(p) = &*c.func else {
            return false;
        };
        if let Some(name) = Self::callee_name(c)
            && self.fact(c.func.span(), &name, &c.args.iter().collect::<Vec<_>>())
        {
            return true;
        }
        let segs: Vec<String> = p
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        let Some(last) = segs.last().cloned() else {
            return false;
        };
        let has = |s: &str| segs[..segs.len() - 1].iter().any(|x| x == s);
        let op = segs[segs.len().saturating_sub(2)..].join("::");
        let arg0 = c.args.first();
        if has("fs") || has("File") || has("OpenOptions") || has("DirBuilder") {
            let kind = if FS_READS.contains(&last.as_str()) && !has("OpenOptions") {
                Kind::Read
            } else {
                Kind::Write
            };
            self.push(Domain::Fs, kind, target_of(arg0), op.clone(), false);
            // `rename(from, to)`, `copy(from, to)`, `hard_link(src, dst)` also touch the second.
            if kind == Kind::Write
                && c.args.len() >= 2
                && matches!(
                    last.as_str(),
                    "rename" | "copy" | "hard_link" | "symlink" | "soft_link"
                )
            {
                self.push(
                    Domain::Fs,
                    Kind::Write,
                    target_of(c.args.iter().nth(1)),
                    op,
                    false,
                );
            }
            return true;
        }
        if has("env") {
            let kind = if last.starts_with("set") || last.starts_with("remove") {
                Kind::Write
            } else {
                Kind::Read
            };
            self.push(Domain::Env, kind, target_of(arg0), op, false);
            return true;
        }
        if has("Command") || has("process") {
            self.push(Domain::Process, Kind::Write, target_of(arg0), op, true);
            return true;
        }
        if has("TcpStream")
            || has("UdpSocket")
            || has("TcpListener")
            || has("UnixStream")
            || has("net")
        {
            let kind = if NET_WRITES.contains(&last.as_str()) {
                Kind::Write
            } else {
                Kind::Read
            };
            self.push(Domain::Net, kind, target_of(arg0), op, false);
            return true;
        }
        if has("set_current_dir") || last == "set_current_dir" {
            self.push(Domain::Env, Kind::Write, Target::Any, op, false);
            return true;
        }
        if self.awaited > 0 {
            // An awaited free function or associated function: a service behind any receiver.
            match self.config.verb(&last) {
                Some((kind, ordered)) => {
                    self.push(Domain::Service(None), kind, target_of(arg0), op, ordered)
                }
                None => self.unclassified(op),
            }
            return true;
        }
        false
    }

    fn unclassified(&mut self, op: String) {
        if self.quiet {
            return;
        }
        if self.config.strict {
            self.push(Domain::Unknown, Kind::Write, Target::Any, op.clone(), true);
        }
        self.found.unclassified.push(op);
    }

    /// An awaited method chain `recv.a(..).b(..).c(..)`: the first method with a known verb
    /// decides, so `client.get(url).send()` reads and `s3.put_object().key(k).send()` writes.
    fn awaited_chain(&mut self, base: &Expr) {
        let mut calls = Vec::new();
        let mut e = base;
        loop {
            match e {
                Expr::MethodCall(m) => {
                    calls.push(m);
                    e = &m.receiver;
                }
                Expr::Try(t) => e = &t.expr,
                Expr::Paren(p) => e = &p.expr,
                _ => break,
            }
        }
        // Facts from the compiler for any call of the chain replace the names' guesses.
        // A chain counts as known when at least one call has a fact and none is opaque.
        let mut any = false;
        let mut complete = true;
        let mut note = |found: Option<bool>| {
            if let Some(full) = found {
                any = true;
                complete &= full;
            }
        };
        for m in &calls {
            let args: Vec<&Expr> = std::iter::once(&*m.receiver).chain(&m.args).collect();
            note(self.fact_full(m.method.span(), &m.method.to_string(), &args));
        }
        if let Expr::Call(c) = e
            && let Some(name) = Self::callee_name(c)
        {
            note(self.fact_full(c.func.span(), &name, &c.args.iter().collect::<Vec<_>>()));
        }
        if any && complete {
            return;
        }
        // `e` is the chain's root; a root that is itself a call (`Command::new(..)`,
        // `File::open(..)`, `fetch(x)`) is classified by `visit_expr_call`.
        if let Expr::Call(c) = e
            && matches!(&*c.func, Expr::Path(_))
        {
            let before = self.found.effects.len() + self.found.unclassified.len();
            self.path_call(c);
            let classified = self.found.effects.len() + self.found.unclassified.len() > before;
            if classified {
                return;
            }
        }
        calls.reverse();
        if calls.is_empty() {
            return;
        }
        // `path.exists()`, `dir.join("a").is_file()`: methods of `std::path::Path` that read the
        // file system at the receiver. Another type may have a method of that name, so the
        // service read below is kept as well: both are reads, and an extra one only adds
        // conflicts.
        for m in &calls {
            if m.args.is_empty() && PATH_FS_READS.contains(&m.method.to_string().as_str()) {
                let target = target_of(Some(&m.receiver));
                self.push(Domain::Fs, Kind::Read, target, m.method.to_string(), false);
            }
        }
        let domain = Domain::Service(receiver_path(e));
        for m in &calls {
            let name = m.method.to_string();
            if let Some((kind, ordered)) = self.config.verb(&name) {
                self.push(domain, kind, target_of(m.args.first()), name, ordered);
                return;
            }
        }
        let names: Vec<String> = calls.iter().map(|m| m.method.to_string()).collect();
        self.unclassified(names.join("."));
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_expr_await(&mut self, e: &'ast syn::ExprAwait) {
        self.awaited += 1;
        self.awaited_chain(&e.base);
        // Calls nested in the chain's arguments are visited for their own effects.
        match &*e.base {
            Expr::MethodCall(_) | Expr::Try(_) | Expr::Paren(_) => {
                let mut x = &*e.base;
                loop {
                    match x {
                        Expr::MethodCall(m) => {
                            for a in &m.args {
                                self.visit_expr(a);
                            }
                            x = &m.receiver;
                        }
                        Expr::Try(t) => x = &t.expr,
                        Expr::Paren(p) => x = &p.expr,
                        Expr::Call(c) => {
                            for a in &c.args {
                                self.visit_expr(a);
                            }
                            break;
                        }
                        other => {
                            self.visit_expr(other);
                            break;
                        }
                    }
                }
            }
            Expr::Call(c) => {
                for a in &c.args {
                    self.visit_expr(a);
                }
            }
            other => self.visit_expr(other),
        }
        self.awaited -= 1;
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        if self.sync && self.awaited == 0 {
            // The outermost call of a chain classifies the whole chain, as an await would.
            if self.chained.insert(m as *const _ as usize) {
                let mut r = &*m.receiver;
                loop {
                    match r {
                        Expr::MethodCall(inner) => {
                            self.chained.insert(inner as *const _ as usize);
                            r = &inner.receiver;
                        }
                        Expr::Try(t) => r = &t.expr,
                        Expr::Paren(p) => r = &p.expr,
                        _ => break,
                    }
                }
                let quiet = std::mem::replace(&mut self.quiet, true);
                self.awaited += 1;
                self.awaited_chain(&Expr::MethodCall(m.clone()));
                self.awaited -= 1;
                self.quiet = quiet;
            }
        } else {
            // Synchronous method calls in async code only have effects psync knows of through
            // the compiler.
            let args: Vec<&Expr> = std::iter::once(&*m.receiver).chain(&m.args).collect();
            self.fact(m.method.span(), &m.method.to_string(), &args);
        }
        syn::visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        // Awaited free functions are handled by `awaited_chain`; plain calls only count when they
        // reach a known domain (`std::fs::write`), so `HashMap::new()` stays effect-free. In
        // synchronous code a free function named by a verb is a service call too.
        if self.sync && self.awaited == 0 {
            let quiet = std::mem::replace(&mut self.quiet, true);
            self.awaited = 1;
            self.path_call(c);
            self.awaited = 0;
            self.quiet = quiet;
        } else {
            let saved = std::mem::replace(&mut self.awaited, 0);
            self.path_call(c);
            self.awaited = saved;
        }
        syn::visit::visit_expr_call(self, c);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Macro bodies are not parsed; an await inside one is unclassified.
        if contains_await(m.tokens.clone()) {
            let name = crate::classify::macro_name(m);
            if !matches!(
                name.as_str(),
                "format"
                    | "vec"
                    | "println"
                    | "eprintln"
                    | "debug"
                    | "info"
                    | "warn"
                    | "error"
                    | "trace"
            ) {
                self.unclassified(format!("{name}!"));
            }
        }
    }

    // Items inside the statement are separate code.
    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

fn contains_await(ts: TokenStream) -> bool {
    ts.into_iter().any(|t| match t {
        proc_macro2::TokenTree::Ident(i) => i == "await",
        proc_macro2::TokenTree::Group(g) => contains_await(g.stream()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn effects(stmt: syn::Stmt) -> Vec<Effect> {
        of_stmt(&stmt, &Config::default()).effects
    }

    #[test]
    fn format_templates_with_the_same_holes() {
        let clash = |a: syn::Stmt, b: syn::Stmt| conflicts(&effects(a), &effects(b)).is_some();
        // Same placeholders, then different text: different files.
        assert!(!clash(
            parse_quote!(std::fs::write(format!("{dir}/AppRun"), a)?;),
            parse_quote!(std::fs::write(format!("{dir}/linuxdeploy"), b)?;),
        ));
        // Positional and named arguments name the same placeholder.
        assert!(!clash(
            parse_quote!(std::fs::write(format!("{}/a", self.dir), a)?;),
            parse_quote!(std::fs::write(format!("{d}/b", d = self.dir), b)?;),
        ));
        // Keys of a service: different fields of one user.
        assert!(!clash(
            parse_quote!(kv.set(&format!("user.{id}.name"), n).await?;),
            parse_quote!(kv.set(&format!("user.{id}.email"), e).await?;),
        ));
        // Still conflicts: a different placeholder may hold the same text,
        assert!(clash(
            parse_quote!(std::fs::write(format!("{dir}/a"), a)?;),
            parse_quote!(std::fs::write(format!("{other}/a"), b)?;),
        ));
        // a directory and a file in it,
        assert!(clash(
            parse_quote!(std::fs::remove_dir_all(format!("{dir}/a"))?;),
            parse_quote!(std::fs::write(format!("{dir}/a/b"), b)?;),
        ));
        // the same template,
        assert!(clash(
            parse_quote!(std::fs::write(format!("{dir}/a"), a)?;),
            parse_quote!(std::fs::write(format!("{dir}/a"), b)?;),
        ));
        // a different format spec (`{dir:?}` is not `{dir}`),
        assert!(clash(
            parse_quote!(std::fs::write(format!("{dir:?}/a"), a)?;),
            parse_quote!(std::fs::write(format!("{dir}/b"), b)?;),
        ));
        // a computed placeholder, which may differ between calls,
        assert!(clash(
            parse_quote!(std::fs::write(format!("{}/a", self.base()), a)?;),
            parse_quote!(std::fs::write(format!("{}/b", self.base()), b)?;),
        ));
        // and a placeholder against text it may contain.
        assert!(clash(
            parse_quote!(std::fs::write(format!("{dir}/a"), a)?;),
            parse_quote!(std::fs::write("out/a", b)?;),
        ));
    }

    #[test]
    fn service_verbs_on_receivers() {
        let d = effects(parse_quote!(api.delete(name).await?;));
        assert_eq!(d.len(), 1);
        assert!(d[0].writes() && d[0].domain == Domain::Service(Some("api".into())));
        let c = effects(parse_quote!(api.create(&obj).await?;));
        assert!(conflicts(&d, &c).is_some());
        // Reads commute.
        let g1 = effects(parse_quote!(let a = api.get("a").await?;));
        let g2 = effects(parse_quote!(let b = api.list().await?;));
        assert!(!g1[0].writes() && conflicts(&g1, &g2).is_none());
        // Different receivers are different services.
        let other = effects(parse_quote!(cache.put(k, v).await;));
        assert!(conflicts(&g1, &other).is_none());
    }

    #[test]
    fn first_verb_in_a_chain_decides() {
        let get = effects(parse_quote!(let r = client.get(url).header(a, b).send().await?;));
        assert_eq!(get[0].kind, Kind::Read);
        let put = effects(parse_quote!(s3.put_object().bucket(b).key(k).send().await?;));
        assert_eq!(put[0].kind, Kind::Write);
        let tx = effects(parse_quote!(tx.send(v).await?;));
        assert!(tx[0].writes() && tx[0].ordered);
    }

    #[test]
    fn files_by_path() {
        let w = effects(parse_quote!(tokio::fs::write("cfg.toml", b"x").await?;));
        let r = effects(parse_quote!(let s = tokio::fs::read_to_string("cfg.toml").await?;));
        let other = effects(parse_quote!(let s = tokio::fs::read_to_string("other.toml").await?;));
        let inside = effects(parse_quote!(let s = std::fs::read("out/a.txt")?;));
        let dir = effects(parse_quote!(std::fs::remove_dir_all("out")?;));
        assert!(w[0].domain == Domain::Fs && w[0].writes());
        assert!(conflicts(&w, &r).is_some());
        assert!(conflicts(&w, &other).is_none());
        assert!(conflicts(&dir, &inside).is_some());
        let unknown = effects(parse_quote!(let s = fs::read(&path).await?;));
        assert!(conflicts(&w, &unknown).is_some());
    }

    #[test]
    fn processes_touch_everything() {
        let p = effects(parse_quote!(let out = Command::new("git").arg("pull").output().await?;));
        let r = effects(parse_quote!(let s = tokio::fs::read_to_string("x").await?;));
        assert!(p[0].domain == Domain::Process, "{p:?}");
        assert!(conflicts(&p, &r).is_some());
    }

    #[test]
    fn unclassified_calls() {
        let s: syn::Stmt = parse_quote!(let r = client.request(req).await?;);
        let lax = of_stmt(&s, &Config::default());
        assert!(lax.effects.is_empty() && lax.unclassified == ["request"]);
        let strict = of_stmt(
            &s,
            &Config {
                strict: true,
                ..Default::default()
            },
        );
        assert!(strict.effects[0].domain == Domain::Unknown);
        // Plain constructors are not effects at all.
        assert!(effects(parse_quote!(let m = HashMap::new();)).is_empty());
    }

    #[test]
    fn format_prefixes_separate_keys() {
        let a = effects(parse_quote!(client.set_account_data("m.megolm_backup.v1", "{}").await?;));
        let b = effects(
            parse_quote!(client.set_account_data(&format!("m.secret_storage.key.{id}"), "{}").await?;),
        );
        let c =
            effects(parse_quote!(client.set_account_data("m.secret_storage.key.k1", "{}").await?;));
        assert!(conflicts(&a, &b).is_none());
        assert!(conflicts(&b, &c).is_some());
    }

    #[test]
    fn user_verbs() {
        let config = Config {
            writes: vec!["bump".into()],
            ..Default::default()
        };
        let s: syn::Stmt = parse_quote!(counter.bump_hits(1).await;);
        assert!(of_stmt(&s, &config).effects[0].writes());
    }
}
