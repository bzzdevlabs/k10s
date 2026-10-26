#!/usr/bin/env python3
"""Builds OCI image layouts and docker-save archives from files, without Docker (scripts/oci-testimages.sh).

  ocilayout.py SPEC.json OUT
SPEC: {"images": [{"name": "k10s-test/echo", "tag": "v1", "arch": "amd64", "files": [[DST, SRC, MODE], ...],
       "dirs": [[DST, MODE], ...], "symlinks": [[DST, TARGET], ...], "config": {"Entrypoint": [...], ...}}, ...]}
Images sharing name and tag are one multi-arch index. Output:
  OUT/layout/            one OCI layout (oci-layout, index.json, blobs/sha256) with every image, gzip layers
  OUT/archives/NAME.tar  per name:tag, the linux/amd64 image as `docker save` writes it (Docker 25+: an OCI layout
                         with uncompressed layers plus manifest.json)
Everything is deterministic: entries sorted, owner 0:0, mtime 0, gzip without name or time.
"""
import gzip
import hashlib
import io
import json
import os
import sys
import tarfile
import tempfile

OCI_INDEX = "application/vnd.oci.image.index.v1+json"
OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
OCI_CONFIG = "application/vnd.oci.image.config.v1+json"
LAYER_GZ = "application/vnd.oci.image.layer.v1.tar+gzip"
LAYER_TAR = "application/vnd.oci.image.layer.v1.tar"


class Blobs:
    def __init__(self, root):
        self.root = root
        os.makedirs(os.path.join(root, "blobs", "sha256"), exist_ok=True)

    def put(self, data, media_type, **extra):
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        p = os.path.join(self.root, "blobs", "sha256", digest[7:])
        if not os.path.exists(p):
            with open(p + ".part", "wb") as f:
                f.write(data)
            os.replace(p + ".part", p)
        return dict(mediaType=media_type, digest=digest, size=len(data), **extra)

    def put_json(self, obj, media_type, **extra):
        return self.put(canonical(obj), media_type, **extra)


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()


def layer_tar(img):
    """The image's single layer, uncompressed, with every parent directory present."""
    entries = {}
    for dst, mode in img.get("dirs", []):
        entries[dst.strip("/")] = ("dir", int(mode), None)
    for dst, src, mode in img.get("files", []):
        entries[dst.strip("/")] = ("file", int(mode), src)
    for dst, target in img.get("symlinks", []):
        entries[dst.strip("/")] = ("link", 0o777, target)
    for p in list(entries):
        d = os.path.dirname(p)
        while d and d not in entries:
            entries[d] = ("dir", 0o755, None)
            d = os.path.dirname(d)
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tf:
        for p in sorted(entries, key=lambda p: p.split("/")):
            kind, mode, arg = entries[p]
            ti = tarfile.TarInfo(p + ("/" if kind == "dir" else ""))
            ti.mtime, ti.uid, ti.gid, ti.uname, ti.gname, ti.mode = 0, 0, 0, "", "", mode
            if kind == "dir":
                ti.type = tarfile.DIRTYPE
                tf.addfile(ti)
            elif kind == "link":
                ti.type, ti.linkname = tarfile.SYMTYPE, arg
                tf.addfile(ti)
            else:
                data = open(arg, "rb").read()
                ti.size = len(data)
                tf.addfile(ti, io.BytesIO(data))
    return buf.getvalue()


def gzipped(data):
    out = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=out, compresslevel=9, mtime=0) as g:
        g.write(data)
    return out.getvalue()


def config_blob(img, raw):
    cfg = {
        "architecture": img["arch"], "os": "linux", "created": "1970-01-01T00:00:00Z",
        "config": img.get("config", {}),
        "rootfs": {"type": "layers", "diff_ids": ["sha256:" + hashlib.sha256(raw).hexdigest()]},
        "history": [{"created": "1970-01-01T00:00:00Z", "created_by": "scripts/oci-testimages.sh"}],
    }
    if img["arch"] == "arm64":
        cfg["variant"] = "v8"
    return cfg


def image(blobs, img, compressed):
    raw = layer_tar(img)
    layer = blobs.put(gzipped(raw), LAYER_GZ) if compressed else blobs.put(raw, LAYER_TAR)
    config = blobs.put_json(config_blob(img, raw), OCI_CONFIG)
    manifest = {"schemaVersion": 2, "mediaType": OCI_MANIFEST, "config": config, "layers": [layer]}
    platform = {"architecture": img["arch"], "os": "linux"}
    if img["arch"] == "arm64":
        platform["variant"] = "v8"
    return blobs.put_json(manifest, OCI_MANIFEST, platform=platform), config, layer


def write_layout_files(root, manifests):
    open(os.path.join(root, "oci-layout"), "w").write('{"imageLayoutVersion":"1.0.0"}')
    index = {"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests}
    open(os.path.join(root, "index.json"), "wb").write(canonical(index))


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    spec, out = json.load(open(sys.argv[1])), sys.argv[2]
    groups = {}
    for img in spec["images"]:
        groups.setdefault((img["name"], img["tag"]), []).append(img)
    layout = Blobs(os.path.join(out, "layout"))
    top = []
    os.makedirs(os.path.join(out, "archives"), exist_ok=True)
    for (name, tag), imgs in sorted(groups.items()):
        ann = {"org.opencontainers.image.ref.name": tag, "io.containerd.image.name": f"{name}:{tag}"}
        descs = [image(layout, img, True)[0] for img in sorted(imgs, key=lambda i: i["arch"])]
        if len(descs) == 1:
            d = dict(descs[0])
            d.pop("platform")
            top.append(dict(d, annotations=ann))
        else:
            idx = {"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": descs}
            top.append(layout.put_json(idx, OCI_INDEX, annotations=ann))
        amd64 = [i for i in imgs if i["arch"] == "amd64"]
        if amd64:
            write_archive(os.path.join(out, "archives", name.split("/")[-1] + ".tar"), amd64[0], name, tag, ann)
    write_layout_files(os.path.join(out, "layout"), top)
    print(f"ocilayout: {len(top)} image(s) in {out}/layout, archives in {out}/archives")


def write_archive(path, img, name, tag, ann):
    """`docker save` form: an OCI layout with uncompressed layers plus the legacy manifest.json."""
    with tempfile.TemporaryDirectory(dir=os.path.dirname(path)) as td:
        blobs = Blobs(td)
        m, config, layer = image(blobs, img, False)
        m = dict(m)
        m.pop("platform")
        write_layout_files(td, [dict(m, annotations=ann)])
        legacy = [{"Config": "blobs/sha256/" + config["digest"][7:], "RepoTags": [f"{name}:{tag}"],
                   "Layers": ["blobs/sha256/" + layer["digest"][7:]]}]
        open(os.path.join(td, "manifest.json"), "wb").write(canonical(legacy))
        with tarfile.open(path + ".part", "w", format=tarfile.PAX_FORMAT) as tf:
            for root, dirs, files in os.walk(td):
                dirs.sort()
                for f in sorted(files) + [d + "/" for d in dirs]:
                    full = os.path.join(root, f.rstrip("/"))
                    rel = os.path.relpath(full, td) + ("/" if f.endswith("/") else "")
                    ti = tarfile.TarInfo(rel)
                    ti.mtime, ti.uid, ti.gid, ti.uname, ti.gname = 0, 0, 0, "", ""
                    if f.endswith("/"):
                        ti.type, ti.mode = tarfile.DIRTYPE, 0o755
                        tf.addfile(ti)
                    else:
                        data = open(full, "rb").read()
                        ti.size, ti.mode = len(data), 0o644
                        tf.addfile(ti, io.BytesIO(data))
        os.replace(path + ".part", path)


if __name__ == "__main__":
    main()
