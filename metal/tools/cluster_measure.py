#!/usr/bin/env python3
"""Cluster measurements: a k10-metal guest in a k10s cluster with 2 hosted `k10s node` processes.

  tools/cluster_measure.py            -> out/results/cluster.json

Per repetition (REPS, default 5; gossip PROBE_MS=100, SUSPICION_MULT=2 unless set):
  * join: QEMU start -> every node sees 3 live members;
  * apply through a hosted node -> each node reports the version applied; -> a guest-placed service answers through
    the guest's ingress;
  * forwarding latency with one keep-alive connection (loadgen, DUR s): guest service via the guest / via a hosted
    node, hosted service via its node / via the other hosted node / via the guest;
  * SIGKILL of the guest while probing a guest-placed service through a hosted ingress every 5 ms: failed probes,
    last failure, time until the hosted nodes declare it dead;
  * restart (same node id) -> 3 live everywhere, -> guest applied, -> a guest-placed service answers via the guest.
Environment: BASE (ports, 19700), K10S, REPS, DUR, PROBE_MS, SUSPICION_MULT, ENGINE (guest, jit).
"""
import json
import os
import secrets
import shutil
import statistics
import subprocess
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)
BASE = int(os.environ.get("BASE", "19700"))
K10S = os.environ.get("K10S", "../target/metal-host/release/k10s")
REPS = int(os.environ.get("REPS", "5"))
DUR = int(os.environ.get("DUR", "3"))
PROBE_MS = os.environ.get("PROBE_MS", "100")
SUSP = os.environ.get("SUSPICION_MULT", "2")
ENGINE = os.environ.get("ENGINE", "jit")
LOADGEN = os.path.abspath("../bench/bin/loadgen")
OUT = "out/cluster-measure"
GUEST_ID = "00000000000000a1"
H = [BASE + 1, BASE + 2, BASE + 3]
G = [BASE + 101, BASE + 102, BASE + 103]
R = [BASE + 201, BASE + 202, BASE + 203]
os.environ["K10S_ADMIN_TOKEN_FILE"] = os.path.abspath(f"{OUT}/admin.key")
sys.path.insert(0, os.path.abspath("../bench/k10s"))
from k10s import OPEN, admin_auth  # noqa: E402


def new_keys():
    """Fresh cluster and admin keys (`k10s keygen` format, mode 0600): hosted nodes take the files, the guest gets
    them as fw_cfg files; admin requests are signed with the admin key (bench/k10s/k10s.py)."""
    os.makedirs(OUT, exist_ok=True)
    for k in ("cluster", "admin"):
        with os.fdopen(os.open(f"{OUT}/{k}.key", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as f:
            f.write(secrets.token_hex(32) + "\n")


def now():
    return time.monotonic()


def get(port, path, timeout=2.0):
    try:
        signed = path.startswith("/_k10s/") and path.split("?")[0] not in OPEN
        headers = {"Authorization": admin_auth(f"127.0.0.1:{port}", "GET", path, b"")} if signed else {}
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception:  # noqa: BLE001
        return 0, b""


def status(port):
    st, body = get(port, "/_k10s/status", 1.0)
    if st != 200:
        return None
    try:
        return json.loads(body)
    except ValueError:
        return None


def wait(cond, timeout=20.0, step=0.005):
    t0 = now()
    while now() - t0 < timeout:
        if cond():
            return now()
        time.sleep(step)
    return None


def live(port):
    s = status(port)
    return s["cluster"]["live_members"] if s and "cluster" in s else None


def applied(port):
    s = status(port)
    return ((s or {}).get("cluster", {}).get("applied") or {}).get("version")


def hosted(name, i, join=None):
    os.makedirs(OUT, exist_ok=True)
    args = [K10S, "node", "--name", name, "--http", f"127.0.0.1:{H[i]}", "--gossip", f"127.0.0.1:{G[i]}", "--rpc",
            f"127.0.0.1:{R[i]}", "--data", f"{OUT}/{name}", "--mem", "64MiB", "--shards", "2", "--probe-ms", PROBE_MS,
            "--suspicion-mult", SUSP, "--cluster-key-file", f"{OUT}/cluster.key", "--admin-token-file", f"{OUT}/admin.key"]
    if join:
        args += ["--join", join]
    return subprocess.Popen(args, stderr=open(f"{OUT}/{name}.log", "w"), stdout=subprocess.DEVNULL)


def guest():
    append = (f"k10.engine={ENGINE} k10.join=127.0.0.1:{G[0]},127.0.0.1:{G[1]} k10.gossip=127.0.0.1:{G[2]} "
              f"k10.rpc=127.0.0.1:{R[2]} k10.http=127.0.0.1:{H[2]} k10.node_id={GUEST_ID} k10.probe_ms={PROBE_MS} "
              f"k10.suspicion_mult={SUSP} k10.mem=67108864")
    cmd = ["qemu-system-x86_64", "-M", "pc", "-enable-kvm", "-cpu", "host", "-m", "128M", "-smp", "1", "-nodefaults",
           "-display", "none", "-no-reboot", "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
           "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{H[2]}-:80,hostfwd=udp:127.0.0.1:{G[2]}-:7946,"
                      f"hostfwd=tcp:127.0.0.1:{R[2]}-:7947",
           "-device", "virtio-net-pci,disable-modern=on,netdev=n0,romfile=", "-kernel", "out/k10-metal.elf",
           "-serial", f"file:{OUT}/guest.log", "-append", append,
           "-fw_cfg", f"name=opt/k10s/cluster_key,file={OUT}/cluster.key", "-fw_cfg", f"name=opt/k10s/admin_key,file={OUT}/admin.key"]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def loadgen(port, path, conns=1):
    r = subprocess.run([LOADGEN, "-addr", f"127.0.0.1:{port}", "-path", path, "-conns", str(conns), "-duration",
                        f"{DUR}s", "-warmup", "300ms"], capture_output=True, text=True)
    d = json.loads(r.stdout)
    return {"rps": round(d["rps"]), "p50_us": d["latency_us"]["p50"], "p99_us": d["latency_us"]["p99"], "errors": d["errors"]}


def one_rep(rep):
    shutil.rmtree(OUT, ignore_errors=True)
    new_keys()
    res = {"loadavg": float(open("/proc/loadavg").read().split()[0])}
    procs = [hosted("n1", 0)]
    time.sleep(0.3)
    procs.append(hosted("n2", 1, f"127.0.0.1:{G[0]}"))
    assert wait(lambda: live(H[0]) == 2 and live(H[1]) == 2), "hosted nodes did not converge"
    t0 = now()
    q = guest()
    t = wait(lambda: all(live(p) == 3 for p in H))
    res["join_ms"] = round((t - t0) * 1000, 1) if t else None
    # 12 copies of `hello` (+ echo for the forwarding measurements): some land on every node.
    subprocess.run(["target/release/mkbundle", "-o", f"{OUT}/b.k10b", "--version", "0", "--replicate", "hello:12"],
                   check=True, capture_output=True)
    data = open(f"{OUT}/b.k10b", "rb").read()
    t0 = now()
    req = urllib.request.Request(f"http://127.0.0.1:{H[0]}/_k10s/apply", data=data, method="POST",
                                 headers={"Authorization": admin_auth(f"127.0.0.1:{H[0]}", "POST", "/_k10s/apply", data)})
    v = json.loads(urllib.request.urlopen(req, timeout=10).read())["version"]
    t_api = now()
    done = {}
    while now() - t0 < 10 and len(done) < 3:
        for i, p in enumerate(H):
            if i not in done and applied(p) == v:
                done[i] = now()
        time.sleep(0.002)
    res["apply_request_ms"] = round((t_api - t0) * 1000, 1)
    res["apply_to_applied_ms"] = {n: round((done[i] - t0) * 1000, 1) if i in done else None
                                  for i, n in enumerate(["n1", "n2", "guest"])}
    st = status(H[2])
    place = st["cluster"]["placement"]
    gsvc = sorted(s for s, n in place.items() if GUEST_ID in n)
    n1_id, n2_id = status(H[0])["cluster"]["id"], status(H[1])["cluster"]["id"]
    n1svc = sorted(s for s, n in place.items() if n1_id in n)
    res["services_on"] = {"guest": len(gsvc), "n1": len(n1svc), "n2": len(place) - len(gsvc) - len(n1svc)}
    if not gsvc or not n1svc:
        res["skipped"] = "placement put nothing on the guest or on n1"
    else:
        gs, hs = gsvc[0], n1svc[0]
        t = wait(lambda: get(H[2], f"/{gs}/")[0] == 200, 5)
        res["apply_to_guest_service_200_ms"] = round((t - t0) * 1000, 1) if t else None
        res["latency"] = {
            "guest service via guest": loadgen(H[2], f"/{gs}/"),
            "guest service via hosted n1": loadgen(H[0], f"/{gs}/"),
            "n1 service via n1": loadgen(H[0], f"/{hs}/"),
            "n1 service via n2": loadgen(H[1], f"/{hs}/"),
            "n1 service via guest": loadgen(H[2], f"/{hs}/"),
            "guest service via hosted n1, 16 conns": loadgen(H[0], f"/{gs}/", 16),
        }
        # Failover: probe the guest's service through n1 every 5 ms, kill the guest.
        probes = []
        t_end = now() + 1.0
        while now() < t_end:
            probes.append((now(), get(H[0], f"/{gs}/", 1.0)[0]))
            time.sleep(0.005)
        t_kill = now()
        q.kill()
        q.wait()
        dead_at = None
        while now() - t_kill < 6:
            probes.append((now(), get(H[0], f"/{gs}/", 1.0)[0]))
            if dead_at is None:
                s = status(H[0])
                if s and any(m["id"] == GUEST_ID and m["state"] == "dead" for m in s["cluster"]["members"]):
                    dead_at = now()
            elif now() - dead_at > 0.5:
                break
            time.sleep(0.005)
        after = [(t, c) for t, c in probes if t >= t_kill]
        fails = [t for t, c in after if c != 200]
        res["failover"] = {
            "probes_before_ok": all(c == 200 for t, c in probes if t < t_kill),
            "probes_after": len(after),
            "failed": len(fails),
            "last_failure_ms": round((fails[-1] - t_kill) * 1000, 1) if fails else 0,
            "declared_dead_ms": round((dead_at - t_kill) * 1000, 1) if dead_at else None,
        }
        # Rejoin with the same id.
        t0 = now()
        q = guest()
        t = wait(lambda: all(live(p) == 3 for p in H))
        res["rejoin_ms"] = round((t - t0) * 1000, 1) if t else None
        t = wait(lambda: applied(H[2]) == v)
        res["rejoin_to_applied_ms"] = round((t - t0) * 1000, 1) if t else None
        t = wait(lambda: get(H[2], f"/{gs}/")[0] == 200 and GUEST_ID in str(status(H[0])["cluster"]["placement"]))
        res["rejoin_to_serving_ms"] = round((t - t0) * 1000, 1) if t else None
    q.kill()
    q.wait()
    for p in procs:
        p.kill()
        p.wait()
    print(f"rep {rep}: {json.dumps(res)}", flush=True)
    return res


def med(xs):
    xs = [x for x in xs if isinstance(x, (int, float))]
    return round(statistics.median(xs), 1) if xs else None


def summarize(runs):
    def walk(prefix, objs):
        out = {}
        for k, v in objs[0].items():
            vals = [o.get(k) for o in objs if isinstance(o, dict) and k in o]
            if isinstance(v, dict):
                out[k] = walk(prefix + k + ".", vals)
            elif isinstance(v, (int, float)) and not isinstance(v, bool):
                nums = [x for x in vals if isinstance(x, (int, float))]
                out[k] = {"median": med(nums), "min": min(nums), "max": max(nums)} if nums else None
            else:
                out[k] = vals
        return out

    return walk("", [r for r in runs if "skipped" not in r] or runs)


if __name__ == "__main__":
    runs = [one_rep(i) for i in range(REPS)]
    s = summarize(runs)
    print(json.dumps(s, indent=1))
    os.makedirs("out/results", exist_ok=True)
    with open("out/results/cluster.json", "w") as f:
        json.dump({"summary": s, "runs": runs, "probe_ms": PROBE_MS, "suspicion_mult": SUSP, "engine": ENGINE}, f, indent=1)
