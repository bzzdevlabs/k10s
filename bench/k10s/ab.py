#!/usr/bin/env python3
"""Interleaved A/B of two k10s binaries (single node + 3-node forwarded), default flags.
Usage: ab.py BASE_BIN NEW_BIN LOADGEN OUT.jsonl [rounds] [dur_s] [port_base]"""
import hashlib, hmac, json, os, signal, subprocess, sys, tempfile, time, urllib.parse, urllib.request

BASE, NEW, LG, OUT = sys.argv[1:5]
ROUNDS = int(sys.argv[5]) if len(sys.argv) > 5 else 4
DUR = int(sys.argv[6]) if len(sys.argv) > 6 else 10
PB = int(sys.argv[7]) if len(sys.argv) > 7 else 30000
WARM = 2
REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "../.."))
SRC = {"echo": f"{REPO}/examples/echo.ten", "compute": f"{REPO}/examples/compute.ten", "chain": f"{REPO}/bench/k10s/ten/chain.ten"}
TCK = os.sysconf("SC_CLK_TCK")


def cpu(pids):
    t = 0
    for p in pids:
        f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
        t += int(f[11]) + int(f[12])
    return t / TCK


def get(url, key=None):
    """GET, signed with the admin key file `key` (K10s-HMAC, docs/NODE.md) when given."""
    req = urllib.request.Request(url)
    if key:
        u = urllib.parse.urlsplit(url)
        target = u.path + (f"?{u.query}" if u.query else "")
        a = get(f"{u.scheme}://{u.netloc}/_k10s/audience").decode().strip()
        t, n, h = str(int(time.time() * 1000)), os.urandom(16).hex(), hashlib.sha256(b"").hexdigest()
        sig = hmac.new(open(key, "rb").read().strip(), "\n".join(["K10s-HMAC", a, "GET", target, t, n, h]).encode(),
                       hashlib.sha256).hexdigest()
        req.add_header("Authorization", f"K10s-HMAC a={a},t={t},n={n},h={h},sig={sig}")
    with urllib.request.urlopen(req, timeout=2) as r:
        return r.read()


def admin_key(variant, d):
    """The new binary signs admin requests; the base serves them unsigned to loopback clients."""
    return f"{d}/admin.key" if variant == "new" else None


def wait_up(port, proc):
    t0 = time.time()
    while time.time() - t0 < 20:
        if proc.poll() is not None:
            raise SystemExit(f"node on {port} died")
        try:
            get(f"http://127.0.0.1:{port}/_k10s/health"); return
        except Exception:
            time.sleep(0.05)
    raise SystemExit(f"node on {port} not up")


def prep(variant, binary, d):
    mods = {}
    for n, s in SRC.items():
        mods[n] = f"{d}/{n}.k10"
        subprocess.run([binary, "build", s, "-o", mods[n]], check=True, capture_output=True)
    calls = ' calls = ["echo"]' if variant == "new" else ""
    with open(f"{d}/app.k10s", "w") as f:
        f.write(f'service echo {{ code = "echo.k10" memory = 64KiB }}\n'
                f'service chain {{ code = "chain.k10" memory = 64KiB config = "echo"{calls} }}\n'
                f'service compute {{ code = "compute.k10" memory = 64KiB cpu_ms = 60000 }}\n')
    with open(f"{d}/echo.k10s", "w") as f:
        f.write('service echo { code = "echo.k10" memory = 64KiB }\n')
    if variant == "new":
        for k in ("cluster.key", "admin.key"):
            subprocess.run([binary, "keygen", "-o", f"{d}/{k}"], check=True, capture_output=True)


def node(binary, d, tag, port, extra, key):
    data = f"{d}/data-{tag}"; os.makedirs(data, exist_ok=True)
    extra = extra + (["--admin-token-file", key] if key else [])
    log = open(f"{d}/{tag}.log", "w")
    p = subprocess.Popen([binary, "node", "--http", f"127.0.0.1:{port}", "--data", data, "--name", tag] + extra,
                         stdout=log, stderr=subprocess.STDOUT)
    wait_up(port, p)
    return p


def stop(ps):
    for p in ps:
        p.send_signal(signal.SIGTERM)
    for p in ps:
        try: p.wait(10)
        except subprocess.TimeoutExpired: p.kill(); p.wait()


def load(addr, path, conns, pids):
    lg = subprocess.Popen([LG, "-addr", addr, "-path", path, "-conns", str(conns), "-duration", f"{DUR}s",
                           "-warmup", f"{WARM}s"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    time.sleep(WARM); c0 = cpu(pids); t0 = time.time()
    out, _ = lg.communicate(); c1 = cpu(pids); w = time.time() - t0
    o = json.loads(out)
    req = o["requests"]
    errs = o.get("errors", 0); errs = sum(errs.values()) if isinstance(errs, dict) else errs
    return {"rps": o["rps"], "p50": o["latency_us"]["p50"], "p99": o["latency_us"]["p99"], "errors": errs,
            "cpu_us_req": (c1 - c0) / max(1, req) * 1e6 * (o["duration_s"] / w) if req else None,
            "load1": float(open("/proc/loadavg").read().split()[0])}


def apply(binary, bundle, addr, key):
    env = dict(os.environ, **({"K10S_ADMIN_TOKEN_FILE": key} if key else {}))
    subprocess.run([binary, "apply", bundle, "--node", addr], check=True, capture_output=True, env=env)


def single(variant, binary, d, port, rnd, emit):
    key = admin_key(variant, d)
    p = node(binary, d, f"s{rnd}", port, ["--shards", "8"], key)
    apply(binary, f"{d}/app.k10s", f"127.0.0.1:{port}", key)
    time.sleep(0.5)
    for path in ["/echo", "/chain", "/compute?n=1000"]:
        get(f"http://127.0.0.1:{port}{path}")
    for tgt, path in [("echo", "/echo"), ("chain", "/chain"), ("compute", "/compute?n=1000")]:
        for c in (1, 64):
            emit({"variant": variant, "round": rnd, "case": f"single/{tgt}/c{c}", **load(f"127.0.0.1:{port}", path, c, [p.pid])})
    stop([p])


def cluster(variant, binary, d, port, rnd, emit):
    ps, http, key = [], [], admin_key(variant, d)
    auth = ["--cluster-key-file", f"{d}/cluster.key"] if variant == "new" else []
    for i in range(3):
        hp, gp, rp = port + 10 + i, port + 20 + i, port + 30 + i
        extra = ["--shards", "2", "--gossip", f"127.0.0.1:{gp}", "--rpc", f"127.0.0.1:{rp}", "--probe-ms", "200"] + auth
        if i: extra += ["--join", f"127.0.0.1:{port + 20}"]
        ps.append(node(binary, d, f"c{rnd}-{i}", hp, extra, key)); http.append(f"127.0.0.1:{hp}")
    t0 = time.time()
    while True:
        ok = all(sum(m.get("state") == "alive" for m in json.loads(get(f"http://{a}/_k10s/status", key)).get("cluster", {}).get("members", [])) == 3 for a in http)
        if ok: break
        if time.time() - t0 > 30: raise SystemExit("cluster did not converge")
        time.sleep(0.1)
    apply(binary, f"{d}/echo.k10s", http[0], key)
    t0 = time.time()
    while True:
        c = json.loads(get(f"http://{http[0]}/_k10s/status", key)).get("cluster", {})
        pl = c.get("placement", {}).get("echo")
        if pl: break
        if time.time() - t0 > 30: raise SystemExit("no placement")
        time.sleep(0.1)
    members = {m["id"]: m.get("http") for m in c.get("members", [])}
    host = members[pl[0]]; other = next(a for a in http if a != host)
    time.sleep(1)
    for a in http: get(f"http://{a}/echo")
    for via, addr in (("local", host), ("fwd", other)):
        for cc in (1, 64):
            emit({"variant": variant, "round": rnd, "case": f"cluster/{via}/c{cc}", **load(addr, "/echo", cc, [p.pid for p in ps])})
    stop(ps)


def main():
    out = open(OUT, "a")
    def emit(r):
        out.write(json.dumps(r) + "\n"); out.flush()
        print(f"{r['variant']:4} r{r['round']} {r['case']:22} rps {r['rps']:9.0f} p50 {r['p50']:7} p99 {r['p99']:7} cpu/req {r['cpu_us_req'] or 0:7.2f} err {r['errors']} load {r['load1']}", flush=True)
    dirs = {}
    for v, b in (("base", BASE), ("new", NEW)):
        dirs[v] = tempfile.mkdtemp(prefix=f"ab-{v}-"); prep(v, b, dirs[v])
    for rnd in range(ROUNDS):
        order = [("base", BASE), ("new", NEW)] if rnd % 2 == 0 else [("new", NEW), ("base", BASE)]
        for v, b in order:
            single(v, b, dirs[v], PB, rnd, emit)
            cluster(v, b, dirs[v], PB + 100, rnd, emit)


main()
