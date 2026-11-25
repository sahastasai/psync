#!/usr/bin/env python3
"""Check server logs against the commutativity specifications of verify/Specs.lean.

    check_logs.py --kube audit.log      Kubernetes API audit log (JSON lines, any level)
    check_logs.py --s3 garage.log       S3 server log (Garage's request lines)

For every pair of requests that may have been in flight at the same time, it decides whether the
specification lets them commute, with the same definitions as the Lean file:

- S3 (`s3Spec`): two reads; requests whose keys are apart (neither is a prefix of the other;
  keys are paths of characters, so a `ListObjectsV2` prefix is a string prefix, exactly as S3
  matches it); or the same PUT or DELETE twice. A PUT's body is not logged, so two PUTs to one key
  are counted as not allowed.
- Kubernetes (`kubeSpec`, for clients that ignore resourceVersions): two reads; writes or gets of
  different objects; a list and a write in another namespace. A cluster-wide list is in every
  namespace.

A pair the spec does not allow, overlapping in time, is a pair that must not have been run
concurrently by a client that relies on the spec: if it came from psync's schedule, it is a
counterexample to that schedule. The log levels used here record requests, not bodies or
resourceVersions, so they cannot test the server side of the spec (that the allowed pairs really
commute at the server).

Overlap. Audit events carry the time a request was received and completed: exact. Garage logs
only the start of a request; a request ends before the next one on the same connection starts
(HTTP/1.1 without pipelining), so that start bounds it from above. The last request of a
connection is bounded by CAP seconds. Both bounds make intervals longer, never shorter, so every
pair that really overlapped is checked.
"""
import argparse, collections, datetime, json, re, sys

CAP = 5.0


def ts(s):
    s = s.rstrip("Z")
    if "." in s:
        a, b = s.split(".")
        s = a + "." + (b + "000000")[:6]
    return datetime.datetime.fromisoformat(s).timestamp()


def lpre(a, b):
    return b.startswith(a)


def apart(a, b):
    return not lpre(a, b) and not lpre(b, a)


# --- S3 -------------------------------------------------------------------------------------

def s3_requests(path):
    """(start, connection, kind, key) for every request; kind: get, head, list, put, delete."""
    line_re = re.compile(r"^(\S+Z)\s+INFO .*?generic_server: (\S+) \(key [^)]*\) (GET|PUT|DELETE|HEAD|POST) (\S+)")
    out = []
    for raw in open(path, errors="replace"):
        m = line_re.match(re.sub(r"\x1b\[[0-9;]*m", "", raw))
        if not m:
            continue
        t, conn, method, uri = m.groups()
        target, _, query = uri.partition("?")
        q = dict(kv.partition("=")[::2] for kv in query.split("&") if kv)
        bucket, _, key = target.lstrip("/").partition("/")
        if method == "GET" and "list-type" in q:
            kind, key = "list", re.sub(r"%([0-9A-Fa-f]{2})", lambda x: chr(int(x.group(1), 16)), q.get("prefix", ""))
        else:
            kind = {"GET": "get", "HEAD": "head", "PUT": "put", "DELETE": "delete", "POST": "post"}[method]
        out.append((ts(t), conn, kind, bucket + "/" + key, uri))
    return out


def s3_intervals(reqs):
    by_conn = collections.defaultdict(list)
    for r in reqs:
        by_conn[r[1]].append(r)
    capped = 0
    out = []
    for conn, rs in by_conn.items():
        rs.sort()
        for i, r in enumerate(rs):
            if i + 1 < len(rs):
                end = rs[i + 1][0]
            else:
                end, capped = r[0] + CAP, capped + 1
            out.append((r[0], end) + r[1:])
    return out, capped


def s3_spec(a, b):
    """The reason s3Spec allows the pair, or None."""
    ka, kb = a[3], b[3]
    reads = {"get", "head", "list"}
    if ka in reads and kb in reads:
        return "two reads"
    if "post" in (ka, kb):
        return None
    if apart(a[4], b[4]):
        return "keys apart"
    if ka == kb == "delete" and a[4] == b[4]:
        return "same DELETE twice"
    return None


# --- Kubernetes -----------------------------------------------------------------------------

READ_VERBS = {"get", "list", "watch"}


def kube_requests(path):
    out = []
    for line in open(path):
        e = json.loads(line)
        if e.get("stage") != "ResponseComplete":
            continue
        o = e.get("objectRef") or {}
        out.append((ts(e["requestReceivedTimestamp"]), ts(e["stageTimestamp"]),
                    e.get("userAgent", "-"), e["verb"],
                    (o.get("resource", ""), o.get("namespace"), o.get("name")), e["requestURI"]))
    return out


def kube_spec(a, b):
    va, vb = a[3], b[3]
    (ra, na, ma), (rb, nb, mb) = a[4], b[4]
    if va in READ_VERBS and vb in READ_VERBS:
        return "two reads"
    if ma is not None and mb is not None:
        return "different objects" if (ra, na, ma) != (rb, nb, mb) else None
    # A list (no name) and a write: only in different namespaces; cluster-wide lists see all.
    lst, wr = (a, b) if ma is None else (b, a)
    if wr[3] in READ_VERBS:
        return "list and read"
    if lst[4][1] is not None and wr[4][1] is not None and lst[4][1] != wr[4][1]:
        return "list and write in another namespace"
    return None


# --- Overlapping pairs ----------------------------------------------------------------------

def overlapping(intervals, same_client):
    """Pairs (a, b) whose intervals overlap and that come from the same client."""
    intervals = sorted(intervals)
    active = []
    for r in intervals:
        active = [a for a in active if a[1] > r[0]]
        for a in active:
            if same_client(a, r):
                yield a, r
        active.append(r)


def report(name, pairs, spec, show):
    reasons = collections.Counter()
    bad = []
    n = 0
    for a, b in pairs:
        n += 1
        why = spec(a, b)
        if why is None:
            bad.append((a, b))
        else:
            reasons[why] += 1
    print(f"{name}: {n} pairs may have overlapped")
    for why, k in reasons.most_common():
        print(f"  allowed, {why}: {k}")
    print(f"  NOT allowed by the spec: {len(bad)}")
    for a, b in bad[:show]:
        print(f"    {a[-1]}  ||  {b[-1]}")
    return len(bad)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--kube")
    p.add_argument("--s3")
    p.add_argument("--show", type=int, default=5)
    args = p.parse_args()
    bad = 0
    if args.kube:
        reqs = kube_requests(args.kube)
        verbs = collections.Counter(r[3] for r in reqs)
        print(f"kube: {len(reqs)} requests: {dict(verbs.most_common())}")
        bad += report("kube", overlapping(reqs, lambda a, b: a[2] == b[2]), kube_spec, args.show)
    if args.s3:
        reqs = s3_requests(args.s3)
        kinds = collections.Counter(r[2] for r in reqs)
        intervals, capped = s3_intervals(reqs)
        print(f"s3: {len(reqs)} requests: {dict(kinds.most_common())}; "
              f"{capped} last-on-connection intervals capped at {CAP} s")
        # One client: the bench process; connections differ, the access key is the same.
        bad += report("s3", overlapping(
            [(s, e, c, k, key) for (s, e, c, k, key, _) in intervals], lambda a, b: a[2] != b[2]),
            s3_spec, args.show)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
