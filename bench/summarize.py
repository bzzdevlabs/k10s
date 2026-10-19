#!/usr/bin/env python3
"""summarize.py k8s|native  -> bench/results/<which>/SUMMARY.md from the JSON results (medians over repeats)."""
import json, os, statistics, sys
B = os.path.dirname(os.path.abspath(__file__))
which = sys.argv[1]
D = f"{B}/results/{which}"
med = statistics.median
def load(n):
    p = f"{D}/{n}.json"
    return json.load(open(p)) if os.path.exists(p) else None
def table(head, rows):
    out = ["| " + " | ".join(head) + " |", "|" + "|".join("---" for _ in head) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return "\n".join(out) + "\n"
def f(x, d=0): return "n/a" if x is None else f"{x:,.{d}f}"
def mm(xs, d=0): return f"{f(med(xs), d)} ({f(min(xs), d)}-{f(max(xs), d)})" if xs else "n/a"
L = []
def envblock(e):
    L.append("```")
    for k, v in e.items(): L.append(f"{k}: {v}")
    L.append("```\n")

def lat_table(runs, with_place):
    rows = []
    keys = sorted({(r.get("place", ""), r["target"], r["conns"]) for r in runs}, key=lambda k: (k[0] != "host", k[0], k[1] != "echo", k[2]))
    for pl, tg, c in keys:
        rs = [r for r in runs if r.get("place", "") == pl and r["target"] == tg and r["conns"] == c]
        q = lambda k: [r["latency_us"][k] for r in rs]
        row = ([pl] if with_place else []) + [tg, c, f(med([r["rps"] for r in rs])), f(med(q("p50"))), f(med(q("p90"))), f(med(q("p99"))), f(med(q("p99.9"))), f(med(q("max"))),
               sum(r["errors"] for r in rs), f(med([r["loadgen_cpu_s"]["total"] / r["duration_s"] for r in rs]), 2)]
        rows.append(row)
    return table((["loadgen at"] if with_place else []) + ["target", "conns", "rps", "p50 µs", "p90 µs", "p99 µs", "p99.9 µs", "max µs", "errors", "loadgen cores"], rows)

if which == "k8s":
    L.append("# k8s (kind) baseline results\n")
    first = None
    for n in ("idle", "pods", "first_response", "latency", "failover", "rollout", "sizes", "nodefail"):
        d = load(n) or load(n + "-three")
        if d: first = first or d; 
    if first: envblock(first["env"])
    d = load("idle")
    if d:
        L.append("## Idle footprint, single-node cluster (60 s windows, median of repeats)\n")
        rs = d["runs"]
        L.append(table(["metric", "value"], [
            ["cluster working set (cgroup memory.current - inactive_file), MiB", mm([r["cluster_working_set_mib"] for r in rs], 0)],
            ["cluster memory.current (incl. page cache), MiB", mm([r["cluster_mem_current_mib"] for r in rs], 0)],
            ["sum of process RSS inside node, MiB", mm([r["rss_sum_mib"] for r in rs], 0)],
            ["sum of process PSS inside node, MiB", mm([r["pss_sum_mib"] for r in rs], 0)],
            ["idle CPU, cores (cgroup cpu.stat)", mm([r["cluster_cpu_cores"] for r in rs], 3)]]))
        comps = sorted({k for r in rs for k in r["components"]}, key=lambda k: -med([r["components"].get(k, {}).get("rss_mib", 0) for r in rs]))
        L.append("Per-component (inside node): procs, RSS MiB, PSS MiB, CPU % of one core\n")
        L.append(table(["component", "procs", "RSS MiB", "PSS MiB", "CPU % core"], [[k, rs[0]["components"].get(k, {}).get("procs", 0),
            f(med([r["components"].get(k, {}).get("rss_mib", 0) for r in rs]), 1), f(med([r["components"].get(k, {}).get("pss_mib", 0) for r in rs]), 1),
            f(med([r["components"].get(k, {}).get("cpu_core_pct", 0) for r in rs]), 2)] for k in comps]))
    d = load("idle-three")
    if d:
        rs = d["runs"]
        L.append("## Idle footprint, 3-node cluster (1 control-plane + 2 workers)\n")
        L.append(table(["metric", "value"], [
            ["cluster working set, MiB", mm([r["cluster_working_set_mib"] for r in rs], 0)],
            ["cluster idle CPU, cores", mm([r["cluster_cpu_cores"] for r in rs], 3)]] +
            [[f"  {n} working set MiB / CPU cores", f"{f(med([r['nodes'][n]['working_set_mib'] for r in rs]),0)} / {f(med([r['nodes'][n]['cpu_cores'] for r in rs]),3)}"] for n in sorted(rs[0]["nodes"])]))
    d = load("pods")
    if d:
        L.append("## Per-pod overhead and all-Ready time (echo scaled 0 -> N, no readiness probe)\n")
        ns = sorted({r["n"] for r in d["runs"]})
        rows = []
        for n in ns:
            rs = [r for r in d["runs"] if r["n"] == n]
            g = lambda c, k: med([r["per_component"].get(c, {}).get(k, 0) for r in rs])
            rows.append([n, mm([r["all_ready_ms"] / 1000 for r in rs], 2), f(med([r["working_set_kib_per_pod"] for r in rs]) / 1024, 1),
                         f(med([r["rss_sum_kib_per_pod"] for r in rs]) / 1024, 1), f(med([r["pss_sum_kib_per_pod"] for r in rs]) / 1024, 1),
                         f(g("app", "rss_kib_per_pod") / 1024, 1), f(g("containerd-shim", "rss_kib_per_pod") / 1024, 1), f(g("pause", "rss_kib_per_pod") / 1024, 1),
                         f(g("app", "pss_kib_per_pod") / 1024, 1), f(g("containerd-shim", "pss_kib_per_pod") / 1024, 1), f(g("pause", "pss_kib_per_pod") / 1024, 1)])
        L.append(table(["N", "all Ready s (med, min-max)", "node working-set delta / pod MiB", "RSS sum / pod MiB", "PSS sum / pod MiB", "app RSS", "shim RSS", "pause RSS", "app PSS", "shim PSS", "pause PSS"], rows))
        L.append("RSS columns are MiB per pod. Working set includes kernel/cgroup/page-cache memory charged to the node; RSS double-counts shared pages (PSS does not).\n")
    d = load("first_response")
    if d:
        rs = d["runs"]
        L.append("## One pod: scale 0->1 to first response (ms)\n")
        L.append(table(["phase", "median (min-max)"], [[k, mm([r[k] for r in rs])] for k in ("scale_cmd_ms", "to_scheduled_ms", "to_ready_ms", "to_first_http_ms", "scheduled_to_first_http_ms")]))
    d = load("latency")
    if d:
        L.append("## Request latency/throughput through NodePort (20 s, closed loop, median of repeats)\n")
        L.append("`host` = loadgen on host via kind extraPortMappings (on Docker Desktop this crosses the VM port forwarder); `node` = loadgen inside the kind node hitting node-IP:NodePort. `chain` adds one ClusterIP (kube-proxy) hop.\n")
        L.append(lat_table(d["runs"], True))
    d = load("failover")
    if d:
        L.append("## Pod failover\n")
        rows = []
        for sc in sorted({r["scenario"] for r in d["runs"]}):
            rs = [r for r in d["runs"] if r["scenario"] == sc]
            if "replacement_ready_ms" in rs[0]:
                rows.append([sc, mm([r["replacement_ready_ms"] for r in rs]), mm([r["first_success_after_last_fail_ms"] or 0 for r in rs]), mm([r["outage_window_ms"] for r in rs]), mm([r["failed_after_event"] for r in rs])])
            else:
                rows.append([sc, "-", "-", "-", f"{mm([r['errors'] for r in rs])} errors of ~{med([r['requests'] for r in rs]):.0f} reqs"])
        L.append(table(["scenario", "delete -> replacement Ready ms", "delete -> requests OK again ms", "outage window ms (first to last failed probe)", "failed probes (each fails fast or after 0.5 s timeout) / loadgen errors"], rows))
    d = load("rollout")
    if d:
        rs = d["runs"]
        L.append("## Rolling update, 10 replicas, echo v1<->v2\n")
        L.append(table(["metric", "median (min-max)"], [["`rollout status` complete, s", mm([r["rollout_status_ms"] / 1000 for r in rs], 2)],
            ["until old pods fully gone, s", mm([r["rollout_and_old_pods_gone_ms"] / 1000 for r in rs], 2)],
            ["failed probes during update (no readiness probe)", mm([r["failed_after_event"] for r in rs])]]))
    for suffix, title in (("", ""), ("-three", "")):
        d = load("nodefail" + suffix)
        if d:
            L.append("## 3-node: worker failure (what Kubernetes actually does, seconds after failure)\n")
            keys = ["node_notready_s", "node_tainted_unreachable_s", "pods_marked_notready_on_victim_s", "endpoints_reduced_s", "first_pod_terminating_s", "replacement_pod_created_s", "all_replicas_ready_on_healthy_nodes_s"]
            L.append(table(["mode"] + keys + ["probe fail ratio", "waited s"], [[r["mode"]] + [r.get(k) if r.get(k) is not None else "never (cap)" for k in keys] + [r["probe"]["fail_ratio"], r["waited_s"]] for r in d["runs"]]))
    d = load("sizes")
    if d:
        s = d["runs"][0]
        L.append("## Artifact sizes\n")
        rows = [[f"image {k}", f"{v/1e6:.2f} MB"] for k, v in s["docker_images_bytes"].items()]
        rows += [[f"binary {k}", f"{v/1e6:.2f} MB"] for k, v in s["binaries_bytes"].items()]
        rows += [["kindest/node image (docker images)", s.get("kind_node_image_docker_images_size")], ["kindest/node image (inspect .Size)", f"{s['kind_node_image_bytes']/1e6:.0f} MB"],
                 ["containerd store inside node", f"{s['node_containerd_dir_kib']/1024:.0f} MiB"], ["etcd data dir (idle)", f"{s['node_etcd_dir_kib']/1024:.0f} MiB"]]
        rows += [[f"k8s binary {k}", f"{v/1e6:.1f} MB"] for k, v in s["k8s_binaries_in_node_bytes"].items()]
        rows += [[f"image in node: {k}", f"{v/1e6:.1f} MB"] for k, v in s["images_in_node"].items() if "k10s-bench" not in k]
        L.append(table(["artifact", "size"], rows))
else:
    L.append("# Native (bare process) baseline results\n")
    d = load("scale")
    if d:
        envblock(d["env"])
        L.append("## echo x N on consecutive ports (no orchestration)\n")
        rows = []
        for n in sorted({r["n"] for r in d["runs"]}):
            rs = [r for r in d["runs"] if r["n"] == n]
            rows.append([n, mm([r["spawn_all_ms"] for r in rs], 1), mm([r["all_ready_ms"] for r in rs], 1), f(med([r["rss_kib_per_proc_median"] for r in rs]) / 1024, 2),
                         f(med([r["pss_kib_per_proc_median"] for r in rs]) / 1024, 2), f(med([r["rss_total_mib"] for r in rs]), 1), f(med([r["pss_total_mib"] for r in rs]), 1),
                         f(med([r["idle_cpu_cores_total"] for r in rs]), 3)])
        L.append(table(["N", "spawn all ms", "all answering HTTP ms", "RSS/proc MiB", "PSS/proc MiB", "total RSS MiB", "total PSS MiB", "idle CPU cores"], rows))
    d = load("latency")
    if d:
        L.append("## Latency/throughput vs one instance (loadgen on same host, 127.0.0.1, 20 s closed loop)\n")
        L.append("`chain` = frontend process -> echo process over loopback HTTP (the bare-process analogue of the Service hop).\n")
        L.append(lat_table(d["runs"], False))
open(f"{D}/SUMMARY.md", "w").write("\n".join(L))
print(f"wrote {D}/SUMMARY.md")
