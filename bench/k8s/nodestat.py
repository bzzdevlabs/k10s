#!/usr/bin/env python3
"""Helpers for k8s/native benchmarks. Subcommands:
  snap <container>...      JSON snapshot: cgroup memory/cpu + per-process rss/pss/cpu ticks inside each container
  mem  <container>...      compact JSON: summed working set (bytes)
  diff <snapA> <snapB>     per-component and cgroup deltas
  stats <json-list>        median/min/max of a list of numbers
"""
import json, subprocess, sys, statistics, time, re, collections

INNER = r'''for d in /proc/[0-9]*; do [ -r $d/smaps_rollup ] || continue; c=$(cat $d/comm 2>/dev/null) || continue;
a=$(awk '/^Rss:/{r=$2}/^Pss:/{p=$2}END{print r+0,p+0}' $d/smaps_rollup 2>/dev/null) || continue;
t=$(awk '{print $14+$15}' $d/stat 2>/dev/null); echo "${d#/proc/} $a $t $c"; done'''

def sh(*a, check=True):
    return subprocess.run(a, capture_output=True, text=True, check=check).stdout

def snap_one(c):
    # Docker Desktop: containers live in a VM, host cgroups are invisible -> read the node's own cgroup root from inside.
    cg = ("echo @@MEM; cat /sys/fs/cgroup/memory.current; echo @@MS; cat /sys/fs/cgroup/memory.stat; "
          "echo @@CPU; cat /sys/fs/cgroup/cpu.stat; echo @@PROCS; ")
    out = sh("docker", "exec", c, "sh", "-c", cg + INNER)
    mem_s, rest = out.split("@@MS\n", 1)[0].split("@@MEM\n", 1)[1], out.split("@@MS\n", 1)[1]
    ms_s, rest = rest.split("@@CPU\n", 1)
    cpu_s, procs_s = rest.split("@@PROCS\n", 1)
    mem = int(mem_s)
    ms = {l.split()[0]: int(l.split()[1]) for l in ms_s.splitlines() if l.strip()}
    cpu = {l.split()[0]: int(l.split()[1]) for l in cpu_s.splitlines() if l.strip()}
    procs = []
    for l in procs_s.splitlines():
        f = l.split(None, 4)
        if len(f) == 5:
            procs.append({"pid": int(f[0]), "rss_kb": int(f[1]), "pss_kb": int(f[2]), "ticks": int(f[3]), "comm": f[4]})
    return {"mem_current": mem, "inactive_file": ms.get("inactive_file", 0), "anon": ms.get("anon", 0),
            "file": ms.get("file", 0), "slab": ms.get("slab", 0), "kernel": ms.get("kernel", 0),
            "working_set": mem - ms.get("inactive_file", 0),
            "cpu_usec": cpu["usage_usec"], "procs": procs, "t": time.time()}

def snap(containers):
    return {c: snap_one(c) for c in containers}

def group(comm):
    if comm.startswith("containerd-shim"): return "containerd-shim"
    if comm in ("kube-apiserver", "kube-controller", "kube-controller-manager"): return "kube-controller-manager" if comm != "kube-apiserver" else comm
    if comm.startswith("kube-controller"): return "kube-controller-manager"
    if comm.startswith("kube-schedul"): return "kube-scheduler"
    if comm.startswith("local-path"): return "local-path-provisioner"
    return comm

def by_comm(s):
    """aggregate rss/pss (kB) and process counts per component across containers"""
    agg = collections.defaultdict(lambda: {"rss_kb": 0, "pss_kb": 0, "n": 0})
    for c, v in s.items():
        for p in v["procs"]:
            a = agg[group(p["comm"])]
            a["rss_kb"] += p["rss_kb"]; a["pss_kb"] += p["pss_kb"]; a["n"] += 1
    return dict(agg)

def cpu_by_comm(a, b):
    out = collections.defaultdict(float)
    for c in b:
        ta = {p["pid"]: p for p in a.get(c, {"procs": []})["procs"]}
        for p in b[c]["procs"]:
            q = ta.get(p["pid"])
            if q and q["comm"] == p["comm"]:
                out[group(p["comm"])] += (p["ticks"] - q["ticks"]) / 100.0
    return dict(out)

def totals(s):
    return {"working_set_bytes": sum(v["working_set"] for v in s.values()),
            "mem_current_bytes": sum(v["mem_current"] for v in s.values()),
            "anon_bytes": sum(v["anon"] for v in s.values()),
            "kernel_bytes": sum(v["kernel"] for v in s.values()),
            "rss_sum_kb": sum(p["rss_kb"] for v in s.values() for p in v["procs"]),
            "pss_sum_kb": sum(p["pss_kb"] for v in s.values() for p in v["procs"])}

def median_stats(xs):
    xs = [float(x) for x in xs]
    return {"median": statistics.median(xs), "min": min(xs), "max": max(xs), "n": len(xs), "values": xs}

# ---------------- reports ----------------
def _load(p):
    return json.load(open(p))

def env_report(cluster, kubeconfig, node_image):
    def run(*a):
        r = subprocess.run(a, capture_output=True, text=True)
        return (r.stdout or r.stderr).strip()
    info = json.loads(run("docker", "info", "--format", "{{json .}}") or "{}")
    cpu = ""
    for l in open("/proc/cpuinfo"):
        if l.startswith("model name"):
            cpu = l.split(":", 1)[1].strip(); break
    e = {"date": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "host_kernel": run("uname", "-r"), "host_cpu": cpu,
         "host_threads": __import__("os").cpu_count(), "host_mem_gib": round(int(open("/proc/meminfo").readline().split()[1]) / 1048576, 1),
         "docker_server": info.get("ServerVersion"), "docker_os": info.get("OperatingSystem"),
         "docker_vm_kernel": info.get("KernelVersion"), "docker_vm_cpus": info.get("NCPU"),
         "docker_vm_mem_gib": round(info.get("MemTotal", 0) / 2**30, 2),
         "kind": run("kind", "version"), "kubectl_client": run("kubectl", "version", "--client", "-o", "json"),
         "node_image": node_image, "cluster": cluster}
    try: e["kubectl_client"] = json.loads(e["kubectl_client"])["clientVersion"]["gitVersion"]
    except Exception: pass
    try:
        n = json.loads(run("kubectl", "--kubeconfig", kubeconfig, "--context", "kind-" + cluster, "get", "nodes", "-o", "json"))
        ni = n["items"][0]["status"]["nodeInfo"]
        e.update({"k8s_version": ni["kubeletVersion"], "node_kernel": ni["kernelVersion"], "container_runtime": ni["containerRuntimeVersion"],
                  "nodes": len(n["items"]), "max_pods": n["items"][0]["status"]["capacity"]["pods"]})
    except Exception as ex:
        e["k8s_error"] = str(ex)
    return e

def idle_report(a, b):
    dt = max(b[c]["t"] for c in b) - min(a[c]["t"] for c in a)
    nodes = {}
    for c in b:
        cores = (b[c]["cpu_usec"] - a[c]["cpu_usec"]) / 1e6 / (b[c]["t"] - a[c]["t"])
        nodes[c] = {"working_set_mib": b[c]["working_set"] / 2**20, "mem_current_mib": b[c]["mem_current"] / 2**20,
                    "anon_mib": b[c]["anon"] / 2**20, "kernel_mib": b[c]["kernel"] / 2**20, "cpu_cores": cores}
    cpu_comp = cpu_by_comm(a, b)
    comp = {k: {"rss_mib": v["rss_kb"] / 1024, "pss_mib": v["pss_kb"] / 1024, "procs": v["n"],
                "cpu_core_pct": 100 * cpu_comp.get(k, 0) / dt} for k, v in by_comm(b).items()}
    tot = totals(b)
    return {"window_s": dt, "cluster_working_set_mib": tot["working_set_bytes"] / 2**20,
            "cluster_mem_current_mib": tot["mem_current_bytes"] / 2**20,
            "cluster_cpu_cores": sum(n["cpu_cores"] for n in nodes.values()),
            "rss_sum_mib": tot["rss_sum_kb"] / 1024, "pss_sum_mib": tot["pss_sum_kb"] / 1024,
            "nodes": nodes, "components": comp}

def pods_report(a, b, n):
    ga, gb = by_comm(a), by_comm(b)
    ta, tb = totals(a), totals(b)
    d = {}
    for k in set(ga) | set(gb):
        x = ga.get(k, {"rss_kb": 0, "pss_kb": 0, "n": 0}); y = gb.get(k, {"rss_kb": 0, "pss_kb": 0, "n": 0})
        if y["rss_kb"] - x["rss_kb"] or y["n"] - x["n"]:
            d[k] = {"rss_kib_per_pod": (y["rss_kb"] - x["rss_kb"]) / n, "pss_kib_per_pod": (y["pss_kb"] - x["pss_kb"]) / n,
                    "procs_delta": y["n"] - x["n"]}
    return {"n": n, "working_set_delta_mib": (tb["working_set_bytes"] - ta["working_set_bytes"]) / 2**20,
            "working_set_kib_per_pod": (tb["working_set_bytes"] - ta["working_set_bytes"]) / 1024 / n,
            "mem_current_kib_per_pod": (tb["mem_current_bytes"] - ta["mem_current_bytes"]) / 1024 / n,
            "kernel_kib_per_pod": (tb["kernel_bytes"] - ta["kernel_bytes"]) / 1024 / n,
            "rss_sum_kib_per_pod": (tb["rss_sum_kb"] - ta["rss_sum_kb"]) / n,
            "pss_sum_kib_per_pod": (tb["pss_sum_kb"] - ta["pss_sum_kb"]) / n,
            "per_component": d}

def wrap(name, envf, runsf):
    runs = [json.loads(l) for l in open(runsf) if l.strip()]
    return {"experiment": name, "env": _load(envf), "runs": runs}

if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "snap":
        print(json.dumps(snap(sys.argv[2:])))
    elif cmd == "env":
        print(json.dumps(env_report(*sys.argv[2:5])))
    elif cmd == "idle":
        print(json.dumps(idle_report(_load(sys.argv[2]), _load(sys.argv[3]))))
    elif cmd == "pods":
        print(json.dumps(pods_report(_load(sys.argv[2]), _load(sys.argv[3]), int(sys.argv[4]))))
    elif cmd == "wrap":
        print(json.dumps(wrap(*sys.argv[2:5]), indent=1))
    elif cmd == "cpu":
        print(sh("docker", "exec", sys.argv[2], "sh", "-c", "awk '/usage_usec/{print $2}' /sys/fs/cgroup/cpu.stat").strip())
    elif cmd == "stats":
        print(json.dumps(median_stats(json.loads(sys.argv[2]))))


