#!/usr/bin/env python3
"""Tie psync's generated certificates for the driver fixture's synchronous functions to the
compiled code: Charon's LLBC of `stash_both`, `stash_same` and `stash_tail` (c28, c29, c32),
translated into the MIR fragment of verify/Footprint.lean, one statement per certificate node.

    cd scripts/charon/fixture && charon cargo --preset=aeneas \\
        --start-from fixture::app::stash_both --start-from fixture::app::stash_same \\
        --start-from fixture::app::stash_tail                       # writes fixture.llbc
    psync-cert --facts verify/facts.tsv --index index.json FILE... > verify/Certs.lean
    psync-cert --index index-tokens.json FILE... > certs-tokens.lean   # without the facts
    fixture2lean.py fixture.llbc index.json verify/Certs.lean certs-tokens.lean c28 c29 c32 \
        > verify/CharonFixture.lean

Lean then decides `hindB` for each certificate as `psync-cert` wrote it (`PsyncCerts.cN`) against
the compiled statements of its nodes, found by the source lines in the index. For c29 it also
decides it for the certificate psync wrote from the tokens alone, before the driver's facts.

It extends `llbc2lean.py` with models of what the fragment has no body for (stated in the
output):
- a string constant is a place of its own, `STR.k`, one per distinct text `k`;
- `fs::write(p, d)` writes the file named by `p`, the place `*p` extended by `.0`, and reads `*p`
  and `*d`;
- `AsRef::as_ref(&x)` returns a reference to what `x` refers to (identity), and a by-value
  parameter of a generic `impl AsRef<Path>` is a reference to its file name;
- `Result::and`, `Try::branch` and `FromResidual::from_residual` move their arguments' values;
- drops of the fixture's values (`Result`, `&str`) do nothing.
Anything else stops the script.
"""
import json, sys

import llbc2lean as L

STR = 997


class Fn(L.Fn):
    def __init__(self, decl, ids, ctx):
        super().__init__(decl, ids)
        self.ctx = ctx

    def literal(self, gid):
        """The text of the string constant global `gid` (Charon records its value)."""
        v = self.ctx["globals"][gid].get("value") or {}
        if "Deduplicated" in v:
            v = {"Value": [v["Deduplicated"], self.ctx["values"][v["Deduplicated"]]]}
        if "Value" in v and isinstance(v["Value"][1][0], dict) and "Str" in v["Value"][1][0]:
            return v["Value"][1][0]["Str"]
        raise L.Unsupported(f"global {gid} is not a string constant")

    def str_place(self, s):
        k = self.ctx["strings"].setdefault(s, len(self.ctx["strings"]))
        return (STR, [k])

    def is_ref(self, k):
        """Charon hash-conses types: resolve the local's type before looking at it."""
        ty = self.locals[k]["ty"]
        if "Deduplicated" in ty:
            ty = self.ctx["types"].get(ty["Deduplicated"], {})
        elif "Value" in ty:
            ty = ty["Value"][1]
        return isinstance(ty, dict) and "Ref" in ty

    def is_ref_local(self, l):
        return l in self.refs or (1 <= l <= self.arg_count and self.is_ref(l))

    def target(self, l):
        """What reference local `l` points to."""
        if l in self.refs:
            return self.refs[l]
        if 1 <= l <= self.arg_count:
            return (l, [])
        raise L.Unsupported(f"local {l} is not a known reference")

    def ref_operand(self, o):
        (_, p), = [(k, v) for k, v in o.items() if k in ("Copy", "Move")]
        p = p[0] if isinstance(p, list) else p
        k = p["kind"]
        if "Local" not in k or not self.is_ref_local(k["Local"]):
            raise L.Unsupported(f"expected a reference, got {json.dumps(p)[:80]}")
        return self.target(k["Local"])

    def statement(self, kind):
        if isinstance(kind, dict) and ("Drop" in kind or "Borrowck" in kind):
            return None
        if isinstance(kind, dict) and "Assign" in kind:
            dst, rv = kind["Assign"]
            d = dst["kind"]
            if "Ref" in rv and "Global" in rv["Ref"]["place"]["kind"]:
                self.refs[d["Local"]] = self.str_place(self.literal(rv["Ref"]["place"]["kind"]["Global"]["id"]))
                return None
            if "Use" in rv and "Local" in d and self.is_ref(d["Local"]):
                self.refs[d["Local"]] = self.ref_operand(rv["Use"][0])
                return None
        if isinstance(kind, dict) and "Call" in kind:
            call = kind["Call"]
            func = call["call"]["func"]["Regular"]["kind"]
            args = call["call"]["args"]
            dest = call["dest"]
            if "Trait" in func:
                trait = self.ctx["trait_name"](func["Trait"][0])
                if trait == "AsRef" and len(args) == 1:
                    self.refs[dest["kind"]["Local"]] = self.ref_operand(args[0])
                    return None
                raise L.Unsupported(f"call of trait {trait}")
            target = self.ids["by_id"][func["Fun"]]
            path = [list(x.values())[0][0] for x in target["item_meta"]["name"] if "Ident" in x]
            name = path[-1]
            if path[:2] == ["std", "fs"] and name == "write":
                p, data = self.ref_operand(args[0]), self.ref_operand(args[1])
                file = (p[0], p[1] + [0])
                return ("assign", file, "(fun a _ => a)", ("place", data), ("place", p))
            if path[0] == "core" and name in ("and", "branch", "from_residual"):
                ops = [self.operand(a) for a in args] + [("const", 0)]
                return ("assign", self.place(dest), "(fun a _ => a)", ops[0], ops[1])
            if func["Fun"] in self.ids["local"]:
                sigma = {0: self.place(dest)}
                for j, a in enumerate(args, start=1):
                    sigma[j] = self.ref_operand(a)
                self.sites += 1
                return ("call", self.ids["local"][func["Fun"]], sigma, self.sites)
            raise L.Unsupported(f"call of body-less {'::'.join(path)}")
        return super().statement(kind)

    def scrutinee(self, sw):
        c = sw["data"]["scrutinee"]
        if "Discriminant" in c:  # `match` on an enum: the place holding it
            return self.place(c["Discriminant"])
        o = self.operand(c["Value"])
        if o[0] != "place":
            raise L.Unsupported("constant condition")
        return o[1]

    def translate(self, stmts):
        """As `llbc2lean.Fn.translate`, also for a `match` on an enum's discriminant."""
        out = []
        for i, s in enumerate(stmts):
            kind = s["kind"]
            if kind == "Return":
                return L.seq(out), True
            if isinstance(kind, dict) and "Switch" in kind:
                sw = kind["Switch"]
                c = self.scrutinee(sw)
                rest = stmts[i + 1:]
                arms = []
                for b in sw["branches"]:
                    t, returned = self.translate(self.block_stmts(b))
                    if not returned:
                        t, returned = self.translate(self.block_stmts(b) + rest)
                    arms.append(t)
                if len(arms) != 2:
                    raise L.Unsupported(f"match with {len(arms)} arms")
                out.append(("ite", c, arms[0], arms[1]))
                return L.seq(out), True
            st = self.statement(kind)
            if st is not None:
                out.append(st)
        return L.seq(out), False

    def node(self, stmts, lines, line_of):
        """The statements of one certificate node: those on its source lines. A `match` that
        ends it (`r1?`) keeps its own arms, not the rest of the block."""
        out = []
        for s in stmts:
            if not (lines[0] <= line_of(s) <= lines[1]):
                continue
            kind = s["kind"]
            if isinstance(kind, dict) and "Switch" in kind:
                sw = kind["Switch"]
                c = self.scrutinee(sw)
                arms = [self.translate(self.block_stmts(b))[0] for b in sw["branches"]]
                out.append(("ite", c, arms[0], arms[1]))
                continue
            st = self.statement(kind)
            if st is not None:
                out.append(st)
        return L.seq(out)


def main():
    llbc, index_path, certs_path, tokens_path, *ids_wanted = sys.argv[1:]
    certs_text = open(certs_path).read()
    raw = json.load(open(llbc))["translated"]
    L.collect_dedup(raw)
    spans = {}

    def collect_spans(x):
        if isinstance(x, dict):
            v = x.get("Value")
            if len(x) == 1 and isinstance(v, list) and len(v) == 2 and isinstance(v[1], dict) and "data" in v[1] \
                    and "beg" in v[1]["data"]:
                spans.setdefault(v[0], v[1])
            for y in x.values():
                collect_spans(y)
        elif isinstance(x, list):
            for y in x:
                collect_spans(y)

    collect_spans(raw)
    values = {}

    def collect_values(x):
        if isinstance(x, dict):
            v = x.get("Value")
            if len(x) == 1 and isinstance(v, list) and len(v) == 2 and isinstance(v[1], list) and v[1] \
                    and isinstance(v[1][0], dict) and "Str" in v[1][0]:
                values.setdefault(v[0], v[1])
            for y in x.values():
                collect_values(y)
        elif isinstance(x, list):
            for y in x:
                collect_values(y)

    collect_values(raw)
    types = {}

    def collect_types(x):
        if isinstance(x, dict):
            t = x.get("ty")
            if isinstance(t, dict) and isinstance(t.get("Value"), list) and len(t["Value"]) == 2:
                types.setdefault(t["Value"][0], t["Value"][1])
            for y in x.values():
                collect_types(y)
        elif isinstance(x, list):
            for y in x:
                collect_types(y)

    collect_types(raw)

    def line_of(s):
        sp = s["span"]
        if "Deduplicated" in sp:
            sp = spans[sp["Deduplicated"]]
        elif "Value" in sp:
            sp = sp["Value"][1]
        return sp["data"]["beg"]["line"]

    crate = raw["crate_name"]
    by_id = {f["def_id"]: f for f in raw["fun_decls"] if f}
    local = [f for f in raw["fun_decls"]
             if f and f["item_meta"]["name"][0] == {"Ident": [crate, 0]} and "Structured" in (f.get("body") or {})]
    ids = {"by_id": by_id, "local": {f["def_id"]: i for i, f in enumerate(local)}}
    trait_decls = {t["def_id"]: t for t in raw["trait_decls"] if t}

    def trait_name(r):
        if "Deduplicated" in r:
            r = TRAIT_REFS[r["Deduplicated"]]
        else:
            TRAIT_REFS[r["Value"][0]] = r
        tid = r["Value"][1]["trait_decl_ref"]["id"]
        return trait_decls[tid]["item_meta"]["name"][-1]["Ident"][0]

    ctx = {"globals": {g["def_id"]: g for g in raw["global_decls"] if g},
           "files": {f["id"]: f.get("contents", "") for f in raw["files"]},
           "strings": {}, "trait_name": trait_name, "values": values, "types": types}
    fns = [Fn(f, ids, ctx) for f in local]
    bodies = []
    for fn in fns:
        try:
            body, _ = fn.translate(fn.block_stmts(fn.block))
        except L.Unsupported as e:
            sys.exit(f"{L.name_of(fn.decl)}: unsupported: {e}")
        bodies.append(body)
    S = L.summaries(bodies)
    names = [L.name_of(f) for f in local]
    index = {c["id"]: c for c in json.load(open(index_path))}
    tokens_text = open(tokens_path).read()

    print(f"""import Certs
import Footprint

/-!
# The driver fixture's synchronous certificates, against compiled code

Generated by `scripts/charon/fixture2lean.py` from Charon's LLBC of `psync-check`'s fixture
(`scripts/charon/fixture`, the fixture compiled as written: `#[psync::graph]` replaced by a macro
that only removes psync's statement attributes). Do not edit. Functions: {', '.join(f'{i} = `{n}`' for i, n in enumerate(names))}.

Models of what has no body (the script's, and so assumed): a string constant is a place of its
own (`{STR}.k`, one per text: {', '.join(f'{k} = {s!r}' for s, k in ctx['strings'].items()) or 'none'});
`fs::write(p, d)` writes the file `*p.0` and reads `*p` and `*d`; `AsRef::as_ref` is the identity
on the reference; `Result::and`, `Try::branch` and `from_residual` move values; drops do nothing.

For each certificate, `hindB` is decided against the compiled statements of its nodes (by source
line): every pair of nodes that the certificate, as `psync-cert` wrote it, calls independent has
disjoint real footprints. With the link check and `check`, `cert_schedule_sound` gives the
theorem for the compiled code.
-/

namespace PsyncCharon.Fixture

open PsyncFootprint PsyncCert PsyncWorld PsyncCerts
""")
    for i, (n, b) in enumerate(zip(names, bodies)):
        print(f"/-- The body of `{n}`, from LLBC. -/\ndef body{i} : Stmt :=\n  {L.lstmt(b)}\n")
        print(f"/-- The summary of `{n}`. -/\ndef sum{i} : Fp := {L.lfp(S[i])}\n")
    print("def prog : Program := ⟨[" + ", ".join(f"(body{i}, sum{i})" for i in range(len(bodies))) + "]⟩\n")
    print("/-- **The link check passes** on the compiled fixture. -/\ntheorem prog_valid : prog.validB = true := by decide\n")
    for cid in ids_wanted:
        c = index[cid]
        k = names.index(c["function"])
        fn = fns[k]
        fn.refs, fn.sites = {}, 100  # call sites of node statements: fresh frames
        stmts = fn.block_stmts(fn.block)
        # References are set up by statements outside the nodes too (argument constants).
        for s in stmts:
            try:
                fn.statement(s["kind"]) if isinstance(s["kind"], dict) and "Assign" in s["kind"] and "Ref" in s["kind"]["Assign"][1] else None
            except L.Unsupported:
                pass
        nodes = [fn.node(stmts, lines, line_of) for lines in c["lines"]]
        print(f"/-- The compiled statements of `{c['function']}`'s certificate nodes (lines {c['lines']}). -/")
        print(f"def st_{cid} : List Stmt := [\n  " + ",\n  ".join(L.lstmt(s, '  ') for s in nodes) + "]\n")
        print(f"""/-- $$\\mathrm{{hindB}}(\\mathtt{{{cid}}}, \\mathrm{{st}}) = \\mathsf{{true}}$$

Every pair of nodes that `{cid}` calls independent has disjoint footprints in the compiled code. -/
theorem {cid}_hindB : hindB prog.sum {cid} st_{cid} = true := by decide
""")
        if f"theorem {cid}_ok : check {cid} = true" in certs_text:
            print(f"""/-- $$\\mathrm{{run}}(\\mathrm{{schedule}}) = \\mathrm{{run}}(\\mathrm{{st}}_0; \\dots)$$

**psync's schedule for `{c['function']}`, as compiled, equals its statements in order**, for every
fuel and every memory: the link check, `{cid}_hindB` and `{cid}_ok` (`check`, in `Certs.lean`). -/
theorem {cid}_sound (n : Nat) {{ms : List (List Stmt)}}
    (hx : Execs ({cid}.levels.map (List.map (List.map (stmtAt st_{cid})))) ms) (m : Mem) :
    (memSys (prog.valid prog_valid) n).runAll ms.flatten m =
      (memSys (prog.valid prog_valid) n).runAll ((List.range {cid}.nodes.length).map (stmtAt st_{cid})) m :=
  cert_schedule_sound (prog.valid prog_valid) n {cid} st_{cid} {cid}_hindB {cid}_ok hx m
""")
        if cid == "c29":
            # The certificate psync wrote from the tokens alone.
            start = tokens_text.index(f"def {cid} : Cert where")
            end = tokens_text.index("\n\n", start)
            print("/-- `c29` as `psync-cert` wrote it from the tokens alone, before the driver's facts: it\nnames no file. -/")
            print(tokens_text[start:end].replace(f"def {cid} :", f"def {cid}_tokens :") + "\n")
            print(f"""/-- $$\\mathrm{{hindB}}(\\mathtt{{{cid}\\_tokens}}, \\mathrm{{st}}) = \\mathsf{{false}}$$

**Refuted**: that certificate calls the two `stash("a.json", ..)` calls independent, and both
write the file `a.json`. -/
theorem {cid}_tokens_refuted : hindB prog.sum {cid}_tokens st_{cid} = false := by decide
""")
    print("end PsyncCharon.Fixture")


TRAIT_REFS = {}

if __name__ == "__main__":
    main()
