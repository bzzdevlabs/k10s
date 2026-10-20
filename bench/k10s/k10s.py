#!/usr/bin/env python3
"""Helpers for the k10s benchmarks (bench/k10s/*.sh). Same definitions as bench/k8s/nodestat.py where they overlap:
working set = cgroup memory.current - inactive_file, RSS/PSS from /proc/<pid>/smaps_rollup, CPU from the cgroup's
cpu.stat (every k10s node runs in its own `systemd-run --user --scope` cgroup) or /proc/<pid>/stat ticks.

Subcommands (all print JSON on stdout):
  env <k10s-bin>                         host + binary description (incl. load average)
  loadavg                                [1m, 5m, 15m]
  snap <pid,pid,...> [addr,addr,...]     per-process memory/CPU (+ cgroup, + /_k10s/status memory/exec if addrs)
  idle <snapA> <snapB>                   idle report between two snapshots (window, RSS/PSS/working set, CPU cores)
  delta <snapA> <snapB> <n>              per-unit memory delta (RSS/PSS/working set/status RSS per unit)
  cpu <pid,pid,...>                      summed CPU µs (cgroup usage_usec if in a k10sbench scope, else ticks)
  manifest <prefix> <n> <code> [key=value ...]   manifest text: services <prefix>0..<prefix>(n-1) (calls=a,b -> calls = ["a", "b"])
  get-many <addr> <paths-file|@prefix:n> [conns] [depth]   pipelined keep-alive GETs, status histogram
  serve-time <k10s-bin> <manifest> <addr,...> <prefix> <n> [timeout_s]   apply -> every service answers 200 on every addr
  cold <addr> <prefix> <n> [query]       first request to never-instantiated services vs. second request (fresh conns)
  status <addr>                          /_k10s/status
  wrap <name> <env.json> <runs.jsonl>    {"experiment", "env", "runs"} like nodestat.py wrap
"""
import json, os, platform, socket, statistics, subprocess, sys, threading, time

TCK = os.sysconf("SC_CLK_TCK")


def loadavg():
    return [float(x) for x in open("/proc/loadavg").read().split()[:3]]


def env(binary):
    cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "")
    e = {"date": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "host_kernel": platform.release(), "host_cpu": cpu,
         "host_threads": os.cpu_count(), "host_mem_gib": round(int(open("/proc/meminfo").readline().split()[1]) / 1048576, 1),
         "loadavg": loadavg(), "k10s_binary": binary}
    try:
        e["k10s_binary_bytes"] = os.path.getsize(binary)
        e["k10s_build"] = json.load(open(binary + ".info"))
    except (OSError, ValueError):
        pass
    e["note"] = "k10s node processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)"
    return e


# ---------------- process / cgroup sampling ----------------
def cgroup_dir(pid):
    try:
        p = open(f"/proc/{pid}/cgroup").read().strip().split("::", 1)[1]
    except (OSError, IndexError):
        return None
    if "k10sbench" not in p:
        return None
    return "/sys/fs/cgroup" + p


def read_kv(path):
    out = {}
    for l in open(path):
        f = l.split()
        if len(f) == 2:
            out[f[0]] = int(f[1])
    return out


def proc_snap(pid):
    sm = {}
    for l in open(f"/proc/{pid}/smaps_rollup"):
        f = l.split()
        if len(f) >= 2 and f[0].endswith(":") and f[1].isdigit():
            sm[f[0][:-1]] = int(f[1])
    st = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    s = {"pid": pid, "rss_kb": sm.get("Rss", 0), "pss_kb": sm.get("Pss", 0), "pss_anon_kb": sm.get("Pss_Anon", 0),
         "pss_file_kb": sm.get("Pss_File", 0), "ticks": int(st[11]) + int(st[12]), "threads": int(st[17])}
    try:
        s["vmas"] = sum(1 for _ in open(f"/proc/{pid}/maps"))
    except OSError:
        pass
    cg = cgroup_dir(pid)
    if cg:
        ms = read_kv(cg + "/memory.stat")
        cur = int(open(cg + "/memory.current").read())
        s["cg"] = {"mem_current": cur, "inactive_file": ms.get("inactive_file", 0), "anon": ms.get("anon", 0),
                   "file": ms.get("file", 0), "kernel": ms.get("kernel", ms.get("kernel_stack", 0) + ms.get("slab", 0) + ms.get("pagetables", 0)),
                   "pagetables": ms.get("pagetables", 0), "slab": ms.get("slab", 0), "sock": ms.get("sock", 0),
                   "working_set": cur - ms.get("inactive_file", 0), "cpu_usec": read_kv(cg + "/cpu.stat")["usage_usec"]}
    return s


def cpu_us(pids):
    tot = 0
    for pid in pids:
        cg = cgroup_dir(pid)
        if cg:
            tot += read_kv(cg + "/cpu.stat")["usage_usec"]
        else:
            st = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            tot += (int(st[11]) + int(st[12])) * 1_000_000 // TCK
    return tot


def host_busy_jiffies():
    """busy jiffies of the whole host (all CPUs): total - idle - iowait, from /proc/stat"""
    f = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    return sum(f[:8]) - f[3] - f[4]


def http_get(addr, path, timeout=5.0, method="GET", body=b""):
    host, port = addr.rsplit(":", 1)
    s = socket.create_connection((host, int(port)), timeout=timeout)
    try:
        s.settimeout(timeout)
        s.sendall(f"{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
        d = b""
        while True:
            c = s.recv(65536)
            if not c:
                break
            d += c
    finally:
        s.close()
    head, _, b = d.partition(b"\r\n\r\n")
    return int(head[9:12]), b


def status(addr):
    st, b = http_get(addr, "/_k10s/status", 10)
    return json.loads(b) if st == 200 else {}


def snap(pids, addrs=()):
    out = {"t": time.time(), "loadavg": loadavg(), "procs": [proc_snap(p) for p in pids]}
    if addrs:
        sts = []
        for a in addrs:
            try:
                s = status(a)
            except OSError:
                s = {}
            sv = s.get("services", [])
            sts.append({"addr": a, "memory": s.get("memory"), "exec": s.get("exec"), "code": s.get("code"),
                        "services": len(sv), "instances_live": sum(x["instances"]["live"] for x in sv),
                        "instances_idle": sum(x["instances"]["idle"] for x in sv)})
        out["status"] = sts
    return out


def totals(s):
    p = s["procs"]
    t = {"rss_kb": sum(x["rss_kb"] for x in p), "pss_kb": sum(x["pss_kb"] for x in p), "threads": sum(x["threads"] for x in p)}
    if all("cg" in x for x in p):
        for k in ("working_set", "mem_current", "anon", "kernel", "pagetables", "slab", "sock", "file"):
            t[k] = sum(x["cg"][k] for x in p)
    if s.get("status"):
        t["status_rss_bytes"] = sum((x["memory"] or {}).get("rss_bytes", 0) for x in s["status"])
        t["instances_live"] = sum(x["instances_live"] for x in s["status"])
    return t


def idle_report(a, b):
    dt = b["t"] - a["t"]
    nodes = []
    for pa, pb in zip(a["procs"], b["procs"]):
        n = {"pid": pb["pid"], "rss_mib": pb["rss_kb"] / 1024, "pss_mib": pb["pss_kb"] / 1024, "threads": pb["threads"],
             "pss_anon_mib": pb.get("pss_anon_kb", 0) / 1024, "pss_file_mib": pb.get("pss_file_kb", 0) / 1024,
             "cpu_cores_ticks": (pb["ticks"] - pa["ticks"]) / TCK / dt}
        if "cg" in pb:
            n.update({"cg_anon_mib": pb["cg"]["anon"] / 2**20, "cg_file_mib": pb["cg"]["file"] / 2**20})
        if "cg" in pb:
            n.update({"working_set_mib": pb["cg"]["working_set"] / 2**20, "mem_current_mib": pb["cg"]["mem_current"] / 2**20,
                      "kernel_mib": pb["cg"]["kernel"] / 2**20, "cpu_cores": (pb["cg"]["cpu_usec"] - pa["cg"]["cpu_usec"]) / 1e6 / dt})
        else:
            n["cpu_cores"] = n["cpu_cores_ticks"]
        nodes.append(n)
    r = {"window_s": dt, "nodes": nodes, "rss_sum_mib": sum(n["rss_mib"] for n in nodes), "pss_sum_mib": sum(n["pss_mib"] for n in nodes),
         "pss_anon_sum_mib": sum(n["pss_anon_mib"] for n in nodes), "pss_file_sum_mib": sum(n["pss_file_mib"] for n in nodes),
         "cpu_cores": sum(n["cpu_cores"] for n in nodes), "loadavg_start": a["loadavg"], "loadavg_end": b["loadavg"]}
    if all("working_set_mib" in n for n in nodes):
        r["working_set_mib"] = sum(n["working_set_mib"] for n in nodes)
        r["mem_current_mib"] = sum(n["mem_current_mib"] for n in nodes)
    if b.get("status"):
        r["status"] = b["status"]
    return r


def delta_report(a, b, n):
    ta, tb = totals(a), totals(b)
    r = {"n": n, "rss_kib_per_unit": (tb["rss_kb"] - ta["rss_kb"]) / n, "pss_kib_per_unit": (tb["pss_kb"] - ta["pss_kb"]) / n,
         "rss_before_mib": ta["rss_kb"] / 1024, "rss_after_mib": tb["rss_kb"] / 1024,
         "pss_before_mib": ta["pss_kb"] / 1024, "pss_after_mib": tb["pss_kb"] / 1024, "threads_after": tb["threads"],
         "loadavg_start": a["loadavg"], "loadavg_end": b["loadavg"]}
    if "working_set" in ta and "working_set" in tb:
        r.update({"working_set_kib_per_unit": (tb["working_set"] - ta["working_set"]) / 1024 / n,
                  "mem_current_kib_per_unit": (tb["mem_current"] - ta["mem_current"]) / 1024 / n,
                  "kernel_kib_per_unit": (tb["kernel"] - ta["kernel"]) / 1024 / n,
                  "pagetables_kib_per_unit": (tb["pagetables"] - ta["pagetables"]) / 1024 / n,
                  "working_set_before_mib": ta["working_set"] / 2**20, "working_set_after_mib": tb["working_set"] / 2**20})
    if "status_rss_bytes" in tb:
        r["instances_live_after"] = tb["instances_live"]
        r["status_rss_kib_per_unit"] = (tb["status_rss_bytes"] - ta.get("status_rss_bytes", 0)) / 1024 / n
    if b.get("status"):
        r["status_after"] = b["status"]
    return r


# ---------------- HTTP: pipelined keep-alive GETs ----------------
def _read_resp(s, buf):
    while True:
        i = buf.find(b"\r\n\r\n")
        if i >= 0:
            break
        c = s.recv(262144)
        if not c:
            raise ConnectionError("closed")
        buf += c
    head = buf[:i]
    rest = buf[i + 4:]
    st = int(head[9:12])
    clen = 0
    for l in head.split(b"\r\n")[1:]:
        k, _, v = l.partition(b":")
        if k.strip().lower() == b"content-length":
            clen = int(v.strip())
    while len(rest) < clen:
        c = s.recv(262144)
        if not c:
            raise ConnectionError("closed")
        rest += c
    return st, rest[:clen], rest[clen:]


def _worker(addr, paths, out, idx, depth, bodies):
    host, port = addr.rsplit(":", 1)
    i = 0
    last_fail = -1
    while i < len(paths):
        s = None
        try:
            s = socket.create_connection((host, int(port)), timeout=10)
            s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            buf = b""
            while i < len(paths):
                batch = paths[i:i + depth]
                s.sendall(b"".join(f"GET {p} HTTP/1.1\r\nHost: {addr}\r\n\r\n".encode() for p in batch))
                for _ in range(len(batch)):
                    st, body, buf = _read_resp(s, buf)
                    out[idx[i]] = st
                    if bodies is not None:
                        bodies[idx[i]] = body
                    i += 1
            s.close()
        except OSError:
            # connection lost (e.g. a `Connection: close` answer): resend from the first unanswered request on a new
            # connection; a request that fails twice in a row is recorded as 0 and skipped
            if s is not None:
                s.close()
            if last_fail == i:
                out[idx[i]] = 0
                i += 1
            last_fail = i


def get_many(addr, paths, conns=8, depth=64, want_bodies=False):
    out = [None] * len(paths)
    bodies = [None] * len(paths) if want_bodies else None
    conns = max(1, min(conns, len(paths)))
    ths = []
    for c in range(conns):
        idx = list(range(c, len(paths), conns))
        ths.append(threading.Thread(target=_worker, args=(addr, [paths[k] for k in idx], out, idx, depth, bodies)))
    for t in ths:
        t.start()
    for t in ths:
        t.join()
    out = [0 if x is None else x for x in out]
    return (out, bodies) if want_bodies else out


def paths_arg(a):
    if a.startswith("@"):
        pre, n = a[1:].rsplit(":", 1)
        return [f"/{pre}{i}" for i in range(int(n))]
    return [l.strip() for l in open(a) if l.strip()]


def hist(xs):
    h = {}
    for x in xs:
        h[str(x)] = h.get(str(x), 0) + 1
    return h


def serve_time(binary, manifest, addrs, prefix, n, timeout=300.0, expect=None):
    """t0 -> `k10s apply` -> poll every service on every addr (10 ms between sweeps) until all answer 200."""
    paths = [f"/{prefix}{i}" for i in range(n)]
    pending = {a: list(paths) for a in addrs}
    t0 = time.monotonic()
    r = subprocess.run([binary, "apply", manifest, "--node", addrs[0]], capture_output=True, text=True)
    t_apply = time.monotonic() - t0
    if r.returncode != 0:
        return {"error": (r.stdout + r.stderr)[-500:], "apply_cmd_ms": t_apply * 1000}
    first = None
    sweeps = 0
    per_addr = {}
    while any(pending.values()) and time.monotonic() - t0 < timeout:
        sweeps += 1
        for a in addrs:
            if not pending[a]:
                continue
            res, bodies = get_many(a, pending[a], conns=8, depth=64, want_bodies=True)
            ok = [p for p, s, b in zip(pending[a], res, bodies) if s == 200 and (expect is None or expect.encode() in (b or b""))]
            if ok and first is None:
                first = time.monotonic() - t0
            okset = set(ok)
            pending[a] = [p for p in pending[a] if p not in okset]
            if not pending[a]:
                per_addr[a] = (time.monotonic() - t0) * 1000
        if any(pending.values()):
            time.sleep(0.01)
    t_all = time.monotonic() - t0
    return {"n": n, "apply_cmd_ms": t_apply * 1000, "first_ok_ms": (first or 0) * 1000, "all_serving_ms": t_all * 1000,
            "per_addr_ms": per_addr, "not_serving": sum(len(v) for v in pending.values()), "sweeps": sweeps,
            "apply_out": r.stdout.strip()[:300]}


def cold(addr, prefix, n, query=""):
    """For each never-instantiated service: first GET (cold: instance created) then a second GET (warm), both on fresh
    connections from this client; also a warm GET on a kept-alive connection as the floor."""
    rows = []
    for i in range(n):
        p = f"/{prefix}{i}{query}"
        t = time.perf_counter(); s1, _ = http_get(addr, p); c = time.perf_counter() - t
        t = time.perf_counter(); s2, _ = http_get(addr, p); w = time.perf_counter() - t
        rows.append({"svc": f"{prefix}{i}", "cold_ms": c * 1000, "warm_ms": w * 1000, "status": [s1, s2]})
    # same on one kept-alive connection (no TCP connect in the measurement): the server-side cold-start cost
    host, port = addr.rsplit(":", 1)
    k = socket.create_connection((host, int(port)), timeout=10)
    k.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    buf = b""
    for i, r in enumerate(rows):
        p = f"/{prefix}{i}{query}".replace(f"/{prefix}", f"/{prefix}ka", 1)
        for key in ("cold_ka_ms", "warm_ka_ms"):
            t = time.perf_counter()
            k.sendall(f"GET {p} HTTP/1.1\r\nHost: {addr}\r\n\r\n".encode())
            st, _, buf = _read_resp(k, buf)
            r[key] = (time.perf_counter() - t) * 1000
            r["status"].append(st)
    k.close()
    cs = [r["cold_ms"] for r in rows]; ws = [r["warm_ms"] for r in rows]
    return {"n": n, "query": query, "cold_ms": med(cs), "warm_ms": med(ws), "cold_minus_warm_ms": med([r["cold_ms"] - r["warm_ms"] for r in rows]),
            "cold_ms_min_max": [min(cs), max(cs)], "warm_ms_min_max": [min(ws), max(ws)],
            "cold_keepalive_ms": med([r["cold_ka_ms"] for r in rows]), "warm_keepalive_ms": med([r["warm_ka_ms"] for r in rows]),
            "cold_minus_warm_keepalive_ms": med([r["cold_ka_ms"] - r["warm_ka_ms"] for r in rows]),
            "errors": sum(1 for r in rows if r["status"] != [200] * 4), "rows": rows}


def burst(addr, path, n, timeout=120.0):
    """n concurrent requests (one fresh connection each, all sent before any response is read)."""
    host, port = addr.rsplit(":", 1)
    t0 = time.monotonic()
    socks = []
    req = f"GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n".encode()
    for _ in range(n):   # connect everything first, then send all requests back to back
        socks.append(socket.create_connection((host, int(port)), timeout=timeout))
    t_conn = time.monotonic() - t0
    for s in socks:
        s.sendall(req)
    t_sent = time.monotonic() - t0 - t_conn
    sts = []
    for s in socks:
        d = b""
        try:
            while True:
                c = s.recv(65536)
                if not c:
                    break
                d += c
            sts.append(int(d[9:12]) if len(d) >= 12 else 0)
        except OSError:
            sts.append(0)
        s.close()
    return {"n": n, "connect_s": t_conn, "send_s": t_sent, "elapsed_s": time.monotonic() - t0, "status": hist(sts)}


def watch_failover(addr, victim_id, svc, t0_ms, timeout=60.0):
    """Polls a surviving node's status every 20 ms: when the victim is suspected / declared dead, and when `svc` is no
    longer placed on it (re-placed). Times in ms since t0_ms (epoch ms of the signal)."""
    out = {"suspect_ms": None, "dead_ms": None, "replaced_ms": None}
    end = time.time() + timeout
    while time.time() < end and None in out.values():
        try:
            s = status(addr)
        except OSError:
            time.sleep(0.02)
            continue
        now = time.time() * 1000 - t0_ms
        c = s.get("cluster", {})
        st = next((m.get("state") for m in c.get("members", []) if m.get("id") == victim_id), None)
        if st in ("suspect", "dead") and out["suspect_ms"] is None:
            out["suspect_ms"] = now
        if st == "dead" and out["dead_ms"] is None:
            out["dead_ms"] = now
        if victim_id not in (c.get("placement", {}).get(svc) or [victim_id]) and out["replaced_ms"] is None:
            out["replaced_ms"] = now
        time.sleep(0.02)
    return out


def rollout(binary, manifest, addrs, path, expect, timeout=120.0):
    """t0 -> `k10s apply` (new version) -> every addr answers a body containing `expect` (polled every 5 ms) -> no
    retired (old-version) service left in any node's status."""
    t0 = time.monotonic()
    r = subprocess.run([binary, "apply", manifest, "--node", addrs[0]], capture_output=True, text=True)
    t_apply = (time.monotonic() - t0) * 1000
    per = {}
    pending = list(addrs)
    while pending and time.monotonic() - t0 < timeout:
        for a in list(pending):
            try:
                st, b = http_get(a, path, 2)
            except OSError:
                continue
            if st == 200 and expect.encode() in b:
                per[a] = (time.monotonic() - t0) * 1000
                pending.remove(a)
        time.sleep(0.005)
    t_new = (time.monotonic() - t0) * 1000
    while time.monotonic() - t0 < timeout:
        try:
            if not any(x.get("retired") for a in addrs for x in status(a).get("services", [])):
                break
        except OSError:
            pass
        time.sleep(0.01)
    t_gone = (time.monotonic() - t0) * 1000
    return {"apply_cmd_ms": t_apply, "new_version_everywhere_ms": t_new, "per_addr_ms": per, "old_version_gone_ms": t_gone,
            "not_switched": pending, "apply_ok": r.returncode == 0}


def med(xs):
    return statistics.median(xs) if xs else None


def manifest(prefix, n, code, kv):
    def val(k, v):
        if k in ("code", "config"):
            return f'"{v}"'
        if k == "calls":  # calls=a,b -> ["a", "b"]
            return "[" + ", ".join(f'"{x}"' for x in v.split(",") if x) + "]"
        return v
    attrs = " ".join(f"{k} = {val(k, v)}" for k, v in kv)
    return "".join(f'service {prefix}{i} {{ code = "{code}" {attrs} }}\n' for i in range(n))


def main():
    a = sys.argv[1:]
    cmd = a[0]
    if cmd == "env":
        print(json.dumps(env(a[1])))
    elif cmd == "fadvise-drop":   # evict a file's clean page-cache pages so the next process that maps it is charged
        fd = os.open(a[1], os.O_RDONLY)
        os.fsync(fd)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
    elif cmd == "hostbusy":   # host busy CPU jiffies (USER_HZ)
        print(host_busy_jiffies())
    elif cmd == "loadavg":
        print(json.dumps(loadavg()))
    elif cmd == "snap":
        pids = [int(x) for x in a[1].split(",") if x]
        addrs = [x for x in a[2].split(",") if x] if len(a) > 2 else []
        print(json.dumps(snap(pids, addrs)))
    elif cmd == "idle":
        print(json.dumps(idle_report(json.load(open(a[1])), json.load(open(a[2])))))
    elif cmd == "delta":
        print(json.dumps(delta_report(json.load(open(a[1])), json.load(open(a[2])), int(a[3]))))
    elif cmd == "cpu":
        print(cpu_us([int(x) for x in a[1].split(",") if x]))
    elif cmd == "manifest":
        kv = [x.split("=", 1) for x in a[4:]]
        sys.stdout.write(manifest(a[1], int(a[2]), a[3], kv))
    elif cmd == "get-many":
        paths = paths_arg(a[2])
        t = time.monotonic()
        res = get_many(a[1], paths, int(a[3]) if len(a) > 3 else 8, int(a[4]) if len(a) > 4 else 64)
        el = time.monotonic() - t
        print(json.dumps({"requests": len(paths), "elapsed_s": el, "rps": len(paths) / el if el else 0, "status": hist(res)}))
    elif cmd == "serve-time":
        print(json.dumps(serve_time(a[1], a[2], a[3].split(","), a[4], int(a[5]), float(a[6]) if len(a) > 6 else 300.0,
                                    a[7] if len(a) > 7 else None)))
    elif cmd == "cold":
        print(json.dumps(cold(a[1], a[2], int(a[3]), a[4] if len(a) > 4 else "")))
    elif cmd == "burst":
        print(json.dumps(burst(a[1], a[2], int(a[3]))))
    elif cmd == "watch-failover":
        print(json.dumps(watch_failover(a[1], a[2], a[3], float(a[4]), float(a[5]) if len(a) > 5 else 60.0)))
    elif cmd == "rollout":
        print(json.dumps(rollout(a[1], a[2], a[3].split(","), a[4], a[5])))
    elif cmd == "placement":   # placement <addr> -> {"node_id": ..., "placement": {svc: [ids]}, "members": {id: http}}
        s = status(a[1]); c = s.get("cluster", {})
        print(json.dumps({"id": c.get("id"), "placement": c.get("placement", {}),
                          "members": {m["id"]: m.get("http") for m in c.get("members", [])}}))
    elif cmd == "status":
        print(json.dumps(status(a[1])))
    elif cmd == "wrap":
        runs = [json.loads(l) for l in open(a[3]) if l.strip()]
        print(json.dumps({"experiment": a[1], "env": json.load(open(a[2])), "runs": runs}, indent=1))
    else:
        sys.exit(f"unknown subcommand {cmd}\n{__doc__}")


if __name__ == "__main__":
    main()
