#!/usr/bin/env python3
"""compare.py -> bench/results/COMPARISON.md: k8s (kind) vs bare processes vs k10s, side by side, from the JSON results
of bench/k8s, bench/native and bench/k10s (medians over repeats). Missing inputs show as n/a."""
import json, os, statistics, time

B = os.path.dirname(os.path.abspath(__file__))
R = f"{B}/results"
OUT = f"{R}/COMPARISON.md"


def load(which, name):
    p = f"{R}/{which}/{name}.json"
    return json.load(open(p)) if os.path.exists(p) else None


K = {n: load("k8s", n) for n in ("idle", "idle-three", "pods", "first_response", "latency", "latency-open", "failover", "rollout", "nodefail-three", "sizes")}
N = {n: load("native", n) for n in ("scale", "latency", "latency-open")}
T = {n: load("k10s", n) for n in ("idle", "overhead", "startup", "latency", "failover", "rollout", "sizes")}


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def runs(d, **kw):
    if not d:
        return []
    return [r for r in d["runs"] if all(r.get(k) == v for k, v in kw.items())]


def f(x, d=0, unit=""):
    if x is None:
        return "n/a"
    if isinstance(x, str):
        return x
    return f"{x:,.{d}f}{unit}"


def ratio(a, b, d=1):
    if a is None or b is None or b == 0:
        return "n/a"
    r = a / b
    if r >= 100:
        return f"{r:,.0f}×"
    return f"{r:,.{d}f}×"


def table(head, rows):
    out = ["| " + " | ".join(head) + " |", "|" + "|".join("---" for _ in head) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return "\n".join(out) + "\n"


def noisy_note(rs):
    if not rs:
        return ""
    n = sum(1 for r in rs if r.get("noisy"))
    return f" ({n}/{len(rs)} noisy)" if n else ""


# ---------------------------------------------------------------- extractors
def k8s_idle(name="idle"):
    rs = runs(K[name])
    return {"ws": med([r["cluster_working_set_mib"] for r in rs]), "rss": med([r["rss_sum_mib"] for r in rs]),
            "pss": med([r["pss_sum_mib"] for r in rs]), "cpu": med([r["cluster_cpu_cores"] for r in rs]),
            "anon": med([sum(n.get("anon_mib", 0) for n in r["nodes"].values()) for r in rs])} if rs else {}


def k8s_idle3():
    return k8s_idle("idle-three")


def t_idle(cfg, svc):
    rs = runs(T["idle"], config=cfg, services=svc)
    return {"ws": med([r.get("working_set_mib") for r in rs]), "rss": med([r["rss_sum_mib"] for r in rs]), "pss": med([r["pss_sum_mib"] for r in rs]),
            "cpu": med([r["cpu_cores"] for r in rs]), "rs": rs,
            "anon": med([sum(n["cg_anon_mib"] for n in r["nodes"]) if all("cg_anon_mib" in n for n in r["nodes"]) else None for r in rs]),
            "pss_anon": med([r.get("pss_anon_sum_mib") for r in rs])} if rs else {}


def k8s_pods(n):
    rs = runs(K["pods"], n=n)
    return {"ws": med([r["working_set_kib_per_pod"] for r in rs]), "rss": med([r["rss_sum_kib_per_pod"] for r in rs]),
            "pss": med([r["pss_sum_kib_per_pod"] for r in rs]), "ready_ms": med([r["all_ready_ms"] for r in rs])} if rs else {}


def nat_scale(n):
    rs = runs(N["scale"], n=n)
    return {"rss": med([r["rss_kib_per_proc_median"] for r in rs]), "pss": med([r["pss_total_mib"] * 1024 / n for r in rs]),
            "ready_ms": med([r["all_ready_ms"] for r in rs])} if rs else {}


def t_ovh(var, n):
    rs = runs(T["overhead"], variant=var, n_target=n)
    if not rs:
        return {}
    o = {"ws": med([r.get("working_set_kib_per_unit") for r in rs]), "rss": med([r["rss_kib_per_unit"] for r in rs]),
         "pss": med([r["pss_kib_per_unit"] for r in rs]), "units": med([r["n"] for r in rs]), "rs": rs}
    if var == "services":
        o["state_ws"] = med([(r.get("per_service_state") or {}).get("working_set_kib_per_unit") for r in rs])
        o["inst_ws"] = med([(r.get("per_instance") or {}).get("working_set_kib_per_unit") for r in rs])
        o["apply_ms"] = med([r["apply_cmd_ms"] for r in rs]); o["cli_ms"] = med([r["apply_cli_only_ms"] for r in rs])
    return o


def t_start(var, n):
    rs = runs(T["startup"], variant=var, n=n)
    return {"all": med([r["all_serving_ms"] for r in rs]), "apply": med([r["apply_cmd_ms"] for r in rs]), "rs": rs} if rs else {}


def t_cold(module):
    rs = runs(T["startup"], variant="cold", module=module)
    return {k: med([r.get(k) for r in rs]) for k in ("cold_ms", "warm_ms", "cold_keepalive_ms", "warm_keepalive_ms", "cold_minus_warm_keepalive_ms",
                                                     "server_init_ms_per_instance")} if rs else {}


# Closed-loop latency cells: with STAT=quietest (default) each cell uses, among its repeats, the one during which other
# processes used the least CPU (`other_cpu_cores`, measured independently of the result, same rule for every system);
# repeats without that measurement (older runs) fall back to the median. STAT=median uses the median of all repeats.
# Open-loop cells always use medians (a single run's tail percentiles are too volatile to pick from).
STAT = os.environ.get("COMPARE_STAT", "quietest")


def lat(d, mode="closed", **kw):
    allrs = [r for r in runs(d, **kw) if r.get("mode", "closed") == mode]
    if not allrs:
        return {}
    rs = allrs
    if STAT == "quietest" and mode == "closed" and all(r.get("other_cpu_cores") is not None for r in allrs):
        rs = [min(allrs, key=lambda r: r["other_cpu_cores"])]
    o = {"rps": med([r["rps"] for r in rs]), "rs": allrs, "picked": rs}
    for k in ("p50", "p90", "p99", "p99.9", "max"):
        o[k] = med([r["latency_us"][k] for r in rs])
        if mode == "open":
            o["jc_" + k] = med([r.get("latency_us_jc", r["latency_us"])[k] for r in rs])
    cr = [r.get("sut_cpu_us_per_req", r.get("server_cpu_us_per_req")) for r in rs]
    o["cpu"] = med(cr)
    o["cpu_approx"] = False
    if o["cpu"] is None and "node_cpu_cores_avg_incl_warmup" in rs[0]:
        # older k8s runs: node CPU averaged over warm-up + measured window; loadgen inside the node subtracted for place=node
        def one(r):
            c = r["node_cpu_cores_avg_incl_warmup"] - (r["loadgen_cpu_s"]["total"] / r["duration_s"] if r.get("place") == "node" else 0)
            return c * 1e6 / r["rps"] if r["rps"] else None
        o["cpu"] = med([one(r) for r in rs]); o["cpu_approx"] = True
    o["errors"] = sum(r["errors"] for r in allrs)
    o["noisy"] = sum(1 for r in rs if r.get("noisy"))
    o["other"] = med([r.get("other_cpu_cores") for r in rs])
    return o


def k8s_lat(place, target, conns=None, rate=None):
    if rate is None:
        return lat(K["latency"], place=place, target=target, conns=conns)
    return lat(K["latency-open"], "open", place=place, target=target, target_rps=rate)


def nat_lat(target, conns=None, rate=None):
    if rate is None:
        return lat(N["latency"], target=target, conns=conns)
    return lat(N["latency-open"], "open", target=target, target_rps=rate)


def t_lat(place, target, conns=None, rate=None):
    if rate is None:
        return lat(T["latency"], place=place, target=target, conns=conns)
    return lat(T["latency"], "open", place=place, target=target, target_rps=rate)


def t_fail(sc):
    return runs(T["failover"], scenario=sc)


def k8s_fail(sc):
    return runs(K["failover"], scenario=sc)


def g(rs, k):
    return med([r.get(k) for r in rs])


# ---------------------------------------------------------------- document
D = []
P = D.append


def envline(name, d):
    if not d:
        return f"- {name}: no results"
    e = d["env"]
    extra = ""
    if "loadavg" in e:
        extra = f", load avg at write {e['loadavg'][0]}"
    if "docker_os" in e:
        extra += f", {e.get('docker_os')} VM kernel {e.get('docker_vm_kernel')} ({e.get('docker_vm_cpus')} vCPU, {e.get('docker_vm_mem_gib')} GiB), {e.get('k8s_version', '')}"
    if "k10s_build" in e:
        b = e["k10s_build"]
        extra += f", k10s {b.get('git_rev')} (binary {b.get('sha256')}, built {b.get('source_mtime')})"
    return f"- {name}: {e.get('date')} on {e.get('host_cpu')} ({e.get('host_threads')} threads), host kernel {e.get('host_kernel')}{extra}"


P("# k10s vs Kubernetes vs bare processes\n")
P(f"Generated by `bench/compare.py` on {time.strftime('%Y-%m-%d %H:%M')} from `bench/results/{{k8s,native,k10s}}/*.json` "
  "(medians over repeats; each suite's own SUMMARY.md has min-max ranges). Same machine for all three: one 4-core/8-thread laptop CPU. "
  "**Read the caveats at the end before quoting any number.**\n")
P("Inputs:\n")
for name, d in (("k8s (kind) single-node suite (idle, pods, startup, failover, rollout, sizes)", K["idle"]), ("k8s latency, closed loop", K["latency"]),
                ("k8s latency, open loop", K["latency-open"]), ("k8s 3-node suite", K["idle-three"]),
                ("native processes, scale", N["scale"]), ("native latency, closed loop", N["latency"]), ("native latency, open loop", N["latency-open"]),
                ("k10s suite (idle, overhead, startup, failover, rollout, sizes)", T["idle"]), ("k10s latency", T["latency"])):
    P(envline(name, d))
allr = [r for d in list(T.values()) + [K["latency-open"], N["latency-open"]] if d for r in d["runs"] if "noisy" in r]
if allr:
    nz = sum(1 for r in allr if r.get("noisy"))
    P(f"\nRuns flagged noisy: {nz} of {len(allr)} recorded k10s/new-baseline runs (latency runs: other processes used more than 0.5 cores during "
      "the measured window, `other_cpu_cores` = host busy CPU - system under test - loadgen; other experiments: 1-min load average above 1.0). "
      "The kind and native baselines from the first session were taken while other agents compiled Rust (see their SUMMARY.md). "
      "**Preliminary: redo on a quiet machine with `bench/run-final.sh`.**\n")

# ---------------- headline
P("## Headline\n")
ki, ki3 = k8s_idle(), k8s_idle3()
ti8, ti8s, ti3, ti3s = t_idle("single-8", 0), t_idle("single-8", 4), t_idle("cluster-3", 0), t_idle("cluster-3", 4)
kp100, np100 = k8s_pods(100), nat_scale(100)
ts100, ti100 = t_ovh("services", 100), t_ovh("instances", 100)
rows = []
rows.append(["Idle footprint, no workloads: working set MiB", f(ki.get("ws")), f(ki3.get("ws")), "0 (no platform)", f(ti8.get("ws"), 2), f(ti3.get("ws"), 2), ratio(ki.get("ws"), ti8.get("ws"), 0)])
rows.append(["Idle CPU, % of one core", f(ki.get("cpu") and ki["cpu"] * 100, 1), f(ki3.get("cpu") and ki3["cpu"] * 100, 1), "0", f(ti8.get("cpu") and ti8["cpu"] * 100, 3),
             f(ti3.get("cpu") and ti3["cpu"] * 100, 3), ratio(ki.get("cpu"), ti8.get("cpu"), 0)])
rows.append(["Per echo replica (N=100): working set KiB", f(kp100.get("ws")), "-", f"{f(np100.get('pss'))} (PSS)", f"{f(ts100.get('ws'), 1)} (service+instance)",
             "-", ratio(kp100.get("ws"), ts100.get("ws"), 0)])
rows.append(["Per echo replica (N=100): PSS KiB", f(kp100.get("pss")), "-", f(np100.get("pss")), f(ts100.get("pss"), 1), "-", ratio(kp100.get("pss"), ts100.get("pss"), 0)])
ts100k = t_ovh("services", 100000)
rows.append(["Density: replicas per GiB of node memory (from the per-replica working set)", f(kp100.get("ws") and 1048576 / kp100["ws"]) + " (max 110 pods/node by default)", "-",
             f(np100.get("pss") and 1048576 / np100["pss"]) + " (PSS)", f(ts100k.get("ws") and 1048576 / ts100k["ws"]) + " (measured at 100k)" if ts100k else "n/a", "-", "-"])
ks = K["first_response"]
k_first = g(runs(ks), "to_first_http_ms") if ks else None
n1 = nat_scale(1)
tc = t_cold("echo")
rows.append(["Cold start of one replica: deploy/scale -> first HTTP 200, ms", f(k_first), "-", f(n1.get("ready_ms"), 1) + " (spawn -> answer)",
             f(t_start("single", 1).get("all"), 1) + " (apply -> answer)", f(t_start("cluster-3", 1).get("all"), 1), ratio(k_first, t_start("single", 1).get("all"), 0)])
rows.append(["Cold start of an instance (first request to a never-used service), extra ms vs warm", "-", "-", "-", f(tc.get("cold_minus_warm_keepalive_ms"), 3), "-", "-"])
rows.append(["Start 100 replicas/services -> all serving, s", f(kp100.get("ready_ms") and kp100["ready_ms"] / 1000, 2), "-", f(np100.get("ready_ms") and np100["ready_ms"] / 1000, 2),
             f(t_start("single", 100).get("all") and t_start("single", 100)["all"] / 1000, 3), f(t_start("cluster-3", 100).get("all") and t_start("cluster-3", 100)["all"] / 1000, 3),
             ratio(kp100.get("ready_ms"), t_start("single", 100).get("all"), 0)])
for tg, c, lbl in (("echo", 1, "echo, 1 conn: p50 µs"), ("echo", 64, "echo, 64 conns: req/s"), ("chain", 1, "chain (service hop), 1 conn: p50 µs"), ("chain", 64, "chain, 64 conns: req/s"),
                   ("compute", 1, "compute n=1000, 1 conn: p50 µs")):
    key = "p50" if "p50" in lbl else "rps"
    kh, kn, nn, ts = k8s_lat("host", tg, c), k8s_lat("node", tg, c), nat_lat(tg, c), t_lat("single-8", tg, c)
    tcl = t_lat("cluster-3-forwarded", tg, c) if tg == "echo" else {}
    r = ratio(kn.get(key), ts.get(key), 1) if key == "p50" else ratio(ts.get(key), kn.get(key), 1)
    rows.append([f"{lbl} (k8s: host / in-node loadgen)", f"{f(kh.get(key))} / {f(kn.get(key))}", "-", f(nn.get(key)), f(ts.get(key)),
                 f(tcl.get(key)) + (" (via other node)" if tcl else ""), r + (" (vs in-node)" if r != "n/a" else "")])
ko, no, to = k8s_lat("host", "echo", rate=5000), nat_lat("echo", rate=5000), t_lat("single-8", "echo", rate=5000)
kon = k8s_lat("node", "echo", rate=5000)
rows.append(["echo open loop 5k req/s: p99 µs (jitter-corrected)", f"{f(ko.get('jc_p99'))} / {f(kon.get('jc_p99'))}", "-", f(no.get("jc_p99")), f(to.get("jc_p99")), "-",
             ratio(kon.get("jc_p99"), to.get("jc_p99"), 1) + (" (vs in-node)" if kon else "")])
kc, nc, tcc = k8s_lat("node", "echo", 16), nat_lat("echo", 16), t_lat("single-8", "echo", 16)
rows.append(["CPU per echo request at 16 conns, µs (system under test)", f"{f(k8s_lat('host', 'echo', 16).get('cpu'), 1)} / {f(kc.get('cpu'), 1)}" + (" ≈" if kc.get("cpu_approx") else ""),
             "-", f(nc.get("cpu"), 1), f(tcc.get("cpu"), 1), "-", ratio(kc.get("cpu"), tcc.get("cpu"), 1) + (" (vs in-node)" if kc else "")])
kf = k8s_fail("1-replica-forced")
tf = t_fail("node-kill")
tt = t_fail("instance-trap")
rows.append(["Replica lost -> requests OK again, ms (k8s: pod force-deleted; k10s: instance trap / node SIGKILL)", f(g(kf, "first_success_after_last_fail_ms")), "-", "-",
             f(g(tt, "event_to_next_ok_ms"), 2) + " (trap)", f(g(tf, "first_success_after_event_ms")) + " (node kill)", ratio(g(kf, "first_success_after_last_fail_ms"), g(tf, "first_success_after_event_ms"), 0) + " (vs node kill)"])
nf = runs(K["nodefail-three"])
tsd = t_fail("node-stop")
kfs = med([r.get("node_notready_s") for r in nf])
rows.append(["Hung node detected (k8s NotReady / k10s declared dead), s", "-", f(kfs, 1), "-", "-", f(g(tsd, "dead_ms") and g(tsd, "dead_ms") / 1000, 2),
             ratio(kfs, g(tsd, "dead_ms") and g(tsd, "dead_ms") / 1000, 0)])
kfr = med([r.get("all_replicas_ready_on_healthy_nodes_s") for r in nf])
rows.append(["Hung node: workloads replaced elsewhere, s", "-", f(kfr, 0), "-", "-", f(g(tsd, "replaced_ms") and g(tsd, "replaced_ms") / 1000, 2), ratio(kfr, g(tsd, "replaced_ms") and g(tsd, "replaced_ms") / 1000, 0)])
kr = runs(K["rollout"])
tr1, tr3 = runs(T["rollout"], mode="single-8"), runs(T["rollout"], mode="cluster-3")
rows.append(["Rolling update (k8s 10 replicas; k10s 1 node / 3 nodes x 10 replicas): complete, s", f(g(kr, "rollout_status_ms") and g(kr, "rollout_status_ms") / 1000, 2), "-", "-",
             f(g(tr1, "new_version_everywhere_ms") and g(tr1, "new_version_everywhere_ms") / 1000, 3), f(g(tr3, "new_version_everywhere_ms") and g(tr3, "new_version_everywhere_ms") / 1000, 3),
             ratio(g(kr, "rollout_status_ms"), g(tr1, "new_version_everywhere_ms"), 0)])
rows.append(["Rolling update: failed probes (20 ms probe)", f(g(kr, "failed_after_event")), "-", "-", f(g(tr1, "failed_after_event")), f(g(tr3, "failed_after_event")), "-"])
ksz, tsz = (K["sizes"] or {}).get("runs", [{}])[0], (T["sizes"] or {}).get("runs", [{}])[0]
echo_img = (ksz.get("docker_images_bytes") or {}).get("echo:v1")
echo_mod = (tsz.get("modules_k10_bytes") or {}).get("echo")
rows.append(["echo artifact (container image / .k10 module), bytes", f(echo_img), "-", f((ksz.get("binaries_bytes") or {}).get("echo")) + " (binary)", f(echo_mod), "-", ratio(echo_img, echo_mod, 0)])
kb = sum((ksz.get("k8s_binaries_in_node_bytes") or {}).get(b, 0) for b in ("kubelet", "containerd", "runc")) or None
rows.append(["Node agent binaries: kubelet+containerd+runc / k10s (stripped), MB", f(kb and kb / 1e6, 1) + " (+ control-plane images)", "-", "-",
             f(tsz.get("k10s_binary_stripped_bytes") and tsz["k10s_binary_stripped_bytes"] / 1e6, 2), "-", ratio(kb, tsz.get("k10s_binary_stripped_bytes"), 0)])
P(table(["metric", "k8s 1 node (kind)", "k8s 3 nodes", "native processes", "k10s 1 node (8 shards)", "k10s 3 nodes", "ratio (k8s ÷ k10s; rps: k10s ÷ k8s)"], rows))
P("Ratios compare the k8s single-node figure (in-node loadgen for latency, which excludes Docker Desktop's port forwarder) with k10s single node "
  "unless noted. \"Replica\" means different things per system: see *What a per-replica number means* below.\n")

# ---------------- idle
P("## 1. Idle footprint\n")
rows = [["k8s single node (kind)", "0", f(ki.get("ws")), f(ki.get("anon")), f(ki.get("rss")), f(ki.get("pss")), f(ki.get("cpu") and ki["cpu"] * 100, 2)],
        ["k8s 3 nodes (1 control plane + 2 workers)", "0", f(ki3.get("ws")), f(ki3.get("anon")), f(ki3.get("rss")), f(ki3.get("pss")), f(ki3.get("cpu") and ki3["cpu"] * 100, 2)]]
for cfg, lbl in (("single-8", "k10s single node, 8 shards"), ("single-1", "k10s single node, 1 shard"), ("cluster-3", "k10s 3 nodes (2 shards each)")):
    for svc in (0, 4):
        x = t_idle(cfg, svc)
        if x:
            rows.append([lbl, "4 reference, 1 warm instance each" if svc else "0", f(x.get("ws"), 2), f(x.get("anon"), 2), f(x.get("rss"), 2), f(x.get("pss"), 2),
                         f(x.get("cpu") and x["cpu"] * 100, 3) + noisy_note(x["rs"])])
P(table(["system", "workloads", "working set MiB", "anon MiB (cgroup)", "RSS MiB (sum)", "PSS MiB (sum)", "idle CPU % of one core"], rows))
P("For k10s each configuration is one node start measured over 3 windows (stable within a start), but the footprint moves by up to ~2-3 MiB "
  "from one start to the next: anonymous memory (allocator arenas of the shard threads: 0.3-3 MiB observed across starts and binaries) and "
  "the page-cache state of the binary's text (file pages: active vs inactive LRU, fault-around). At k8s's scale this does not matter, but "
  "treat single-digit-MiB differences between k10s configurations as noise.\n")
P(f"k8s single node vs k10s single node (8 shards), no workloads: working set {ratio(ki.get('ws'), ti8.get('ws'), 0)}, PSS {ratio(ki.get('pss'), ti8.get('pss'), 0)}, "
  f"idle CPU {ratio(ki.get('cpu'), ti8.get('cpu'), 0)}. 3 nodes: working set {ratio(ki3.get('ws'), ti3.get('ws'), 0)}, CPU {ratio(ki3.get('cpu'), ti3.get('cpu'), 0)}. "
  "Bare processes have no platform: their fixed overhead is zero by definition (and they get no deployment, restart, routing or failover either).\n")

# ---------------- per workload
P("## 2. Per-workload overhead (echo)\n")
rows = []
for n in (1, 10, 50, 100, 1000, 10000, 100000):
    kp, nn, ti, ts = k8s_pods(n), nat_scale(n), t_ovh("instances", n), t_ovh("services", n)
    if not (kp or nn or ti or ts):
        continue
    rows.append([n, f(kp.get("ws")), f(kp.get("pss")), f(nn.get("rss")), f(nn.get("pss")), f(ti.get("ws"), 1), f(ti.get("pss"), 1), f(ts.get("ws"), 1),
                 f(ts.get("state_ws"), 1), f(ts.get("inst_ws"), 1), f(ts.get("pss"), 1)])
P(table(["N", "k8s pod: node working set KiB", "k8s pod: PSS KiB", "native process: RSS KiB", "native: PSS KiB", "k10s N instances of 1 service: ws KiB", "… PSS KiB",
         "k10s N services: ws KiB", "… of which service state", "… of which instance", "… PSS KiB"], rows))
P(f"At N=100: k8s pod {f(kp100.get('ws'))} KiB vs k10s service+instance {f(ts100.get('ws'), 1)} KiB ({ratio(kp100.get('ws'), ts100.get('ws'), 0)}) "
  f"and k10s instance of a shared service {f(ti100.get('ws'), 1)} KiB ({ratio(kp100.get('ws'), ti100.get('ws'), 0)}); a bare Go process {f(np100.get('pss'))} KiB PSS. "
  "Small N values are dominated by one-off costs (first touch of code paths, allocator arenas); N ≥ 100 is the steady per-unit cost. "
  "k8s stops at 100 (default max 110 pods per node); k10s continues to 100k.\n")
rows = []
for n in (1000, 10000, 100000):
    ts = t_ovh("services", n)
    if ts:
        rows.append([f"{n:,}", f(ts.get("ws"), 2), f(ts.get("ws") and ts["ws"] * n / 1024, 0), f(ts.get("apply_ms") and ts["apply_ms"] / 1000, 2), f(ts.get("cli_ms") and ts["cli_ms"] / 1000, 2)])
if rows:
    P("Density (k10s, N echo services with one instance each, one node):\n")
    P(table(["services", "working set KiB per service+instance", "total working-set delta MiB", "`k10s apply` s", "of which CLI-side s"], rows))

# ---------------- startup
P("## 3. Startup\n")
rows = []
for n in (1, 10, 50, 100, 1000):
    kp, nn, ts, tc3 = k8s_pods(n), nat_scale(n), t_start("single", n), t_start("cluster-3", n)
    if not (kp or nn or ts or tc3):
        continue
    rows.append([n, f(kp.get("ready_ms") and kp["ready_ms"] / 1000, 2), f(nn.get("ready_ms"), 1), f(ts.get("all"), 1) + noisy_note(ts.get("rs")), f(tc3.get("all"), 1)])
P(table(["N", "k8s: scale 0 -> N, all Ready s (no readiness probe)", "native: spawn N -> all answer ms", "k10s 1 node: apply N services -> all answer ms",
         "k10s 3 nodes: -> all answer on every node ms"], rows))
if ks:
    kr0 = runs(ks)
    P(f"One replica, k8s scale 0→1: scheduled {f(g(kr0, 'to_scheduled_ms'))} ms, Ready {f(g(kr0, 'to_ready_ms'))} ms, first HTTP 200 {f(g(kr0, 'to_first_http_ms'))} ms. ")
if tc:
    tk = t_cold("compute")
    P(f"k10s instance cold start (never-instantiated service, from a Python client): echo first request {f(tc.get('cold_keepalive_ms'), 3)} ms vs warm {f(tc.get('warm_keepalive_ms'), 3)} ms "
      f"on a kept-alive connection (difference {f(tc.get('cold_minus_warm_keepalive_ms'), 3)} ms; server-side init {f(tc.get('server_init_ms_per_instance') and tc['server_init_ms_per_instance'] * 1000, 1)} µs per instance); "
      f"compute {f(tk.get('cold_keepalive_ms'), 3)} vs {f(tk.get('warm_keepalive_ms'), 3)} ms. k10s has no separate \"scale from zero\": an instance is created by the request that needs it.\n")

# ---------------- latency
P("## 4. Request latency and throughput (closed loop, 20 s, same loadgen and flags)\n")
P("Columns: k8s `host` = loadgen on the host through Docker Desktop's port forwarder into the kind VM; k8s `in-node` = loadgen inside the kind node "
  "(node IP:NodePort, kube-proxy, no forwarder); native = bare Go processes on the host; k10s = `k10s node` on the host (8 shards); "
  "k10s 3-node = echo through a node that does not host it (one peer hop). Loadgen and server share the same 8 hardware threads in every case.\n")
rows = []
for tg in ("echo", "chain", "compute"):
    for c in (1, 16, 64):
        kh, kn, nn, ts = k8s_lat("host", tg, c), k8s_lat("node", tg, c), nat_lat(tg, c), t_lat("single-8", tg, c)
        tf3 = t_lat("cluster-3-forwarded", tg, c) if tg == "echo" else {}
        if not (kh or kn or nn or ts):
            continue
        cell = lambda o: f"{f(o.get('rps'))} / {f(o.get('p50'))} / {f(o.get('p99'))}" if o else "n/a"
        rows.append([tg, c, cell(kh), cell(kn), cell(nn), cell(ts), cell(tf3) if tg == "echo" else "-",
                     ratio(ts.get("rps"), nn.get("rps"), 2), ratio(ts.get("rps"), kn.get("rps"), 2)])
P(table(["target", "conns", "k8s host: rps / p50 / p99 µs", "k8s in-node", "native", "k10s 1 node", "k10s 3 nodes (forwarded)", "k10s ÷ native rps", "k10s ÷ k8s in-node rps"], rows))
def other_summary(d, mode="closed"):
    rs = [r for r in (d or {}).get("runs", []) if r.get("mode", "closed") == mode and r.get("other_cpu_cores") is not None]
    if not rs:
        return "not recorded"
    cells = {}
    for r in rs:
        cells.setdefault((r.get("place"), r["target"], r["conns"], r.get("target_rps")), []).append(r["other_cpu_cores"])
    picked = [min(v) for v in cells.values()]
    return f"all repeats median {med([r['other_cpu_cores'] for r in rs]):.2f} (max {max(r['other_cpu_cores'] for r in rs):.2f}); quietest repeat per cell median {med(picked):.2f} (max {max(picked):.2f})"
P(f"Cells show, for each configuration, the repeat with the least CPU used by other processes during its window (`COMPARE_STAT={STAT}`; "
  "`COMPARE_STAT=median` gives medians, as in each suite's SUMMARY.md). Foreign CPU, in cores (host busy - SUT - loadgen; for kind: host busy - "
  f"Docker VM - loadgen): k8s {other_summary(K['latency'])}; native {other_summary(N['latency'])}; k10s {other_summary(T['latency'])}.\n")
tl, tf_ = t_lat("cluster-3-local", "echo", 1), t_lat("cluster-3-forwarded", "echo", 1)
if tl:
    P(f"k10s 3 nodes, echo 1 conn: through the hosting node p50 {f(tl.get('p50'))} µs ({f(tl.get('rps'))} rps), through another node p50 {f(tf_.get('p50'))} µs ({f(tf_.get('rps'))} rps).\n")
P("`chain` = a frontend that calls echo for every request: k8s = Go HTTP client → ClusterIP (kube-proxy) → echo pod; native = Go HTTP client → echo over loopback; "
  "k10s = `call` host function (in-process, same node).\n")

P("### Language vs platform: compute (`?n=1000`, 1001 chained SHA-256)\n")
rows = []
for c in (1, 16, 64):
    na, npg, ts, kn = nat_lat("compute", c), nat_lat("compute-purego", c), t_lat("single-8", "compute", c), k8s_lat("node", "compute", c)
    if not (na or npg or ts or kn):
        continue
    rows.append([c, f"{f(na.get('rps'))} / {f(na.get('p50'))}", f"{f(npg.get('rps'))} / {f(npg.get('p50'))}", f"{f(ts.get('rps'))} / {f(ts.get('p50'))}",
                 f"{f(kn.get('rps'))} / {f(kn.get('p50'))}", ratio(na.get("rps"), npg.get("rps"), 2), ratio(npg.get("rps"), ts.get("rps"), 2)])
P(table(["conns", "Go native (asm SHA-256): rps / p50 µs", "Go native, -tags purego", "Ten on k10s (JIT)", "Go in k8s (in-node)", "asm ÷ purego", "Go purego ÷ Ten"], rows))
P("Go's crypto/sha256 uses hand-written assembly (SHA-NI/AVX2) unless built with `-tags purego`; Ten's SHA-256 is portable code. "
  "`Go purego ÷ Ten` isolates the code-generation gap (same algorithm in portable code), `asm ÷ purego` the hand-written-assembly advantage. "
  "At 1 connection compute is CPU-bound, so the platform overhead (µs) is negligible next to the hashing (~0.4-1 ms).\n")

P("## 5. Open loop (fixed request rate, 64 connections, 20 s): tail latency under the same load\n")
rows = []
for tg, rates in (("echo", (5000, 20000)), ("chain", (5000, 20000)), ("compute", (500, 2000))):
    for rate in rates:
        kh, kn, nn, ts = k8s_lat("host", tg, rate=rate), k8s_lat("node", tg, rate=rate), nat_lat(tg, rate=rate), t_lat("single-8", tg, rate=rate)
        if not (kh or kn or nn or ts):
            continue
        cell = lambda o: f"{f(o.get('rps'))} / {f(o.get('jc_p50'))} / {f(o.get('jc_p99'))} / {f(o.get('jc_p99.9'))}" if o else "n/a"
        rows.append([tg, f(rate), cell(kh), cell(kn), cell(nn), cell(ts), f(kn.get("cpu"), 1) if kn else "n/a", f(nn.get("cpu"), 1) if nn else "n/a", f(ts.get("cpu"), 1) if ts else "n/a"])
P(table(["target", "rate", "k8s host: achieved / p50 / p99 / p99.9 µs (jc)", "k8s in-node", "native", "k10s 1 node", "k8s in-node CPU µs/req", "native CPU µs/req", "k10s CPU µs/req"], rows))
P(f"Open-loop cells are medians of the repeats. Foreign CPU during the open-loop runs (cores): k8s {other_summary(K['latency-open'], 'open')}; native {other_summary(N['latency-open'], 'open')}; "
  f"k10s {other_summary(T['latency'], 'open')}.\n")
P("Latency is measured from the intended send time (no coordinated omission) and *jitter-corrected* (`jc`): when the loadgen's sender slept until the intended "
  "time, Go's timer wake-up lateness (median ~0.4 ms on this machine, reported as `wakeup_late_us`) is not charged to the system under test; when it was behind "
  "schedule (connection still busy), queueing is charged. Uncorrected percentiles are in the JSON (`latency_us`). If achieved < rate, the system saturated.\n")

P("## 6. CPU cost per request (system under test only)\n")
rows = []
for tg in ("echo", "chain", "compute"):
    for c in (1, 16, 64):
        kh, kn, nn, ts = k8s_lat("host", tg, c), k8s_lat("node", tg, c), nat_lat(tg, c), t_lat("single-8", tg, c)
        if not (kh or kn or nn or ts):
            continue
        kidle = ki.get("cpu")
        marg = (kn["cpu"] - kidle * 1e6 / kn["rps"]) if kn.get("cpu") is not None and kidle is not None and kn.get("rps") else None
        rows.append([tg, c, f(kh.get("cpu"), 1) + (" ≈" if kh.get("cpu_approx") else ""), f(kn.get("cpu"), 1) + (" ≈" if kn.get("cpu_approx") else ""), f(marg, 1),
                     f(nn.get("cpu"), 1), f(ts.get("cpu"), 1), ratio(kn.get("cpu"), ts.get("cpu"), 1), ratio(marg, ts.get("cpu"), 1), ratio(nn.get("cpu"), ts.get("cpu"), 1)])
P(table(["target", "conns", "k8s host µs/req", "k8s in-node µs/req", "k8s in-node, idle subtracted", "native µs/req", "k10s µs/req", "k8s in-node ÷ k10s",
         "(idle subtracted) ÷ k10s", "native ÷ k10s"], rows))
P(f"`idle subtracted` removes the kind node's idle CPU ({f(ki.get('cpu'), 3)} cores: control plane, kubelet…) spread over the requests of the window, "
  "i.e. the marginal data-path cost; k10s's idle CPU is negligible.\n")
P("k8s: kind node cgroup `cpu.stat` over the measured window (whole node: kubelet, apiserver, etcd… included; the in-node loadgen's own CPU subtracted; "
  "≈ = older runs without a window sample: node CPU averaged over warm-up + window). It does not include Docker Desktop's port forwarder or the VM's own kernel "
  "work outside the node cgroup, so `host` figures understate k8s's real cost. native: utime+stime of the server process(es) (`chain` = chain + echo). "
  "k10s: the node's cgroup. Busy-polling runtimes (Go's scheduler spins idle Ps; k10s shards do not spin by default) inflate CPU/request at low load.\n")

# ---------------- failover
P("## 7. Failover\n")
rows = []
for sc in ("1-replica-forced", "1-replica-graceful"):
    rs = k8s_fail(sc)
    if rs:
        rows.append([f"k8s: delete the pod ({sc})", f(g(rs, "first_success_after_last_fail_ms")), f(g(rs, "outage_window_ms")), f(g(rs, "failed_after_event")), "-"])
rs = k8s_fail("3-replicas-delete-1-under-300rps")
if rs:
    rows.append(["k8s: 3 replicas, delete 1 under 300 req/s", "-", "-", f"{f(g(rs, 'errors'))} errors / {f(g(rs, 'requests'))} req", "-"])
for sc, lbl in (("instance-trap", "k10s: an instance traps (request fails, instance destroyed)"), ("node-kill", "k10s 3 nodes: SIGKILL the node hosting a 1-replica service"),
                ("node-stop", "k10s 3 nodes: SIGSTOP (hung node) the host"), ("kill-1-of-2-under-300rps", "k10s 3 nodes: 2 replicas, SIGKILL one host under 300 req/s")):
    rs = t_fail(sc)
    if not rs:
        continue
    if sc.startswith("kill-1"):
        rows.append([lbl, "-", "-", f"{f(g(rs, 'errors'))} errors / {f(g(rs, 'requests'))} req", "-"])
    elif sc == "instance-trap":
        rows.append([lbl, f(g(rs, "event_to_next_ok_ms"), 2) + " (trap → next OK)", f(g(rs, "outage_window_ms")), f(g(rs, "failed_after_event")) + " + the trapping request", "-"])
    else:
        rows.append([lbl, f"{f(g(rs, 'first_success_after_event_ms'))} first / {f(med([r.get('first_success_after_last_fail_ms') or r.get('first_success_after_event_ms') for r in rs]))} 100%",
                     f(g(rs, "outage_window_ms")), f(g(rs, "failed_after_event")), f"suspected {f(g(rs, 'suspect_ms'))}, dead {f(g(rs, 'dead_ms'))}, re-placed {f(g(rs, 'replaced_ms'))}"])
P(table(["scenario", "event → requests OK again ms", "outage window ms", "failed probes (20 ms probe, 0.5 s timeout)", "membership"], rows))
if nf:
    P("Worker failure, k8s 3 nodes (default timers): " + "; ".join(
        f"{r['mode']}: node NotReady after {r.get('node_notready_s')} s, replacement pods Ready after {r.get('all_replicas_ready_on_healthy_nodes_s')} s" for r in nf) +
      ". k10s (default gossip: 200 ms probe period, suspicion ×4): a hung node is suspected (and avoided by routing) after "
      f"{f(g(tsd, 'suspect_ms'))} ms, declared dead after {f(g(tsd, 'dead_ms'))} ms, its services re-placed after {f(g(tsd, 'replaced_ms'))} ms.\n")
P("A failed probe that hangs costs its 0.5 s timeout before the next probe is sent (the probe is sequential, identical for both systems), "
  "so after a SIGSTOP the first success can be no earlier than ~0.5 s.\n")

# ---------------- rollout
P("## 8. Rolling update\n")
rows = []
if kr:
    rows.append(["k8s: 10-replica Deployment, image v1 ↔ v2 (no readiness probe)", f(g(kr, "rollout_status_ms") and g(kr, "rollout_status_ms") / 1000, 2) + " (`rollout status`)",
                 f(g(kr, "rollout_and_old_pods_gone_ms") and g(kr, "rollout_and_old_pods_gone_ms") / 1000, 2), f(g(kr, "failed_after_event")), "-"])
for rs, lbl in ((tr1, "k10s 1 node: re-apply (config v1 ↔ v2), + 1000 req/s"), (tr3, "k10s 3 nodes, replicas = 10 (on all 3), apply + probe via node 0, + 1000 req/s via node 1")):
    if rs:
        rows.append([lbl, f(g(rs, "new_version_everywhere_ms") and g(rs, "new_version_everywhere_ms") / 1000, 3) + " (every node answers v_new)",
                     f(g(rs, "old_version_gone_ms") and g(rs, "old_version_gone_ms") / 1000, 3), f(g(rs, "failed_after_event")), f(med([r["load"]["errors"] for r in rs])) + " of ~11k"])
P(table(["update", "complete s", "old version gone s", "failed probes", "load errors"], rows))

# ---------------- sizes
P("## 9. Artifact sizes\n")
rows = []
for w in ("echo", "chain", "compute", "counter"):
    rows.append([w, f((ksz.get("docker_images_bytes") or {}).get(f"{w}:v1")), f((ksz.get("binaries_bytes") or {}).get(w)), f((tsz.get("sources_ten_bytes") or {}).get(w)),
                 f((tsz.get("modules_k10_bytes") or {}).get(w)), f(((tsz.get("jit_code") or {}).get(w) or {}).get("code_bytes")),
                 ratio((ksz.get("docker_images_bytes") or {}).get(f"{w}:v1"), (tsz.get("modules_k10_bytes") or {}).get(w), 0)])
P(table(["workload", "container image B (FROM scratch)", "Go static binary B", "Ten source B", ".k10 module B", "JIT machine code B (on the node)", "image ÷ module"], rows))
rows = []
for b, v in (ksz.get("k8s_binaries_in_node_bytes") or {}).items():
    rows.append([f"k8s: {b}", f(v / 1e6, 1)])
if ksz:
    rows.append(["k8s: control-plane + addon images in the node (apiserver, etcd, controller-manager, scheduler, proxy, coredns, kindnetd, …)",
                 f(sum(v for k, v in ksz.get("images_in_node", {}).items() if "k10s-bench" not in k) / 1e6, 0)])
    rows.append(["k8s: kindest/node image", f(ksz.get("kind_node_image_bytes", 0) / 1e6, 0)])
if tsz:
    rows.append(["k10s: binary as built (release + debuginfo)", f(tsz["k10s_binary_bytes"] / 1e6, 2)])
    rows.append(["k10s: binary stripped (the whole node and control plane; dynamic deps: libc, libgcc_s)", f(tsz["k10s_binary_stripped_bytes"] / 1e6, 2)])
P(table(["component", "MB"], rows))

# ---------------- caveats
P("## Caveats (read before quoting)\n")
P("""- **Preliminary numbers on a shared machine.** Other agents ran Rust builds, QEMU guests and multi-hour soak tests during every run of this session
  (1-min load average 3-20 on 8 hardware threads; 1-7.5 foreign cores busy during latency windows, recorded per run as `other_cpu_cores`). Latency,
  throughput, tails and CPU-per-request are the most affected (the same configuration varied up to 3× between repeats); memory, startup, failover and
  rollout much less. The kind single-node/3-node suites (idle, pods, startup, failover, rollout, sizes) and native scale come from a morning session
  (load 2-9); all latency tables (k8s, native, k10s) from the evening of the same day. Final numbers must come from `bench/run-final.sh` on a quiet
  machine (it waits for a 1-min load average below 1.0 and < 1 busy core before each suite).
- **Docker Desktop.** Docker here is Docker Desktop: the kind nodes run inside a LinuxKit VM (kernel 6.10, 8 vCPU, 15.6 GiB) under QEMU/KVM, while native
  processes and k10s run directly on the host kernel (6.1). k8s memory/CPU are read from the node containers' cgroups inside the VM; the VM's own kernel,
  dockerd and the port forwarder are **not** counted (they would add to k8s). Host-side k8s latency crosses Docker Desktop's userspace port forwarder; the
  in-node loadgen column removes it but then the loadgen shares the VM's vCPUs. Neither is identical to a bare-metal kubelet; the in-node column is the
  fairer one for the data path, and the comparison of fixed overheads is, if anything, favourable to k8s.
- **Same definitions, different shapes.** Working set = cgroup `memory.current - inactive_file` for both (k10s nodes run in their own `systemd-run --user
  --scope` cgroup; the k10s binary's page cache is evicted before each start so its text is charged to the node, as a kind node's is). Shared libc pages
  are charged to whoever touched them first (not the k10s scope); PSS (proportional) is the cross-check. Idle windows: 60 s × 3, medians, as for k8s.
- **What a per-replica number means.** k8s: one pod = pause container + containerd-shim + the Go process + kubelet/apiserver/etcd bookkeeping, measured as the
  node-level delta / N 20 s after Ready. native: one Go process (RSS; PSS shares the binary's text). k10s: an *instance* is a memory slot (64 KiB here,
  pages touched only) plus runtime bookkeeping, running shared machine code; it exists only while there is demand (scale-to-one after 30 s idle by
  default). `N instances of 1 service` forces N concurrent instances with the `hold` module (echo that spins 50 ms; echo itself never needs more than one
  instance per shard) and includes up to 64 retained fiber stacks per shard; `N services` is N separately deployed echo services, one instance each
  (service state + instance). The k10s per-replica figures are per *idle* instance: concurrent requests add fiber stacks (touched pages of 1 MiB
  reserved stacks), not counted here.
- **Ten vs Go.** The k10s workloads are the Ten ports in `examples/` (same HTTP behaviour; `compute` is byte-identical). Ten's SHA-256 is portable code,
  Go's uses assembly: see the purego comparison. For `chain` the benchmark used `bench/k10s/ten/chain.ten` = `examples/chain.ten` plus `heap_reset()`:
  the example leaked one heap allocation per request (`read_config()`), so with 64 KiB of memory each instance trapped every ~4000 requests (HTTP 500,
  ~0.025 % errors; measured with both backends); once `examples/chain.ten` calls `heap_reset()` itself the harness uses the example (recorded as
  `env.chain_module`). The Go `chain` uses an HTTP client and a TCP connection pool; Ten's `call` is an in-process host call.
- **Loadgen.** The same `bench/bin/loadgen` (Go, raw sockets, keep-alive) drives every system from the same host and competes with the server for the 8
  hardware threads (~1-2.5 cores at high rates). Open-loop percentiles use the jitter-corrected histogram added to loadgen in this session (raw ones are
  kept). kind's tails also include the Docker Desktop VM's vCPU scheduling (its 8 vCPUs are threads on the same 8 host threads).
- **k10s cluster on one machine.** The 3 k10s nodes are processes on localhost (2 shards each, real UDP gossip and TCP peer links over loopback); the 3 kind
  nodes are containers in one VM. Neither measures a real network.
- **Probe semantics.** Failover/rollout probes are `k8s/probe.py` (fresh connection every 20 ms, 0.5 s timeout, sequential) for both systems; the
  k10s probe enters through a node that does not host the service (the k8s one through the NodePort).
- **k10s cold start** is measured client-side from Python (the floor is ~50 µs per request); the server-side instance creation is a few µs.
- **Startup.** k8s times start just before `kubectl scale` (no readiness probe, images preloaded); k10s times start just before `k10s apply` (modules
  precompiled to `.k10`, the analogue of preloaded images) and include process start of the CLI. Applying 100k services is dominated by the CLI's
  manifest parser (quadratic duplicate check, see below).
""")
P("## k10s problems found by the harness\n")
P("""- `examples/chain.ten` leaked one heap allocation per request (`read_config()` without `heap_reset()`/`free`): with `memory = 64KiB` every instance
  traps (`Unreachable` from `alloc`) after ~4000 requests, the request fails with HTTP 500 and the instance is replaced (48-50 traps per 200k requests,
  JIT and interpreter alike). The harness runs a fixed copy until the example is fixed.
- `k10s apply` is quadratic in the number of services on the CLI side: `k10_mesh::manifest::parse_text` checks duplicate names with a linear scan
  (`out.iter().any(|d| d.name == name)`), so parsing alone takes ~0.3-0.7 s for 10k services and ~40-65 s for 100k (the node-side apply is a few
  seconds). Reported by `overhead.sh` as `apply_cli_only_ms`.
""")
P("## What k10s does not provide (yet) that Kubernetes does\n")
P("""Not measured because absent in k10s: arbitrary Linux binaries/containers (k10s runs only TIR modules, e.g. compiled from Ten); a filesystem, raw
sockets or outbound network for workloads (only the host API: HTTP request/response, `call`, node-local KV, logs, time, randomness); persistent volumes and
replicated storage (KV is node-local, asynchronously durable); TLS, authentication/authorization, RBAC, admission control, network policies, secrets;
an API server with watch semantics, CRDs/operators and the ecosystem (Helm, service meshes, observability integrations); readiness/liveness probes and
init containers (k10s health is direct observation of traps); jobs/cron jobs/daemon sets/stateful sets; consensus on the desired state (concurrent
applies resolve by version+hash; gossip and peer links are unauthenticated); multi-tenant resource quotas beyond per-request CPU budgets, memory slots and the per-service KV quota;
node-process supervision (a crashed single node is restarted by systemd, not by k10s); horizontal autoscaling is implicit (instances on demand), node
autoscaling absent. Conversely, bare processes provide none of the orchestration (deployment, routing, restart, failover, rolling update).
""")
open(OUT, "w").write("\n".join(D))
print(f"wrote {OUT}")
