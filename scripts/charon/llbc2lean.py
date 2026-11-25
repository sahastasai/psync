#!/usr/bin/env python3
"""Translate Charon's LLBC (structured MIR) of a crate into the MIR fragment of verify/Footprint.lean.

    charon cargo --preset=aeneas          # in the crate: writes <crate>.llbc
    llbc2lean.py drain.llbc callers > verify/CharonDrain.lean

For every function of the crate with a body, it emits a `Stmt`, computes its summary as the least
fixed point of `fp` (as psync-driver does), and writes a `Program`. For the function named on the
command line, it takes the calls among its top-level statements as the statements of a schedule
that runs them all in one level, and writes a certificate whose nodes are their footprints. Lean
then decides, independently of this script, that the summaries pass the link check, that the
certificate covers the footprints and that `check` accepts it, and `mir_schedule_sound` applies.

The translation follows the borrow checker's naming of places:
- a reference local `_4 = &mut (*s).a` (and reborrows `_3 = &mut *_4`) is resolved to the place it
  borrows, so `*_3` is `(*s).a`;
- a parameter `x: &mut T` is a place of the callee, `(x, [])`; at a call, `σ` maps it to the place
  the argument borrows, the callee's `_0` to the call's destination, and its other locals to fresh
  places of the caller;
- `if` with an early `return` in one branch takes the rest of the block into the other branch.
Values are integers: the footprint theorems do not depend on them. Unsupported (an error): by-value
parameters of the crate's functions, loops, `dyn` and function pointers, calls of body-less
functions other than arithmetic, and unwinding, which the fragment does not model.
"""
import json, sys

PURE_INTRINSICS = {"wrapping_add": "(· + ·)", "wrapping_sub": "(· - ·)", "wrapping_mul": "(· * ·)"}
BINOPS = {"Add": "(· + ·)", "Sub": "(· - ·)", "Mul": "(· * ·)",
          "Eq": "(fun a b => if a = b then 1 else 0)", "Ne": "(fun a b => if a ≠ b then 1 else 0)",
          "Lt": "(fun a b => if a < b then 1 else 0)", "Le": "(fun a b => if a ≤ b then 1 else 0)",
          "Gt": "(fun a b => if a > b then 1 else 0)", "Ge": "(fun a b => if a ≥ b then 1 else 0)"}
# A callee's own locals at call site k live in the caller's frame region: local j at FRAME.k.j,
# and the callee's frame region at FRAME.k. Summaries widen every frame place to FRAME, which
# covers them all (covering is by prefix), so summaries of recursive functions stay finite.
FRAME = 999


class Unsupported(Exception):
    pass


DEDUP = {}


def collect_dedup(x):
    """Charon hash-conses values: `{"Value": [id, x]}` the first time, `{"Deduplicated": id}`
    after. Record every first occurrence (types may be recursive, so nothing is expanded)."""
    if isinstance(x, dict):
        v = x.get("Value")
        # Ids are per kind of value; only constants are needed here.
        if (len(x) == 1 and isinstance(v, list) and len(v) == 2 and isinstance(v[0], int)
                and isinstance(v[1], list) and v[1] and isinstance(v[1][0], dict)
                and set(v[1][0]) & {"Integer", "Bool"}):
            DEDUP.setdefault(v[0], v[1])
        for y in x.values():
            collect_dedup(y)
    elif isinstance(x, list):
        for y in x:
            collect_dedup(y)


def name_of(f):
    return f["item_meta"]["name"][-1]["Ident"][0]


def const_value(c):
    if "Deduplicated" in c:
        c = {"Value": [c["Deduplicated"], DEDUP[c["Deduplicated"]]]}
    v = c["Value"][1][0] if "Value" in c else None
    if v and "Integer" in v:
        return int(list(v["Integer"].values())[0][1])
    if v and "Bool" in v:
        return int(v["Bool"])
    raise Unsupported(f"constant {json.dumps(c)[:80]}")


class Fn:
    def __init__(self, decl, ids):
        self.decl, self.ids = decl, ids
        body = decl["body"]["Structured"]
        self.arg_count = body["locals"]["arg_count"]
        self.locals = body["locals"]["locals"]
        self.refs = {}  # reference local -> place it borrows
        self.sites = 0
        self.block = body["body"]

    def is_ref(self, k):
        ty = json.dumps(self.locals[k]["ty"])
        return '"Ref"' in ty

    def place(self, p):
        k = p["kind"]
        if "Local" in k:
            return (k["Local"], [])
        base, proj = k["Projection"]
        if proj == "Deref":
            b = base["kind"]
            if "Local" in b:
                l = b["Local"]
                if l in self.refs:
                    return self.refs[l]
                if 1 <= l <= self.arg_count:
                    return (l, [])  # a reference parameter: the callee's own place
            raise Unsupported(f"deref of {json.dumps(b)[:80]}")
        if "Field" in proj:
            n, path = self.place(base)
            return (n, path + [proj["Field"][1]])
        raise Unsupported(f"projection {proj}")

    def operand(self, o):
        if "Const" in o:
            return ("const", const_value(o["Const"]))
        (_, p), = [(k, v) for k, v in o.items() if k in ("Copy", "Move")]
        p = p[0] if isinstance(p, list) else p
        return ("place", self.place(p))

    def block_stmts(self, block):
        return block["statements"]

    def translate(self, stmts):
        """A list of LLBC statements -> a Stmt (as a tree); handles early returns."""
        out = []
        for i, s in enumerate(stmts):
            kind = s["kind"]
            if kind == "Return":
                return seq(out), True
            if isinstance(kind, dict) and "Switch" in kind:
                sw = kind["Switch"]
                c = self.operand(sw["data"]["scrutinee"]["Value"])
                if c[0] != "place":
                    raise Unsupported("constant condition")
                branches = sw["branches"]
                # `if c {b0} else {b1}`: the `false` value selects branch 1.
                rest = stmts[i + 1:]
                arms = []
                for b in branches:
                    t, returned = self.translate(self.block_stmts(b))
                    if not returned:
                        t, returned = self.translate(self.block_stmts(b) + rest)
                    arms.append(t)
                out.append(("ite", c[1], arms[0], arms[1]))
                return seq(out), True
            st = self.statement(kind)
            if st is not None:
                out.append(st)
        return seq(out), False

    def statement(self, kind):
        if isinstance(kind, str) or "StorageLive" in kind or "StorageDead" in kind:
            return None
        if "Assign" in kind:
            dst, rv = kind["Assign"]
            if "Aggregate" in rv and rv["Aggregate"][1] == []:
                return None  # `_0 = ()`
            if "Ref" in rv:
                d = dst["kind"]
                if "Local" not in d:
                    raise Unsupported("reference stored through a projection")
                self.refs[d["Local"]] = self.place(rv["Ref"]["place"])
                return None
            d = self.place(dst)
            if "Use" in rv:
                return ("assign", d, "(fun a _ => a)", self.operand(rv["Use"][0]), ("const", 0))
            if "BinaryOp" in rv:
                op, a, b = rv["BinaryOp"]
                op = op if isinstance(op, str) else list(op)[0]
                if op not in BINOPS:
                    raise Unsupported(f"binary operator {op}")
                return ("assign", d, BINOPS[op], self.operand(a), self.operand(b))
            raise Unsupported(f"rvalue {list(rv)}")
        if "Call" in kind:
            call = kind["Call"]
            func = call["call"]["func"]["Regular"]["kind"]
            if "Fun" not in func:
                raise Unsupported("call of a trait method or function pointer")
            callee = func["Fun"]
            args = [self.operand(a) for a in call["call"]["args"]]
            dest = self.place(call["dest"])
            target = self.ids["by_id"][callee]
            if callee not in self.ids["local"]:
                n = name_of(target)
                if n in PURE_INTRINSICS and len(args) == 2:
                    return ("assign", dest, PURE_INTRINSICS[n], args[0], args[1])
                raise Unsupported(f"call of body-less {n}")
            sigma = {0: dest}
            for j, a in enumerate(args, start=1):
                if a[0] != "place" or a[1][1] or a[1][0] not in self.refs:
                    raise Unsupported("by-value argument to the crate's function")
                sigma[j] = self.refs[a[1][0]]  # a reference: pass the place it borrows
            self.sites += 1
            return ("call", self.ids["local"][callee], sigma, self.sites)
        raise Unsupported(f"statement {list(kind)}")


def seq(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return ("nop",)
    t = xs[-1]
    for x in reversed(xs[:-1]):
        t = ("seq", x, t)
    return t


# --- footprints, as `fp` in Footprint.lean -------------------------------------------------

def frame(site, n):
    return (FRAME, [site]) if n == FRAME else (FRAME, [site, n])


def subst(sigma, site, p):
    n, path = p
    base = sigma.get(n, frame(site, n))
    return (base[0], base[1] + path)


def fp(S, s):
    tag = s[0]
    if tag == "nop":
        return [], []
    if tag == "assign":
        _, d, _, a, b = s
        return [x[1] for x in (a, b) if x[0] == "place"], [d]
    if tag == "seq":
        r1, w1 = fp(S, s[1]); r2, w2 = fp(S, s[2])
        return r1 + r2, w1 + w2
    if tag == "ite":
        r1, w1 = fp(S, s[2]); r2, w2 = fp(S, s[3])
        return [s[1]] + r1 + r2, w1 + w2
    if tag == "call":
        _, f, sigma, site = s
        r, w = S[f]
        return [subst(sigma, site, p) for p in r], [subst(sigma, site, p) for p in w]
    raise ValueError(tag)


def pre(p, q):
    return p[0] == q[0] and q[1][:len(p[1])] == p[1]


def normalize(ps, widen=False):
    """Keep the places not contained in another one, deduplicated (a summary covers what it
    contains). `widen`: frame places become FRAME."""
    ps = sorted({(p[0], () if widen and p[0] == FRAME else tuple(p[1])) for p in ps})
    ps = [(n, list(t)) for n, t in ps]
    return [p for p in ps if not any(q != p and pre(q, p) for q in ps)]


def summaries(bodies):
    S = [([], []) for _ in bodies]
    while True:
        new = []
        for b in bodies:
            r, w = fp(S, b)
            new.append((normalize(r + w, True), normalize(w, True)))
        if new == S:
            return S
        S = new


# --- Lean output ----------------------------------------------------------------------------

def lpl(p):
    return f"({p[0]}, [{', '.join(map(str, p[1]))}])"


def lop(o):
    return f"(.place {lpl(o[1])})" if o[0] == "place" else f"(.const {o[1]})"


def lsigma(sigma, site):
    cases = " | ".join(f"{k} => {lpl(v)}" for k, v in sorted(sigma.items()))
    return (f"(fun k => match k with | {cases} | j => "
            f"if j = {FRAME} then ({FRAME}, [{site}]) else ({FRAME}, [{site}, j]))")


def lstmt(s, ind="  "):
    tag = s[0]
    if tag == "nop":
        return ".nop"
    if tag == "assign":
        _, d, f, a, b = s
        return f"(.assign {lpl(d)} {f} {lop(a)} {lop(b)})"
    if tag == "seq":
        return f"(.seq {lstmt(s[1], ind)}\n{ind}  {lstmt(s[2], ind + '  ')})"
    if tag == "ite":
        return f"(.ite {lpl(s[1])}\n{ind}  {lstmt(s[2], ind + '  ')}\n{ind}  {lstmt(s[3], ind + '  ')})"
    if tag == "call":
        _, f, sigma, site = s
        return f"(.call {f} {lsigma(sigma, site)})"
    raise ValueError(tag)


def lfp(f):
    r, w = f
    return f"([{', '.join(map(lpl, r))}], [{', '.join(map(lpl, w))}])"


def enc(p):
    return [p[0]] + p[1]


def flatten(s):
    return flatten(s[1]) + flatten(s[2]) if s[0] == "seq" else [s]


def main():
    path, entry = sys.argv[1], sys.argv[2]
    d = json.load(open(path))["translated"]
    collect_dedup(d)
    crate = d["crate_name"]
    by_id = {f["def_id"]: f for f in d["fun_decls"] if f}
    local = [f for f in d["fun_decls"]
             if f and f["item_meta"]["name"][0] == {"Ident": [crate, 0]} and "Structured" in (f.get("body") or {})]
    ids = {"by_id": by_id, "local": {f["def_id"]: i for i, f in enumerate(local)}}
    fns = [Fn(f, ids) for f in local]
    bodies = []
    for fn in fns:
        try:
            body, _ = fn.translate(fn.block_stmts(fn.block))
        except Unsupported as e:
            sys.exit(f"{name_of(fn.decl)}: unsupported: {e}")
        bodies.append(body)
    S = summaries(bodies)
    names = [name_of(f) for f in local]
    k = names.index(entry)
    calls = [s for s in flatten(bodies[k]) if s[0] == "call"]
    nodes = []
    for s in calls:
        r, w = fp(S, s)
        nodes.append(([enc(p) for p in normalize(r)], [enc(p) for p in normalize(w)]))

    print(f"""import Footprint

/-!
# `{crate}`, from Rust through Charon

Generated by `scripts/charon/llbc2lean.py` from Charon's LLBC of the crate `{crate}`
(`scripts/charon/{crate}/src/lib.rs`). Do not edit. Functions: {', '.join(f'{i} = `{n}`' for i, n in enumerate(names))}.

The summaries were computed by the script; Lean decides on its own that they pass the link check,
that the certificate for `{entry}`'s calls covers their footprints, and that `check` accepts it.
-/

namespace PsyncCharon.{crate.capitalize()}

open PsyncFootprint PsyncCert PsyncWorld
""")
    for i, (n, b) in enumerate(zip(names, bodies)):
        print(f"/-- The body of `{n}`, from LLBC. -/\ndef body{i} : Stmt :=\n  {lstmt(b)}\n")
        print(f"/-- The summary of `{n}`: places it may touch, places it may write. -/\ndef sum{i} : Fp := {lfp(S[i])}\n")
    print("def prog : Program := ⟨[" + ", ".join(f"(body{i}, sum{i})" for i in range(len(bodies))) + "]⟩\n")
    print("""/-- $$\\mathrm{validB}(\\mathrm{prog}) = \\mathrm{true}$$

**The link check passes** on the compiled crate: every summary covers its body. -/
theorem prog_valid : prog.validB = true := by decide
""")
    print(f"/-- The calls of `{entry}`, as the statements of a schedule. -/\ndef st : Nat → Stmt")
    for i, s in enumerate(calls):
        print(f"  | {i} => {lstmt(s)}")
    print("  | _ => .nop\n")
    nl = ", ".join(f"⟨[{', '.join(str(p).replace(' ', '') for p in r)}], [{', '.join(str(p).replace(' ', '') for p in w)}]⟩" for r, w in nodes)
    level = ", ".join(f"[{i}]" for i in range(len(calls)))
    print(f"""/-- All of `{entry}`'s calls in one level. -/
def cert : Cert where
  nodes := [{nl}]
  levels := [[{level}]]

/-- $$\\mathrm{{check}}(\\mathrm{{cert}}) = \\mathrm{{true}}$$ -/
theorem cert_ok : check cert = true := by decide

/-- $$\\forall i,\\ \\mathrm{{CoversNode}}(\\mathrm{{cert}}_i, \\mathrm{{fp}}(\\mathrm{{st}}_i))$$ -/
theorem cert_covers : ∀ i, CoversNode (cert.node i) (fp prog.sum (st i)) := by
  intro i
  match i with""")
    for i in range(len(calls)):
        print(f"  | {i} => unfold CoversNode; decide")
    print(f"""  | _ + {len(calls)} => exact ⟨fun _ h => by simp [st, fp] at h, fun _ h => by simp [st, fp] at h⟩

/-- $$\\mathrm{{run}}(\\mathrm{{schedule}}) = \\mathrm{{run}}(\\mathrm{{st}}_0; \\dots)$$

**`{entry}`'s calls in one level equal the calls in order**, for every fuel and every memory. -/
theorem schedule_sound (n : Nat) {{ms : List (List Stmt)}}
    (hx : Execs (cert.levels.map (List.map (List.map st))) ms) (m : Mem) :
    (memSys (prog.valid prog_valid) n).runAll ms.flatten m =
      (memSys (prog.valid prog_valid) n).runAll ((List.range cert.nodes.length).map st) m :=
  mir_schedule_sound (prog.valid prog_valid) n cert st cert_covers cert_ok hx m

end PsyncCharon.{crate.capitalize()}""")


if __name__ == "__main__":
    main()
