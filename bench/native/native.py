#!/usr/bin/env python3
"""Bare-process baseline. Usage:
  native.py scale   <echo-binary> <outfile> [repeats]   N=1,10,50,100 copies on consecutive ports: start time, RSS/PSS per process
  native.py latency <bindir> <outfile> [repeats]        loadgen vs one echo (and chain->echo) instance, 1/16/64 conns
  native.py latency-open <bindir> <outfile> [repeats]   open loop (-rate) at OPEN_RATES_<target>, OPEN_CONNS (64) conns
Targets: LAT_TARGETS (default "echo chain"; also "compute" = /?n=1000 and "compute-purego" = the same Go service built
with -tags purego, i.e. without Go's hand-written SHA-256 assembly). Every run also records the CPU time of the server
process(es) over the measured window (server_cpu_us_per_req, from /proc/<pid>/stat), the CPU used by everything else on the
host in that window (other_cpu_cores = host busy - servers - loadgen; noisy if > NOISY_CPU, 0.5) and the 1-min load average.
"""
import json, os, signal, socket, statistics, subprocess, sys, time, platform

BASE_PORT = int(os.environ.get("NATIVE_BASE_PORT", "19100"))
NS = [int(x) for x in os.environ.get("NS_LIST", "1 10 50 100").split()]

def env():
    cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "")
    return {"date": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "host_kernel": platform.release(), "host_cpu": cpu,
            "host_threads": os.cpu_count(), "host_mem_gib": round(int(open("/proc/meminfo").readline().split()[1]) / 1048576, 1),
            "note": "bare processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)"}

def get(port, timeout=0.5):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        s.settimeout(timeout)
        s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        d = b""
        while True:
            c = s.recv(4096)
            if not c: break
            d += c
        s.close()
        return d.startswith(b"HTTP/1.1 200")
    except OSError:
        return False

def mem(pid):
    rss = pss = 0
    try:
        for l in open(f"/proc/{pid}/smaps_rollup"):
            if l.startswith("Rss:"): rss = int(l.split()[1])
            elif l.startswith("Pss:"): pss = int(l.split()[1])
    except OSError:
        pass
    return rss, pss

def cpu_ticks(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])

def stop(procs):
    for p in procs:
        try: p.send_signal(signal.SIGKILL)
        except OSError: pass
    for p in procs: p.wait()

def scale(binary, outfile, repeats):
    runs = []
    for n in NS:
        for r in range(repeats):
            ports = [BASE_PORT + i for i in range(n)]
            t0 = time.monotonic()
            procs = [subprocess.Popen([binary], env={**os.environ, "PORT": str(p)}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for p in ports]
            t_spawn = time.monotonic() - t0
            pending = set(ports); first = None
            while pending and time.monotonic() - t0 < 60:
                for p in list(pending):
                    if get(p, 0.2):
                        pending.discard(p)
                        if first is None: first = time.monotonic() - t0
            t_all = time.monotonic() - t0
            time.sleep(3)
            ticks0 = [cpu_ticks(p.pid) for p in procs]; time.sleep(5)
            idle_cpu = sum(cpu_ticks(p.pid) - a for p, a in zip(procs, ticks0)) / 100 / 5
            ms = [mem(p.pid) for p in procs]
            runs.append({"n": n, "repeat": r + 1, "spawn_all_ms": t_spawn * 1000, "first_ready_ms": (first or 0) * 1000,
                         "all_ready_ms": t_all * 1000, "not_ready": len(pending),
                         "rss_kib_per_proc_median": statistics.median(m[0] for m in ms), "pss_kib_per_proc_median": statistics.median(m[1] for m in ms),
                         "rss_total_mib": sum(m[0] for m in ms) / 1024, "pss_total_mib": sum(m[1] for m in ms) / 1024,
                         "idle_cpu_cores_total": idle_cpu})
            print(json.dumps(runs[-1]), file=sys.stderr)
            stop(procs); time.sleep(1)
    json.dump({"experiment": "native_scale", "env": env(), "runs": runs}, open(outfile, "w"), indent=1)

TARGET_PATH = {"echo": "/", "chain": "/", "compute": "/?n=1000", "compute-purego": "/?n=1000"}
NOISY_CPU = float(os.environ.get("NOISY_CPU", "0.5"))   # cores used by other processes during the measured window

def host_busy():
    f = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    return sum(f[:8]) - f[3] - f[4]

def loadavg1():
    return float(open("/proc/loadavg").read().split()[0])

def start_servers(bindir, targets):
    """returns {target: (port, [pids of the processes serving it])}, [Popen]"""
    procs, out = [], {}
    pe = BASE_PORT
    def spawn(binary, port, extra=None):
        p = subprocess.Popen([f"{bindir}/{binary}"], env={**os.environ, "PORT": str(port), **(extra or {})})
        procs.append(p)
        t = time.time()
        while not get(port, 0.3):
            assert time.time() - t < 20, "server did not start"; time.sleep(0.1)
        return p
    echo = None
    if "echo" in targets or "chain" in targets:
        echo = spawn("echo", pe); out["echo"] = (pe, [echo.pid])
    if "chain" in targets:
        ch = spawn("chain", pe + 1, {"BACKEND_URL": f"http://127.0.0.1:{pe}/"}); out["chain"] = (pe + 1, [ch.pid, echo.pid])
    if "compute" in targets:
        c = spawn("compute", pe + 2); out["compute"] = (pe + 2, [c.pid])
    if "compute-purego" in targets:
        c = spawn("compute-purego", pe + 3); out["compute-purego"] = (pe + 3, [c.pid])
    return out, procs

def run_loadgen(bindir, port, path, pids, label, args, warm):
    l0 = loadavg1()
    lg = subprocess.Popen([f"{bindir}/loadgen", "-addr", f"127.0.0.1:{port}", "-path", path, "-label", label] + args,
                          stdout=subprocess.PIPE, text=True)
    time.sleep(float(warm))
    c0 = sum(cpu_ticks(p) for p in pids); h0 = host_busy(); t0 = time.monotonic()
    out, _ = lg.communicate()
    c1 = sum(cpu_ticks(p) for p in pids); h1 = host_busy(); t1 = time.monotonic()
    j = json.loads(out)
    cpu_s = (c1 - c0) / 100.0
    l1 = loadavg1()
    w = t1 - t0
    busy = (h1 - h0) / os.sysconf("SC_CLK_TCK") / w
    other = busy - cpu_s / w - j["loadgen_cpu_s"]["total"] / j["duration_s"]
    j.update({"server_cpu_s": cpu_s, "server_cpu_window_s": w, "server_cpu_cores": cpu_s / w,
              "server_cpu_us_per_req": (cpu_s / w * j["duration_s"] * 1e6 / j["requests"]) if j["requests"] else None,
              "host_busy_cores": busy, "other_cpu_cores": other,
              "loadavg_start": l0, "loadavg_end": l1, "noisy": other > NOISY_CPU})
    return j

def latency(bindir, outfile, repeats, open_loop=False):
    dur = os.environ.get("LAT_DURATION", "20"); warm = os.environ.get("LAT_WARMUP", "3")
    conns = os.environ.get("LAT_CONNS", "1 16 64").split()
    targets = os.environ.get("LAT_TARGETS", "echo chain").split()
    srv, procs = start_servers(bindir, targets)
    runs = []
    try:
        for target in targets:
            port, pids = srv[target]
            if open_loop:
                plan = [(os.environ.get("OPEN_CONNS", "64"), r) for r in os.environ.get(f"OPEN_RATES_{target.replace('-', '_')}",
                        "500 2000" if target.startswith("compute") else "5000 20000").split()]
            else:
                plan = [(c, None) for c in conns]
            for c, rate in plan:
                for r in range(repeats):
                    args = ["-conns", c, "-duration", dur + "s", "-warmup", warm + "s"] + (["-rate", rate] if rate else [])
                    j = run_loadgen(bindir, port, TARGET_PATH[target], pids, f"native/{target}", args, warm)
                    j.update({"target": target, "repeat": r + 1, "place": "host"}); runs.append(j)
                    print(f"native/{target} c={c} rate={rate or '-'} rep={r+1} rps={j['rps']:.0f} p50={j['latency_us']['p50']} p99={j['latency_us']['p99']} err={j['errors']} server_cpu={j['server_cpu_cores']:.2f} cores {j['server_cpu_us_per_req'] or -1:.1f} us/req other={j['other_cpu_cores']:.2f} cores load={j['loadavg_end']}", file=sys.stderr)
    finally:
        stop(procs)
    json.dump({"experiment": "native_latency_open" if open_loop else "native_latency", "env": env(), "runs": runs}, open(outfile, "w"), indent=1)

def latency_open(bindir, outfile, repeats):
    latency(bindir, outfile, repeats, open_loop=True)

if __name__ == "__main__":
    cmd, a, out = sys.argv[1:4]
    rep = int(sys.argv[4]) if len(sys.argv) > 4 else int(os.environ.get("REPEATS", "3"))
    {"scale": scale, "latency": latency, "latency-open": latency_open}[cmd](a, out, rep)
