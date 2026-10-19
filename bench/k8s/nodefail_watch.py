#!/usr/bin/env python3
"""nodefail_watch.py <kubeconfig> <context> <victim_node> <duration_s> <outfile>
Polls every ~2 s and appends JSON lines: elapsed s, victim Ready status, #ready endpoints, per-pod state summary."""
import json, subprocess, sys, time
kc, ctx, victim, dur, out = sys.argv[1:6]; dur = float(dur)
K = ["kubectl", "--kubeconfig", kc, "--context", ctx]
def kj(*a):
    r = subprocess.run(K + list(a) + ["-o", "json"], capture_output=True, text=True, timeout=20)
    return json.loads(r.stdout) if r.stdout else {}
t0 = time.time()
with open(out, "w", buffering=1) as f:
    while time.time() - t0 < dur:
        t = time.time() - t0
        try:
            n = next(i for i in kj("get", "nodes")["items"] if i["metadata"]["name"] == victim)
            ready = next(c["status"] for c in n["status"]["conditions"] if c["type"] == "Ready")
            tainted = [x["key"] for x in n["spec"].get("taints", [])]
            ep = kj("get", "endpoints", "echo")
            eps = sum(len(s.get("addresses", [])) for s in ep.get("subsets", []) or [])
            pods = []
            for p in kj("get", "pods", "-l", "app=echo")["items"]:
                rdy = any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", []))
                pods.append({"node": p["spec"].get("nodeName"), "ready": rdy, "terminating": "deletionTimestamp" in p["metadata"], "phase": p["status"]["phase"]})
            f.write(json.dumps({"t": round(t, 1), "victim_ready": ready, "taints": tainted, "ready_endpoints": eps, "pods": pods}) + "\n")
        except Exception as e:
            f.write(json.dumps({"t": round(t, 1), "error": str(e)[:100]}) + "\n")
        time.sleep(1.0)
