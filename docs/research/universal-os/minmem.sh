#!/usr/bin/env bash
# Smallest -m that boots, applies the 8 Ten examples and serves /hello/ and /compute/?n=1000 (2 tries per size).
#   minmem.sh KERNEL INITRD "APPEND" SIZE... [extra boot.py args via EXTRA=]
cd "$(dirname "$0")/.."
k=$1 i=$2 ap=$3; shift 3
for m in "$@"; do
  res=FAIL
  for try in 1 2; do
    out=$(python3 scripts/boot.py --kernel "$k" --initrd "$i" --append "$ap" --mem $m --timeout 40 \
          --apply wt/examples/app.k10s --check /hello/ --check '/compute/?n=1000' --idle 1 ${EXTRA:-})
    if echo "$out" | python3 -c '
import json,sys; d=json.load(sys.stdin); r=d["runs"][0]
ok = r.get("apply_ok") and all(v[0]==200 for v in r.get("checks",{}).values()) and len(r.get("checks",{}))==2
sys.exit(0 if ok else 1)'; then res=OK; break; fi
  done
  mem=$(echo "$out" | python3 -c 'import json,sys; r=json.load(sys.stdin)["runs"][0]; print(r.get("last_mem","")[:200] or r.get("error","")[:200] or r.get("apply_err","")[:200])')
  echo "-m $m: $res  $mem"
done
