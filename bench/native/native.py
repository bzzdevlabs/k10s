#!/usr/bin/env python3
"""Bare-process baseline. Usage:
  native.py scale   <echo-binary> <outfile> [repeats]   N=1,10,50,100 copies on consecutive ports: start time, RSS/PSS per process
  native.py latency <bindir> <outfile> [repeats]        loadgen vs one echo (and chain->echo) instance, 1/16/64 conns
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

def latency(bindir, outfile, repeats):
    dur = os.environ.get("LAT_DURATION", "20"); warm = os.environ.get("LAT_WARMUP", "3")
    conns = os.environ.get("LAT_CONNS", "1 16 64").split()
    pe, pc = BASE_PORT, BASE_PORT + 1
    echo = subprocess.Popen([f"{bindir}/echo"], env={**os.environ, "PORT": str(pe)})
    chain = subprocess.Popen([f"{bindir}/chain"], env={**os.environ, "PORT": str(pc), "BACKEND_URL": f"http://127.0.0.1:{pe}/"})
    for p in (pe, pc):
        t = time.time()
        while not get(p, 0.3):
            assert time.time() - t < 20, "server did not start"; time.sleep(0.1)
    runs = []
    try:
        for target, port in (("echo", pe), ("chain", pc)):
            for c in conns:
                for r in range(repeats):
                    o = subprocess.run([f"{bindir}/loadgen", "-addr", f"127.0.0.1:{port}", "-conns", c, "-duration", dur + "s", "-warmup", warm + "s",
                                        "-label", f"native/{target}"], capture_output=True, text=True)
                    j = json.loads(o.stdout); j.update({"target": target, "repeat": r + 1, "place": "host"}); runs.append(j)
                    print(f"native/{target} c={c} rep={r+1} rps={j['rps']:.0f} p50={j['latency_us']['p50']} p99={j['latency_us']['p99']} err={j['errors']}", file=sys.stderr)
    finally:
        stop([echo, chain])
    json.dump({"experiment": "native_latency", "env": env(), "runs": runs}, open(outfile, "w"), indent=1)

if __name__ == "__main__":
    cmd, a, out = sys.argv[1:4]
    rep = int(sys.argv[4]) if len(sys.argv) > 4 else int(os.environ.get("REPEATS", "3"))
    {"scale": scale, "latency": latency}[cmd](a, out, rep)
