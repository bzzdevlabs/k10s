#!/usr/bin/env bash
# Interleaved boot measurements of every x86-64 variant (QEMU/KVM, 1 vCPU, user networking): N rounds, each round
# runs every variant once, so host load hits all variants alike. Results: runs/final/<variant>-<round>.json
set -u
cd "$(dirname "$0")/.."
N=${N:-7}; out=runs/final; mkdir -p $out
B="python3 scripts/boot.py --mem 256M --timeout 60 --quiet"
Q="console=ttyS0 quiet panic=-1"
run() { local name=$1; shift; $B --json $out/$name-$r.json "$@" > /dev/null; }
for r in $(seq $N); do
  run metal          --kernel img/k10-metal.elf --initrd img/bundle.k10b --append "" --ready "k10-metal ready" --health /hello/ --legacy-virtio
  run deb-qemu       --kernel /boot/vmlinuz-6.1.0-53-amd64 --initrd img/deb-qemu.cpio.zst --append "$Q"
  run deb-x86u       --kernel /boot/vmlinuz-6.1.0-53-amd64 --initrd img/deb-x86u.cpio.zst --append "$Q"
  run min-pid1       --kernel img/k10s-x86_64-bzImage --initrd img/min-pid1.cpio.zst --append "$Q"
  run min-unmod      --kernel img/k10s-x86_64-bzImage --initrd img/min-unmod.cpio.zst --append "$Q ip=dhcp -- node --http 0.0.0.0:80 --data /data"
  run disk-min-bios  --boot-disk img/disk-min-x86_64.img
  run disk-deb-bios  --boot-disk img/disk-deb-x86_64.img
  run i386-pid1      --rng --arch i386 --kernel img/k10s-i386-bzImage --initrd img/i386-pid1.cpio.zst --append "$Q"
  if [ $r -le 3 ]; then
    run disk-min-uefi --boot-disk img/disk-min-x86_64.img --uefi /usr/share/OVMF/OVMF_CODE_4M.fd --uefi-vars /usr/share/OVMF/OVMF_VARS_4M.fd
    run uki-uefi      --boot-disk img/disk-uki-x86_64.img --uefi /usr/share/OVMF/OVMF_CODE_4M.fd --uefi-vars /usr/share/OVMF/OVMF_VARS_4M.fd
  fi
  echo "round $r done, load $(cut -d' ' -f1-3 /proc/loadavg)"
done
python3 - <<'PY'
import glob, json, re, statistics, collections
res = collections.defaultdict(list); mem = collections.defaultdict(list); loads = []
for f in sorted(glob.glob("runs/final/*.json")):
    name = re.sub(r"-\d+\.json$", "", f.split("/")[-1])
    d = json.load(open(f)); r = d["runs"][0]; loads.append(float(r["load"][0]))
    if "first_200_ms" in r: res[name].append(r["first_200_ms"])
    for l in r["init_lines"]:
        m = re.search(r"mem: total (\d+) KiB, used (\d+) KiB.*?free (\d+) KiB.*?k10s RSS (\d+) KiB", l)
        if m and "ready" in l: mem[name].append(tuple(int(x) for x in m.groups()))
for n, v in res.items():
    mm = mem.get(n)
    ms = ""
    if mm:
        t, u, fr, rss = [statistics.median(x[i] for x in mm) for i in range(4)]
        ms = f"MemTotal {t/1024:.1f} MiB, total-avail {u/1024:.1f}, total-free {(t-fr)/1024:.1f}, k10s RSS {rss/1024:.2f}"
    print(f"{n:15s} n={len(v)} median {statistics.median(v):7.1f} ms  min {min(v):7.1f}  max {max(v):7.1f}  {ms}")
print("load during runs: min %.1f median %.1f max %.1f" % (min(loads), statistics.median(loads), max(loads)))
PY
