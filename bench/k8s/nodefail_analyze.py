#!/usr/bin/env python3
"""nodefail_analyze.py <watchfile> <probefile> <t_fail_epoch_ms> <victim> -> JSON timeline of events (seconds after the failure)."""
import json, sys
w = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
w = [x for x in w if "error" not in x]
probe = [tuple(map(int, l.split())) for l in open(sys.argv[2]) if l.strip()]
tf = int(sys.argv[3]); victim = sys.argv[4]
def first(pred):
    for x in w:
        if pred(x): return x["t"]
    return None
ev = {}
base = w[0]
ev["node_notready_s"] = first(lambda x: x["victim_ready"] != "True")
ev["node_tainted_unreachable_s"] = first(lambda x: any("unreachable" in k or "not-ready" in k for k in x["taints"]))
n0 = base["ready_endpoints"]
ev["initial_ready_endpoints"] = n0
ev["endpoints_reduced_s"] = first(lambda x: x["ready_endpoints"] < n0)
ev["pods_marked_notready_on_victim_s"] = first(lambda x: any(p["node"] == victim and not p["ready"] for p in x["pods"]))
ev["first_pod_terminating_s"] = first(lambda x: any(p["terminating"] for p in x["pods"]))
total = len(base["pods"])
ev["replacement_pod_created_s"] = first(lambda x: len(x["pods"]) > total or any(p["node"] != victim and p["node"] and not p["ready"] and not p["terminating"] for p in x["pods"]))
ev["all_replicas_ready_on_healthy_nodes_s"] = first(lambda x: sum(1 for p in x["pods"] if p["ready"] and p["node"] != victim and not p["terminating"]) >= total)
after = [(t, c) for t, c in probe if t >= tf]
bad = [t for t, c in after if c != 200]
ev["probe"] = {"probes_after_failure": len(after), "failed": len(bad), "fail_ratio": round(len(bad) / max(1, len(after)), 3),
               "first_fail_s": round((bad[0] - tf) / 1000, 1) if bad else None, "last_fail_s": round((bad[-1] - tf) / 1000, 1) if bad else None}
print(json.dumps(ev))
