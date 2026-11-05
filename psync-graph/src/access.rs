//! Token-level read/write analysis.
//!
//! Without type information, the analysis approximates what a statement touches from its tokens:
//! every identifier it mentions, and every *access path* (`x`, `self.groups`, `conn.inner`) it
//! reads or writes. A path is written by assignment, `&mut path`, a mutating method call on a
//! `mut` binding or `&mut` handle, or an order-sensitive call (`send`, `write`, `recv`, ...) on any
//! receiver. Two statements conflict when one writes a path that overlaps a path the other touches.

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use std::collections::BTreeSet;

/// A set of identifier names or dotted access paths.
pub(crate) type Set = BTreeSet<String>;

/// What one statement touches.
#[derive(Default, Clone, Debug)]
pub(crate) struct Access {
    /// Every variable-like identifier mentioned (drives def-use edges).
    pub uses: Set,
    /// Root identifiers of written paths.
    pub writes: Set,
    /// Access paths read, such as `self.client`.
    pub rpaths: Set,
    /// Access paths written, such as `self.groups`.
    pub wpaths: Set,
}

/// Keywords and names that never denote a variable.
const KEYWORDS: &[&str] = &[
    "let", "mut", "await", "match", "if", "else", "fn", "move", "async", "as", "in", "for",
    "while", "loop", "return", "break", "continue", "true", "false", "ref", "where", "impl", "dyn",
    "unsafe", "struct", "enum", "type", "use", "crate", "super", "Self",
];

/// Methods assumed not to mutate their receiver.
pub(crate) const PURE_METHODS: &[&str] = &[
    "clone",
    "to_string",
    "to_owned",
    "into",
    "as_ref",
    "as_str",
    "as_slice",
    "len",
    "is_empty",
    "unwrap_or_default",
    "as_deref",
    "iter",
    "to_vec",
    "copied",
    "cloned",
    "is_some",
    "is_none",
];

/// Prefixes of further read-only method names (`is_*`, `get*`, `as_*`, ...).
const READ_PREFIXES: &[&str] = &[
    "is_",
    "has_",
    "get",
    "as_",
    "to_",
    "contains",
    "len",
    "iter",
    "keys",
    "values",
    "first",
    "last",
    "count",
    "unwrap",
    "expect",
    "starts_with",
    "ends_with",
    "clone",
    "map",
    "ok",
    "err",
    "list",
    "fetch",
    "find",
    "load",
    "query",
    "lookup",
    "describe",
    "stat",
    "head",
    "exists",
    "search",
    "resolve",
    "download",
    "retrieve",
];

/// Calls whose relative order is observable even through `&self`: channel, stream and I/O
/// operations on one receiver must stay in program order.
const ORDERED_METHODS: &[&str] = &[
    "send",
    "send_all",
    "try_send",
    "recv",
    "try_recv",
    "next",
    "try_next",
    "feed",
    "write",
    "write_all",
    "flush",
    "read_exact",
    "read_to_end",
    "read_to_string",
    "shutdown",
    "close",
    "execute",
    "commit",
    "rollback",
];

/// True when `method` is assumed not to mutate its receiver.
fn reads_only(method: &str) -> bool {
    PURE_METHODS.contains(&method) || READ_PREFIXES.iter().any(|p| method.starts_with(p))
}

/// True when two access paths overlap: equal, or one is a field prefix of the other.
pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long == short || long.starts_with(short) && long[short.len()..].starts_with('.')
}

/// The first path in `writes` that overlaps a path in `other`.
pub(crate) fn conflict(writes: &Set, other: &Set) -> Option<String> {
    writes
        .iter()
        .find(|w| other.iter().any(|o| overlaps(w, o)))
        .cloned()
}

/// The identifier a dotted path starts from.
pub(crate) fn root(path: &str) -> &str {
    path.split('.').next().unwrap_or_default()
}

fn is_punct(tt: Option<&TokenTree>, c: char) -> bool {
    matches!(tt, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

/// Record what `tokens` touch into `acc`.
///
/// `mutable` holds `mut` bindings and `handles` holds bindings to `&mut` values (including
/// `&mut self`): method calls on them are writes unless the method looks read-only, and a handle
/// passed along whole may be used for anything.
pub(crate) fn scan(tokens: TokenStream, mutable: &Set, handles: &Set, acc: &mut Access) {
    let tts: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        match tt {
            // `#[attr(..)]` contents are not code.
            TokenTree::Group(g)
                if g.delimiter() == Delimiter::Bracket
                    && i > 0
                    && is_punct(tts.get(i - 1), '#') => {}
            TokenTree::Group(g) => scan(g.stream(), mutable, handles, acc),
            // `format!("{name}")` captures `name` implicitly.
            TokenTree::Literal(lit) => {
                for name in format_captures(&lit.to_string()) {
                    acc.rpaths.insert(name.clone());
                    acc.uses.insert(name);
                }
            }
            TokenTree::Ident(id) => {
                let name = id.to_string();
                // `.field` and `.method` are not variables; `..x` (range, struct update) is.
                let after_dot = i > 0
                    && is_punct(tts.get(i - 1), '.')
                    && !(i > 1 && is_punct(tts.get(i - 2), '.'));
                // `Type::item` and `module::item` name items, not variables.
                let path_prefix = is_punct(tts.get(i + 1), ':') && is_punct(tts.get(i + 2), ':');
                let lifetime = i > 0 && is_punct(tts.get(i - 1), '\'');
                if after_dot || path_prefix || lifetime || KEYWORDS.contains(&name.as_str()) {
                    continue;
                }
                acc.uses.insert(name.clone());

                // Follow `.field` and `[index]` to the place being accessed.
                let mut path = name.clone();
                let mut j = i + 1;
                loop {
                    match tts.get(j) {
                        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => j += 1,
                        Some(TokenTree::Punct(p))
                            if p.as_char() == '.' && !is_punct(tts.get(j + 1), '.') =>
                        {
                            let Some(TokenTree::Ident(field)) = tts.get(j + 1) else {
                                break;
                            };
                            let call = matches!(tts.get(j + 2), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
                                || is_punct(tts.get(j + 2), ':');
                            if call || field == "await" {
                                break;
                            }
                            path = format!("{path}.{field}");
                            j += 2;
                        }
                        _ => break,
                    }
                }
                let method = match (tts.get(j), tts.get(j + 1)) {
                    (Some(TokenTree::Punct(p)), Some(TokenTree::Ident(m)))
                        if p.as_char() == '.' && m != "await" =>
                    {
                        Some(m.to_string())
                    }
                    _ => None,
                };

                // Write forms: `&mut path`, `path = ..` / `path += ..`, mutating method calls.
                let amp_mut = i >= 2
                    && matches!(&tts[i - 1], TokenTree::Ident(m) if m == "mut")
                    && is_punct(tts.get(i - 2), '&');
                let mut op = String::new();
                let mut k = j;
                while let Some(TokenTree::Punct(p)) = tts.get(k) {
                    op.push(p.as_char());
                    k += 1;
                    if p.spacing() == Spacing::Alone {
                        break;
                    }
                }
                // `let x = ..` and `let mut x = ..` introduce a binding; they do not write one.
                let binding = (i >= 1 && matches!(&tts[i - 1], TokenTree::Ident(k) if k == "let"))
                    || (i >= 2
                        && matches!(&tts[i - 1], TokenTree::Ident(m) if m == "mut")
                        && matches!(&tts[i - 2], TokenTree::Ident(k) if k == "let"));
                let assign = !binding
                    && method.is_none()
                    && op.ends_with('=')
                    && !matches!(op.as_str(), "==" | "<=" | ">=" | "!=" | "=>");
                let mut_root = mutable.contains(&name) || handles.contains(&name);
                let mutating_call = method
                    .as_deref()
                    .is_some_and(|m| ORDERED_METHODS.contains(&m) || (mut_root && !reads_only(m)));
                let handle_passed = handles.contains(&name) && path == name && method.is_none();

                // `&mut x.f()` borrows the call's result, not `x`.
                let amp_mut = amp_mut && method.is_none();
                acc.rpaths.insert(path.clone());
                if amp_mut || assign || mutating_call || handle_passed {
                    acc.wpaths.insert(path);
                    acc.writes.insert(name);
                }
            }
            _ => {}
        }
    }
}

/// Names captured by `{name}` / `{name:spec}` in a string literal (format-string arguments).
fn format_captures(literal: &str) -> Vec<String> {
    if !literal.ends_with('"') {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rest = literal;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        if rest.starts_with('{') {
            rest = &rest[1..];
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && rest[name.len()..].starts_with(['}', ':'])
        {
            out.push(name);
        }
    }
    out
}

/// Identifiers mentioned by `tokens`, ignoring mutability.
pub(crate) fn mentions(tokens: TokenStream) -> Set {
    let mut acc = Access::default();
    scan(tokens, &Set::new(), &Set::new(), &mut acc);
    acc.uses
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn access(tokens: TokenStream, mutable: &[&str]) -> Access {
        let mutable = mutable.iter().map(|s| s.to_string()).collect();
        let mut acc = Access::default();
        scan(tokens, &mutable, &Set::new(), &mut acc);
        acc
    }

    #[test]
    fn fields_are_tracked_separately() {
        let acc = access(
            quote!(self.groups.insert(k, query(&self.client, g).await)),
            &["self"],
        );
        assert!(acc.wpaths.contains("self.groups"));
        assert!(acc.rpaths.contains("self.client"));
        assert!(conflict(&acc.wpaths, &["self.client".to_string()].into()).is_none());
        assert!(overlaps("self", "self.groups") && !overlaps("self.group", "self.groups"));
    }

    #[test]
    fn format_strings_capture_names() {
        assert_eq!(format_captures(r#""{a} {b:?} {{c}} {0} {}""#), ["a", "b"]);
        assert!(access(quote!(format!("{file}")), &[]).uses.contains("file"));
    }

    #[test]
    fn assignment_and_ordered_calls_write() {
        let acc = access(
            quote!({
                total += x;
                tx.send(v).await;
            }),
            &[],
        );
        assert!(acc.wpaths.contains("total") && acc.wpaths.contains("tx"));
        assert!(!acc.wpaths.contains("x"));
        assert!(access(quote!(a == b), &[]).wpaths.is_empty());
    }
}
