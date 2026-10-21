#!/usr/bin/env python3
"""k10-metal measurements (writes out/results/<section>.json and prints a summary).

  tools/measure.py [sections...]      sections: size boot ram mem http spin engines smp linux   (default: all)

Environment: PORT (base host port, default 18090), RUNS (boot runs, default 10), REPS (http repetitions, default 3),
DUR (loadgen seconds per run, default 5). Needs ./build.sh and, for `linux`, tools/linux-compare.sh to have built
out/linux/*. The host is shared, so every throughput/latency figure is the median of interleaved repetitions.
"""
import json
import os
import re
import secrets
import select
import socket
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)
OUT = "out/results"
os.makedirs(OUT, exist_ok=True)
PORT = int(os.environ.get("PORT", "18090"))
RUNS = int(os.environ.get("RUNS", "10"))
REPS = int(os.environ.get("REPS", "3"))
DUR = int(os.environ.get("DUR", "5"))
LOADGEN = os.path.abspath("../bench/bin/loadgen")
KERNEL = "out/k10-metal.elf"
BUNDLE = "out/bundle.k10b"
LINUX_KERNEL = f"/boot/vmlinuz-{os.uname().release}"
QBOOT = "/usr/share/qemu/qboot.rom"
# Admin key given to every k10-metal guest (fw_cfg opt/k10s/admin_key; `k10s keygen` format, mode 0600); admin
# requests are signed with it (bench/k10s/k10s.py).
ADMIN_KEY = os.path.abspath(f"{OUT}/admin.key")
if os.path.exists(ADMIN_KEY):
    os.unlink(ADMIN_KEY)
with os.fdopen(os.open(ADMIN_KEY, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as f:
    f.write(secrets.token_hex(32) + "\n")
os.environ["K10S_ADMIN_TOKEN_FILE"] = ADMIN_KEY
sys.path.insert(0, os.path.abspath("../bench/k10s"))
from k10s import OPEN, admin_auth  # noqa: E402


def qemu_cmd(mem="64M", kernel=KERNEL, initrd=BUNDLE, append="", port=PORT, bios=None, net=True, debug_exit=True, smp=1):
    c = ["qemu-system-x86_64", "-name", "k10,debug-threads=on", "-M", "pc", "-enable-kvm", "-cpu", "host", "-m", mem, "-smp", str(smp), "-nodefaults",
         "-display", "none", "-serial", "stdio", "-no-reboot"]
    if debug_exit:
        c += ["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]
    if net:
        c += ["-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80",
              "-device", "virtio-net-pci,disable-modern=on,netdev=n0,romfile="]
    if bios:
        c += ["-bios", bios]
    c += ["-kernel", kernel]
    if initrd:
        c += ["-initrd", initrd]
    if "k10-metal" in kernel:
        c += ["-fw_cfg", f"name=opt/k10s/admin_key,file={ADMIN_KEY}"]
    if append:
        c += ["-append", append]
    return c


class Guest:
    """A QEMU process with its serial console on a pipe."""

    def __init__(self, cmd, marker, timeout=30.0):
        self.t0 = time.monotonic()
        self.p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
        self.out = b""
        self.ready = None
        fd = self.p.stdout.fileno()
        os.set_blocking(fd, False)
        while time.monotonic() - self.t0 < timeout:
            left = timeout - (time.monotonic() - self.t0)
            if not select.select([fd], [], [], max(left, 0))[0]:
                break
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            self.out += chunk
            if marker.encode() in self.out:
                self.ready = time.monotonic() - self.t0
                break

    def drain(self):
        try:
            while True:
                c = os.read(self.p.stdout.fileno(), 65536)
                if not c:
                    break
                self.out += c
        except BlockingIOError:
            pass
        return self.out.decode(errors="replace")

    def kill(self):
        self.p.kill()
        self.p.wait()

    def threads_cpu(self):
        """{thread name: cpu seconds} of the QEMU process."""
        res = {}
        hz = os.sysconf("SC_CLK_TCK")
        base = f"/proc/{self.p.pid}/task"
        for tid in os.listdir(base):
            try:
                st = open(f"{base}/{tid}/stat").read()
            except OSError:
                continue
            name = st[st.index("(") + 1:st.rindex(")")]
            f = st[st.rindex(")") + 2:].split()
            res[name] = res.get(name, 0) + (int(f[11]) + int(f[12])) / hz
        return res


def http(port, path, method="GET", body=b"", timeout=5.0):
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as s:
        s.settimeout(timeout)
        signed = path.startswith("/_k10s/") and path.split("?")[0] not in OPEN
        auth = f"Authorization: {admin_auth(f'127.0.0.1:{port}', method, path, body)}\r\n" if signed else ""
        s.sendall(f"{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {len(body)}\r\n"
                  f"Connection: close\r\n\r\n".encode() + body)
        data = b""
        while True:
            c = s.recv(65536)
            if not c:
                break
            data += c
            head, sep, rest = data.partition(b"\r\n\r\n")
            m = re.search(rb"(?i)content-length:\s*(\d+)", head) if sep else None
            if m and len(rest) >= int(m.group(1)):
                break
    head, _, rest = data.partition(b"\r\n\r\n")
    return int(head[9:12]), rest


def status(port):
    return json.loads(http(port, "/_k10s/status")[1])


def loadgen(port, path, conns, dur=DUR, method="GET", body=None, warmup="1s"):
    cmd = [LOADGEN, "-addr", f"127.0.0.1:{port}", "-path", path, "-conns", str(conns), "-duration", f"{dur}s", "-warmup", warmup]
    if method != "GET":
        cmd += ["-method", method]
    if body is not None:
        cmd += ["-body", body]
    return json.loads(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout)


def med(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 2) if xs else None


def loadavg():
    return open("/proc/loadavg").read().split()[0]


def save(name, data):
    data["host_loadavg_at_end"] = loadavg()
    with open(f"{OUT}/{name}.json", "w") as f:
        json.dump(data, f, indent=1)
    print(f"-> {OUT}/{name}.json")


# --- sections -------------------------------------------------------------------------------------------------

def sec_size():
    r = {"elf_file_bytes": os.path.getsize(KERNEL), "bundle_bytes": os.path.getsize(BUNDLE)}
    ph = subprocess.run(["readelf", "-lW", KERNEL], capture_output=True, text=True).stdout
    loads = [l.split() for l in ph.splitlines() if l.strip().startswith("LOAD")]
    r["loaded_file_bytes"] = sum(int(l[4], 16) for l in loads)
    r["loaded_mem_bytes"] = sum(int(l[5], 16) for l in loads)
    sh = subprocess.run(["readelf", "-SW", KERNEL], capture_output=True, text=True).stdout
    for l in sh.splitlines():
        m = re.match(r"\s*\[\s*\d+\]\s+(\.\S+)\s+\S+\s+[0-9a-f]+\s+[0-9a-f]+\s+([0-9a-f]+)", l)
        if m and m.group(1) in (".text", ".rodata", ".data", ".bss"):
            r[f"section{m.group(1)}"] = int(m.group(2), 16)
    mods = subprocess.run(["target/release/mkbundle", "--list"], capture_output=True, text=True).stdout
    r["modules"] = {l.split()[0]: int(l.split()[1]) for l in mods.splitlines() if l.strip()}
    print(json.dumps(r, indent=1))
    save("size", r)


def sec_boot():
    r = {}
    samples = {"seabios": [], "qboot": []}
    # Interleaved so that host-load drift affects both firmwares alike.
    for _ in range(RUNS):
        for fw, bios in (("seabios", None), ("qboot", QBOOT)):
            g = Guest(qemu_cmd(bios=bios), "k10-metal ready")
            t_http = None
            if g.ready is not None:
                while time.monotonic() - g.t0 < 10:
                    try:
                        if http(PORT, "/hello/", timeout=1.0)[0] == 200:
                            t_http = time.monotonic() - g.t0
                            break
                    except OSError:
                        time.sleep(0.001)
            m = re.search(r"ready ([0-9.]+)ms vm->kmain=([0-9.]+)ms", g.drain())
            samples[fw].append((g.ready * 1000 if g.ready else None, t_http * 1000 if t_http else None,
                                float(m.group(1)) if m else None, float(m.group(2)) if m else None))
            g.kill()
    for fw, xs in samples.items():
        r[fw] = {
            "ready_ms_median": med([x[0] for x in xs]),
            "ready_ms_min": round(min(x[0] for x in xs if x[0]), 2),
            "first_http_200_ms_median": med([x[1] for x in xs]),
            "first_http_200_ms_min": round(min(x[1] for x in xs if x[1]), 2),
            "kernel_kmain_to_ready_ms_median": med([x[2] for x in xs]),
            "vm_created_to_kmain_ms_median": med([x[3] for x in xs]),
            "qemu_start_to_vm_created_ms_median": med([x[0] - x[2] - x[3] for x in xs if x[0] and x[2] is not None]),
            "samples_ready_ms": [round(x[0], 1) if x[0] else None for x in xs],
            "samples_http_ms": [round(x[1], 1) if x[1] else None for x in xs],
        }
        print(fw, json.dumps({k: v for k, v in r[fw].items() if not k.startswith("samples")}))
    # QEMU's own startup: process start until the monitor prompt of a paused (-S) VM, i.e. before any firmware runs.
    t = []
    for _ in range(RUNS):
        t0 = time.monotonic()
        p = subprocess.Popen(["qemu-system-x86_64", "-M", "pc", "-enable-kvm", "-cpu", "host", "-m", "64M", "-nodefaults",
                              "-display", "none", "-S", "-monitor", "stdio",
                              "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{PORT}-:80",
                              "-device", "virtio-net-pci,disable-modern=on,netdev=n0,romfile="],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        buf = b""
        while b"(qemu)" not in buf:
            c = p.stdout.read1(4096)
            if not c:
                break
            buf += c
        t.append((time.monotonic() - t0) * 1000)
        p.kill()
        p.wait()
    r["qemu_init_until_guest_can_start_ms_median"] = med(t)
    print("qemu init (no firmware/guest)", r["qemu_init_until_guest_can_start_ms_median"])
    save("boot", r)


def serves(port):
    try:
        ok = http(port, "/hello/")[0] == 200
        ok &= http(port, "/echo/", "POST", b"x" * 20000) == (200, b"x" * 20000)
        ok &= http(port, "/chain")[0] == 200
        ok &= http(port, "/counter")[0] == 200
        lg = loadgen(port, "/hello/", 16, dur=2, warmup="200ms")
        return ok and lg["errors"] == 0 and lg["requests"] > 0, lg["rps"]
    except Exception as e:  # noqa: BLE001
        return False, str(e)


def sec_ram():
    r = {"k10_metal": {}}
    for mem in ["2M", "3M", "4M", "6M", "8M", "16M", "32M"]:
        g = Guest(qemu_cmd(mem=mem), "k10-metal ready", timeout=10)
        res = {"boots": g.ready is not None}
        if g.ready is not None:
            res["serves"], res["rps"] = serves(PORT)
            try:
                st = status(PORT)
                res["used_bytes"] = st["mem"]["used_incl_kernel"]
                res["total_ram"] = st["mem"]["total_ram"]
            except Exception:  # noqa: BLE001
                pass
        else:
            res["tail"] = g.drain()[-200:]
        g.kill()
        r["k10_metal"][mem] = res
        print("k10-metal", mem, res)
    passing = [m for m, v in r["k10_metal"].items() if v.get("serves")]
    r["k10_metal_min"] = passing[0] if passing else None
    save("ram", r)


def sec_mem():
    r = {}
    # Kernel alone (no services), and with the 13 test services before/after one request each.
    g = Guest(qemu_cmd(initrd=None), "k10-metal ready")
    r["no_services_after_boot"] = status(PORT)["mem"]
    g.kill()
    g = Guest(qemu_cmd(), "k10-metal ready")
    r["test_bundle_after_boot"] = status(PORT)["mem"]
    for s in ["hello", "echo", "compute", "counter", "chain", "instcount", "freshcount", "config", "log", "time", "random"]:
        http(PORT, "/" + s)
    r["test_bundle_after_one_request_each"] = status(PORT)["mem"]
    g.kill()
    # Per instance: N copies of `hello` (64 KiB slots), one warm instance each.
    for n in (100, 1000):
        subprocess.run(["target/release/mkbundle", "-o", f"out/hello{n}.k10b", "--replicate", f"hello:{n}"], check=True, capture_output=True)
        g = Guest(qemu_cmd(mem="512M", initrd=f"out/hello{n}.k10b"), "k10-metal ready")
        before = status(PORT)
        t0 = time.monotonic()
        for i in range(n):
            assert http(PORT, f"/hello-{i}/")[0] == 200
        dt = time.monotonic() - t0
        after = status(PORT)
        g.kill()
        used_b, used_a = before["mem"]["used"], after["mem"]["used"]
        inst = sum(s["instances"] for s in after["services"])
        r[f"hello_x{n}"] = {
            "services": n,
            "instances": inst,
            "used_after_boot": used_b,
            "used_after_one_request_each": used_a,
            "bytes_per_service_deployed": round((used_b - r["no_services_after_boot"]["used"]) / n),
            "bytes_per_instance": round((used_a - used_b) / inst),
            "slot_bytes": after["services"][0]["instance_bytes"],
            "first_request_each_ms_avg": round(dt * 1000 / n, 3),
        }
        print(f"hello x{n}", r[f"hello_x{n}"])
    save("mem", r)


def cpu_split(g, before, after, requests):
    """Per-request CPU (µs) of the QEMU threads: vCPU (guest + exits) vs main loop (slirp + device emulation)."""
    d = {k: after.get(k, 0) - before.get(k, 0) for k in after}
    vcpu = sum(v for k, v in d.items() if "CPU" in k)
    main = sum(v for k, v in d.items() if "CPU" not in k)
    return {"vcpu_us_per_req": round(vcpu * 1e6 / requests, 1), "qemu_main_us_per_req": round(main * 1e6 / requests, 1)}


def run_http_case(label, cmd, marker, port, path, conns, method="GET", body=None, guest_status=False):
    g = Guest(cmd, marker)
    assert g.ready is not None, f"{label}: no ready line"
    time.sleep(0.2)
    st0 = status(port) if guest_status else None
    c0 = g.threads_cpu()
    lg = loadgen(port, path, conns, method=method, body=body)
    c1 = g.threads_cpu()
    res = {"rps": round(lg["rps"]), "p50_us": lg["latency_us"]["p50"], "p99_us": lg["latency_us"]["p99"],
           "errors": lg["errors"], "requests": lg["requests"]}
    total_req = lg["requests"] * (DUR + 1) / DUR  # warmup requests are not counted by loadgen but cost CPU
    res.update(cpu_split(g, c0, c1, total_req))
    if guest_status:
        st1 = status(port)
        up = st1["uptime_ms"] - st0["uptime_ms"]
        idle = (st1["cpu"]["halted_ms"] - st0["cpu"]["halted_ms"]) + (st1["cpu"]["polling_ms"] - st0["cpu"]["polling_ms"])
        res["guest_busy_pct"] = round(100 * (up - idle) / up, 1)
        res["guest_steal_pct"] = round(100 * (st1["cpu"]["steal_ms"] - st0["cpu"]["steal_ms"]) / up, 1)
        n0, n1 = st0["counters"]["net"], st1["counters"]["net"]
        dreq = st1["counters"]["requests"] - st0["counters"]["requests"]
        res["guest_rx_pkts_per_req"] = round((n1["rx_packets"] - n0["rx_packets"]) / dreq, 2)
        res["guest_tx_pkts_per_req"] = round((n1["tx_packets"] - n0["tx_packets"]) / dreq, 2)
    g.kill()
    return res


def sec_http():
    cases = []
    k10 = lambda: qemu_cmd()  # noqa: E731
    linux = lambda: qemu_cmd(mem="128M", kernel=LINUX_KERNEL, initrd="out/linux/initramfs-http.cpio",  # noqa: E731
                             append="console=ttyS0 quiet panic=-1 k10mode=http", debug_exit=False)
    k10_nospin = lambda: qemu_cmd(append="k10.spin_us=0")  # noqa: E731
    for conns in (1, 16, 64):
        cases.append(("k10-metal hello", k10, "k10-metal ready", "/hello/", conns, "GET", None, True))
        cases.append(("k10-metal hello spin_us=0", k10_nospin, "k10-metal ready", "/hello/", conns, "GET", None, True))
        cases.append(("linux-guest hello", linux, "ready (http)", "/", conns, "GET", None, False))
        cases.append(("host-native echo (no VM)", None, None, "/", conns, "GET", None, False))
    for conns in (1, 64):
        cases.append(("k10-metal echo POST 1KiB", k10, "k10-metal ready", "/echo/", conns, "POST", "x" * 1024, True))
        cases.append(("k10-metal compute n=1000", k10, "k10-metal ready", "/compute?n=1000", conns, "GET", None, True))
        cases.append(("k10-metal chain (call echo)", k10, "k10-metal ready", "/chain", conns, "GET", None, True))
        cases.append(("k10-metal counter (kv)", k10, "k10-metal ready", "/counter", conns, "GET", None, True))
    results = {}
    for rep in range(REPS):
        for label, mk, marker, path, conns, method, body, gs in cases:
            key = f"{label} c={conns}"
            if mk is None:
                env = dict(os.environ, PORT=str(PORT + 2))
                p = subprocess.Popen([os.path.abspath("../bench/bin/echo")], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                time.sleep(0.3)
                lg = loadgen(PORT + 2, path, conns)
                p.kill()
                p.wait()
                res = {"rps": round(lg["rps"]), "p50_us": lg["latency_us"]["p50"], "p99_us": lg["latency_us"]["p99"], "errors": lg["errors"]}
            else:
                res = run_http_case(key, mk(), marker, PORT, path, conns, method, body, gs)
            results.setdefault(key, []).append(res)
            print(f"rep {rep} {key}: {res}", flush=True)
    summary = {}
    for key, runs in results.items():
        summary[key] = {k: med([r.get(k) for r in runs]) for k in runs[0]}
        summary[key]["runs"] = len(runs)
    print(json.dumps(summary, indent=1))
    save("http", {"summary": summary, "raw": results})


def sec_spin():
    """`hello` load alone vs. with 4 connections hammering the infinite-loop service (100 ms CPU budget each)."""
    r = {}
    for rep in range(REPS):
        for with_spin in (False, True):
            g = Guest(qemu_cmd(), "k10-metal ready")
            spin = None
            if with_spin:
                spin = subprocess.Popen([LOADGEN, "-addr", f"127.0.0.1:{PORT}", "-path", "/spin", "-conns", "4",
                                         "-duration", f"{DUR + 2}s", "-warmup", "0s", "-timeout", "10s"],
                                        stdout=subprocess.PIPE, text=True)
                time.sleep(0.5)
            lg = loadgen(PORT, "/hello/", 16)
            res = {"rps": round(lg["rps"]), "p50_us": lg["latency_us"]["p50"], "p99_us": lg["latency_us"]["p99"],
                   "max_us": lg["latency_us"]["max"], "errors": lg["errors"]}
            if spin:
                so = json.loads(spin.communicate()[0])
                st = status(PORT)
                sp = [s for s in st["services"] if s["name"] == "spin"][0]
                res["spin_requests"] = so["requests"] + sum(so["errors_by_kind"].values())
                res["spin_interrupted"] = sp["interrupted"]
                res["preemptions"] = st["counters"]["preemptions"]
            g.kill()
            key = "hello c=16 with 4 spinning connections" if with_spin else "hello c=16 alone"
            r.setdefault(key, []).append(res)
            print(rep, key, res, flush=True)
    summary = {k: {f: med([x.get(f) for x in v]) for f in v[0]} for k, v in r.items()}
    print(json.dumps(summary, indent=1))
    save("spin", {"summary": summary, "raw": r})


def sec_engines():
    """JIT vs interpreter on the same services, interleaved (REPS boots per engine):
    * per-invocation CPU time measured inside the guest with `/_k10s/bench/<n>/<service>/...` (n invocations back to
      back in one task; TSC time while the invocation runs, host calls included, network excluded): min and median;
    * end-to-end latency and throughput with one keep-alive connection (loadgen, DUR s);
    * JIT compile time and code size per module, and kernel entry -> ready with the 18-service bundle."""
    workloads = [("hello", "/hello/", 2000), ("compute n=1000 (xorshift, builder)", "/compute?n=1000", 500),
                 ("sha n=1000 (Ten SHA-256 chain)", "/sha/?n=1000", 40), ("nbody n=1000 (Ten f64)", "/nbody/?n=1000", 40),
                 ("sort n=50000 (Ten merge sort)", "/sort/?n=50000", 10), ("todo GET (Ten KV + JSON)", "/todo/", 2000)]
    raw = {}
    compile_info = {}
    for rep in range(REPS):
        for engine in ("jit", "interp"):
            g = Guest(qemu_cmd(append=f"k10.engine={engine}"), "k10-metal ready")
            assert g.ready is not None, engine
            time.sleep(0.05)  # the rest of the ready line (the console is slow)
            m = re.search(r"ready ([0-9.]+)ms", g.drain())
            raw.setdefault(f"{engine} kmain->ready ms", []).append(float(m.group(1)) if m else None)
            st = status(PORT)
            for sv in st["services"]:
                ci = compile_info.setdefault(sv["name"], {"tir_bytes": sv["code_bytes"]})
                if engine == "jit":
                    ci["native_bytes"] = sv["native_bytes"]
                    ci.setdefault("compile_us", []).append(sv["compile_us"])
            for label, path, n in workloads:
                if engine == "interp":
                    n = max(5, n // 10)
                b = json.loads(http(PORT, f"/_k10s/bench/{n}{path}", timeout=120)[1])
                lg = loadgen(PORT, path, 1, dur=DUR, warmup="200ms")
                res = {"cpu_min_us": b["min_us"], "cpu_p50_us": b["p50_us"], "bench_steal_ms": b["steal_ms"],
                       "rps_c1": round(lg["rps"]), "p50_us": lg["latency_us"]["p50"], "p99_us": lg["latency_us"]["p99"],
                       "errors": lg["errors"], "loadavg": float(loadavg())}
                raw.setdefault(f"{engine} {label}", []).append(res)
                print(f"rep {rep} {engine} {label}: {res}", flush=True)
            g.kill()
    summary = {}
    for k, runs in raw.items():
        if k.endswith("ms"):
            summary[k] = med(runs)
        else:
            summary[k] = {f: med([r.get(f) for r in runs]) for f in runs[0]}
    for label, _, _ in workloads:
        j, i = summary[f"jit {label}"], summary[f"interp {label}"]
        summary[f"speedup {label}"] = {
            "cpu_min": round(i["cpu_min_us"] / j["cpu_min_us"], 1) if j["cpu_min_us"] else None,
            "cpu_p50": round(i["cpu_p50_us"] / j["cpu_p50_us"], 1) if j["cpu_p50_us"] else None,
            "latency_p50": round(i["p50_us"] / j["p50_us"], 1) if j["p50_us"] else None,
        }
    for v in compile_info.values():
        if "compile_us" in v:
            v["compile_us"] = med(v["compile_us"])
    summary["code"] = compile_info
    print(json.dumps(summary, indent=1))
    save("engines", {"summary": summary, "raw": raw})


def sec_smp():
    """Throughput scaling with -smp 1/2/4 (JIT): a CPU-bound service (SHA-256 chain, ~0.9 ms per request) and
    `hello` (network-bound), closed loop, interleaved; per-CPU invocation counts and busy time from the guest."""
    cases = [("sha n=1000, 16 conns", "/sha/?n=1000", 16), ("nbody n=1000, 16 conns", "/nbody/?n=1000", 16),
             ("hello, 64 conns", "/hello/", 64)]
    raw = {}
    for rep in range(REPS):
        for n in (1, 2, 4):
            g = Guest(qemu_cmd(mem="128M", smp=n), "k10-metal ready")
            assert g.ready is not None
            for label, path, conns in cases:
                http(PORT, path)
                st0 = status(PORT)
                c0 = g.threads_cpu()
                lg = loadgen(PORT, path, conns)
                c1 = g.threads_cpu()
                st1 = status(PORT)
                up = st1["uptime_ms"] - st0["uptime_ms"]
                busy = []
                for a, b in zip(st0["cpus"], st1["cpus"]):
                    busy.append(round(100 * (1 - (b["halted_ms"] - a["halted_ms"]) / up), 1))
                tasks = [b["tasks_run"] - a["tasks_run"] for a, b in zip(st0["cpus"], st1["cpus"])]
                vcpu = sum(v - c0.get(k, 0) for k, v in c1.items() if "CPU" in k)
                res = {"rps": round(lg["rps"]), "p50_us": lg["latency_us"]["p50"], "p99_us": lg["latency_us"]["p99"],
                       "errors": lg["errors"], "cpus_online": len(st1["cpus"]), "tasks_per_cpu": tasks,
                       "busy_pct_per_cpu": busy, "vcpu_threads_cpu_s": round(vcpu, 2), "loadavg": float(loadavg())}
                raw.setdefault(f"smp{n} {label}", []).append(res)
                print(f"rep {rep} smp{n} {label}: {res}", flush=True)
            g.kill()
    summary = {}
    for k, runs in raw.items():
        summary[k] = {f: med([r[f] for r in runs]) for f in ("rps", "p50_us", "p99_us", "errors", "loadavg")}
        summary[k]["tasks_per_cpu"] = runs[len(runs) // 2]["tasks_per_cpu"]
        summary[k]["busy_pct_per_cpu"] = runs[len(runs) // 2]["busy_pct_per_cpu"]
    for label, _, _ in cases:
        base = summary[f"smp1 {label}"]["rps"]
        for n in (2, 4):
            summary[f"smp{n} {label}"]["scaling_vs_smp1"] = round(summary[f"smp{n} {label}"]["rps"] / base, 2) if base else None
    print(json.dumps(summary, indent=1))
    save("smp", {"summary": summary, "raw": raw})


def sec_linux():
    r = {"kernel": LINUX_KERNEL, "kernel_bytes": os.path.getsize(LINUX_KERNEL),
         "initramfs_boot_bytes": os.path.getsize("out/linux/initramfs-boot.cpio"),
         "initramfs_http_bytes": os.path.getsize("out/linux/initramfs-http.cpio")}
    for mode, initrd, marker, net in (("boot", "out/linux/initramfs-boot.cpio", "linux-guest ready", False),
                                      ("http", "out/linux/initramfs-http.cpio", "ready (http)", True)):
        for fw, bios in (("seabios", None), ("qboot", QBOOT)):
            ready, first = [], []
            for _ in range(RUNS):
                g = Guest(qemu_cmd(mem="128M", kernel=LINUX_KERNEL, initrd=initrd, net=net, bios=bios, debug_exit=False,
                                   append=f"console=ttyS0 quiet panic=-1 k10mode={mode}"), marker)
                ready.append(g.ready * 1000 if g.ready else None)
                if net and g.ready:
                    t = None
                    while time.monotonic() - g.t0 < 10:
                        try:
                            if http(PORT, "/", timeout=1.0)[0] == 200:
                                t = (time.monotonic() - g.t0) * 1000
                                break
                        except OSError:
                            time.sleep(0.001)
                    first.append(t)
                g.kill()
            r[f"{mode}_{fw}"] = {"ready_ms_median": med(ready), "ready_ms_min": round(min(x for x in ready if x), 1),
                                 "first_http_200_ms_median": med(first) if first else None,
                                 "samples_ready_ms": [round(x, 1) if x else None for x in ready]}
            print(mode, fw, r[f"{mode}_{fw}"], flush=True)
    save("linux", r)
    sec_linux_ram(r)


def sec_linux_ram(r=None):
    if r is None:
        r = json.load(open(f"{OUT}/linux.json")) if os.path.exists(f"{OUT}/linux.json") else {}
    # Minimum RAM: boot-only and serving.
    for mode, initrd, marker, net in (("boot", "out/linux/initramfs-boot.cpio", "linux-guest ready", False),
                                      ("http", "out/linux/initramfs-http.cpio", "ready (http)", True)):
        res = {}
        for mem in ["32M", "48M", "64M", "72M", "80M", "88M", "96M", "128M"]:
            g = Guest(qemu_cmd(mem=mem, kernel=LINUX_KERNEL, initrd=initrd, net=net, debug_exit=False,
                               append=f"console=ttyS0 quiet panic=-1 k10mode={mode}"), marker, timeout=20)
            ok = g.ready is not None
            if ok and net:
                try:
                    ok = http(PORT, "/")[0] == 200 and loadgen(PORT, "/", 16, dur=2, warmup="200ms")["errors"] == 0
                except Exception:  # noqa: BLE001
                    ok = False
            res[mem] = ok
            g.kill()
            print("linux", mode, mem, ok, flush=True)
        r[f"min_ram_{mode}"] = next((m for m, v in res.items() if v), None)
        r[f"ram_{mode}"] = res
    save("linux", r)


SECTIONS = {"size": sec_size, "boot": sec_boot, "ram": sec_ram, "mem": sec_mem, "http": sec_http, "spin": sec_spin,
            "engines": sec_engines, "smp": sec_smp, "linux": sec_linux, "linux_ram": sec_linux_ram}

if __name__ == "__main__":
    print(f"host load average: {loadavg()}")
    for s in sys.argv[1:] or list(SECTIONS):
        print(f"== {s}", flush=True)
        SECTIONS[s]()
