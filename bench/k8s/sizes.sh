#!/usr/bin/env bash
# Artifact sizes: workload images/binaries, node image, images inside the node (crictl), node rootfs data.
source "$(dirname "$0")/lib.sh"
CP=$(CP_NODE)
: > "$TMP/runs.jsonl"
python3 - "$BENCH_DIR" "$NODE_IMAGE" "$CP" >> "$TMP/runs.jsonl" <<'PY'
import json,sys,subprocess,os
bench,nimg,cp=sys.argv[1:4]
sh=lambda *a: subprocess.run(a,capture_output=True,text=True).stdout
out={"binaries_bytes":{n:os.path.getsize(f"{bench}/bin/{n}") for n in ("echo","compute","chain","counter","loadgen")}}
out["docker_images_bytes"]={}
for n in ("echo:v1","compute:v1","chain:v1","counter:v1"):
    out["docker_images_bytes"][n]=int(sh("docker","image","inspect","-f","{{.Size}}",f"k10s-bench/{n}").strip() or 0)
out["kind_node_image_bytes"]=int(sh("docker","image","inspect","-f","{{.Size}}",nimg.split("@")[0]).strip() or 0)
out["kind_node_image_docker_images_size"]=sh("docker","images","--format","{{.Size}}",nimg.split("@")[0]).strip()
imgs=json.loads(sh("docker","exec",cp,"crictl","images","-o","json") or "{}").get("images",[])
out["images_in_node"]={(i["repoTags"] or i["repoDigests"] or [i["id"]])[0]:int(i["size"]) for i in imgs}
out["node_containerd_dir_kib"]=int(sh("docker","exec",cp,"sh","-c","du -sk /var/lib/containerd | cut -f1").strip() or 0)
out["node_etcd_dir_kib"]=int(sh("docker","exec",cp,"sh","-c","du -sk /var/lib/etcd | cut -f1").strip() or 0)
out["k8s_binaries_in_node_bytes"]={b:int(sh("docker","exec",cp,"sh","-c",f"stat -L -c %s $(command -v {b} || echo /usr/local/bin/{b} || echo /dev/null) 2>/dev/null || echo 0").strip() or 0) for b in ("kubelet","kubeadm","kubectl","containerd","runc","crictl")}
print(json.dumps(out))
PY
write_result sizes
