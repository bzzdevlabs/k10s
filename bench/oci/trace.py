#!/usr/bin/env python3
"""Runs each corpus image (corpus.json) under lxtrace in a QEMU/KVM Linux guest: the merged image is the guest's
initramfs, lxtrace its /init. Results: target/oci/runs/NAME.json (the summary), NAME.static.json (elfscan.py) and
NAME.console.

  trace.py --kernel VMLINUZ --lxtrace BIN [--corpus FILE] [NAME ...]
"""
import argparse
import json
import os
import signal
import subprocess
import sys

import elfscan
import oci

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = os.path.join(oci.CACHE, "runs")
SIGNALS = {s.name: int(s) for s in signal.Signals}


def plan(entry, cfg):
    c = cfg.get("config", {})
    env = list(c.get("Env") or [])
    if not any(e.startswith("PATH=") for e in env):
        env.append("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
    env += [f"HOSTNAME={entry['name']}"] + entry.get("env", [])
    if not any(e.startswith("HOME=") for e in env):
        env.append("HOME=/root")
    argv = list(entry.get("entrypoint", c.get("Entrypoint") or [])) + list(entry.get("cmd", c.get("Cmd") or []))
    stop = SIGNALS[c.get("StopSignal") or "SIGTERM"]
    lines = [f"hostname\t{entry['name']}", f"timeout\t{entry.get('timeout', 300)}"]
    lines += [f"env\t{e}" for e in env]
    if c.get("WorkingDir"):
        lines.append(f"cwd\t{c['WorkingDir']}")
    user = c.get("User") or ""
    if user:
        uid, _, gid = user.partition(":")
        if not (uid.isdigit() and (gid or uid).isdigit()):
            raise SystemExit(f"{entry['name']}: User {user!r} is not numeric")
        lines.append(f"user\t{uid}\t{gid or uid}")
    lines.append("run\t" + "\t".join(argv))
    for s in entry["steps"]:
        s = ["signal", str(stop)] if s == ["stop"] else s
        lines.append("step\t" + "\t".join(s))
    text = "\n".join(lines) + "\n"
    if any(ch in "".join(argv + env) for ch in "\n\t"):
        raise SystemExit(f"{entry['name']}: tab or newline in argv/env")
    return text


def run(entry, kernel, lxtrace):
    digest, m, cfg = oci.pull(entry["image"])
    os.makedirs(RUNS, exist_ok=True)
    work = os.path.join(oci.tmpdir(), entry["name"])
    os.makedirs(work, exist_ok=True)
    plan_path = os.path.join(work, "plan")
    open(plan_path, "w").write(plan(entry, cfg))
    extra = {"/k10s/lxtrace": (lxtrace, 0o755), "/k10s/plan": (plan_path, 0o644)}
    for dst, src in entry.get("files", {}).items():
        extra[dst] = (os.path.join(HERE, src), 0o755)
    cpio = os.path.join(work, "initrd.cpio")
    oci.rootfs(entry["image"], cpio, extra)
    console, out = (os.path.join(RUNS, entry["name"] + s) for s in (".console", ".serial"))
    timeout = entry.get("timeout", 300) + 120
    cmd = ["qemu-system-x86_64", "-M", "pc", "-enable-kvm", "-cpu", "host", "-smp", "2",
           "-m", str(entry.get("memory", 3072)), "-nodefaults", "-display", "none", "-no-reboot",
           "-serial", f"file:{console}", "-serial", f"file:{out}", "-kernel", kernel, "-initrd", cpio,
           "-append", "console=ttyS0 panic=-1 rdinit=/k10s/lxtrace"]
    try:
        subprocess.run(cmd, check=True, timeout=timeout)
    finally:
        os.unlink(cpio)
    text = open(out, "rb").read().decode("utf-8", "replace").replace("\r", "")
    if "--- lxtrace json ---" not in text:
        raise SystemExit(f"{entry['name']}: no summary (see {console})")
    body = text.split("--- lxtrace json ---\n", 1)[1].split("--- lxtrace end ---", 1)[0]
    summary = json.loads(body)
    summary.update(name=entry["name"], tag=entry["tag"], image=entry["image"], manifest=digest,
                   layers_bytes=sum(x["size"] for x in m["layers"]))
    json.dump(summary, open(os.path.join(RUNS, entry["name"] + ".json"), "w"), indent=1, sort_keys=True)
    os.unlink(out)
    calls = {nr for p in summary["programs"].values() for nr in p["syscalls"]}
    print(f"{entry['name']}: exit {summary.get('exit')}, killed {summary.get('killed')}, "
          f"{len(calls)} distinct syscalls, {summary.get('elapsed_ms')} ms", flush=True)
    for d in summary.get("driver", []):
        print("  " + d[:200], flush=True)
    static = elfscan.scan(entry["image"], os.path.join(RUNS, entry["name"] + ".json"))
    json.dump(static, open(os.path.join(RUNS, entry["name"] + ".static.json"), "w"), indent=1, sort_keys=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--lxtrace", required=True)
    ap.add_argument("--corpus", default=os.path.join(HERE, "corpus.json"))
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    corpus = json.load(open(a.corpus))
    for e in corpus:
        if not a.names or e["name"] in a.names:
            run(e, a.kernel, a.lxtrace)


if __name__ == "__main__":
    sys.exit(main())
