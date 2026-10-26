#!/usr/bin/env python3
"""OCI image fetch and root file system packing for the syscall sizing (bench/oci/trace.sh). Test tooling only:
the distribution API is spoken through curl (bearer-token flow included), every blob is checked against its digest
and cached under target/oci/blobs, and an image's layers are merged (whiteouts, opaque directories) straight into a
newc cpio, so ownership and modes survive without root.

  oci.py resolve REPO:TAG                  prints the digest of the tag (index or manifest)
  oci.py pull REPO@DIGEST                  fetches the linux/amd64 manifest, config and layers into the cache
  oci.py config REPO@DIGEST                prints the image config (Entrypoint, Cmd, Env, User, ...)
  oci.py rootfs REPO@DIGEST OUT.cpio [DST=SRC ...]   merged root file system plus extra files
REPO is docker.io/library/nginx, ghcr.io/owner/name, ...; 127.0.0.1:PORT/NAME is a test registry over plain HTTP.
K10S_OCI_AUTH=USER:PASS is sent as HTTP Basic to the token endpoint (through curl's stdin, not its command line).
"""
import gzip
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.environ.get("K10S_OCI_CACHE", os.path.join(HERE, "..", "..", "target", "oci"))
INDEX_TYPES = ["application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json"]
MANIFEST_TYPES = ["application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"]
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
MAX_MANIFEST = 4 << 20


def split_ref(ref):
    if "@" in ref:
        repo, ref_ = ref.split("@", 1)
        if not DIGEST.match(ref_):
            raise SystemExit(f"bad digest in {ref}")
    else:
        repo, ref_ = ref.rsplit(":", 1)
    host, name = repo.split("/", 1)
    if host == "docker.io":
        host = "registry-1.docker.io"
    return host, name, ref_


def loopback(host):
    """Plain HTTP is spoken only to a test registry on this machine (os/tools/registry.py)."""
    return host.split(":")[0] in ("127.0.0.1", "localhost")


class Registry:
    def __init__(self, host, name):
        self.host, self.name, self.token = host, name, None
        self.scheme = "http" if loopback(host) else "https"

    def get(self, path, accept=None, out=None, head=False):
        """GET (or HEAD) https://host/v2/NAME/path; returns (status, headers, body or None when written to out)."""
        url = f"{self.scheme}://{self.host}/v2/{self.name}/{path}"
        for attempt in (0, 1):
            with tempfile.TemporaryDirectory(dir=tmpdir()) as td:
                hdr, body = os.path.join(td, "h"), out or os.path.join(td, "b")
                cmd = ["curl", "-sS", "-L", "--proto", f"={self.scheme}", "--max-time", "1800", "-D", hdr, "-o", body,
                       "-w", "%{http_code}"]
                cmd += ["-I"] if head else []
                for a in accept or []:
                    cmd += ["-H", f"Accept: {a}"]
                if self.token:
                    cmd += ["-H", f"Authorization: Bearer {self.token}"]
                code = int(subprocess.run(cmd + [url], check=True, capture_output=True, text=True).stdout or 0)
                headers = parse_headers(open(hdr, "rb").read().decode("latin-1"))
                if code == 401 and attempt == 0 and "www-authenticate" in headers:
                    self.auth(headers["www-authenticate"])
                    continue
                data = None
                if out is None and not head:
                    if os.path.getsize(body) > MAX_MANIFEST:
                        raise SystemExit(f"{url}: response over {MAX_MANIFEST} bytes")
                    data = open(body, "rb").read()
                return code, headers, data
        raise SystemExit(f"{url}: unauthorized")

    def auth(self, challenge):
        m = re.match(r"\s*Bearer\s+(.*)", challenge, re.I)
        if not m:
            raise SystemExit(f"unsupported auth challenge {challenge!r}")
        params = dict(re.findall(r'(\w+)="([^"]*)"', m.group(1)))
        realm = params.pop("realm")
        params.setdefault("scope", f"repository:{self.name}:pull")
        if not realm.startswith(f"{self.scheme}://"):
            raise SystemExit(f"token realm {realm!r} is not {self.scheme}")
        q = "&".join(f"{k}={v}" for k, v in params.items())
        cred = os.environ.get("K10S_OCI_AUTH")
        r = subprocess.run(["curl", "-fsS", "--proto", f"={self.scheme}", "--max-time", "60"] +
                           (["-K", "-"] if cred else []) + [f"{realm}?{q}"],
                           input=f'user = "{cred}"\n'.encode() if cred else None, check=True, capture_output=True)
        tok = json.loads(r.stdout)
        self.token = tok.get("token") or tok.get("access_token")


def parse_headers(text):
    # With redirects, the last response's headers win.
    h = {}
    for line in text.splitlines():
        if line.startswith("HTTP/"):
            h = {}
        elif ":" in line:
            k, v = line.split(":", 1)
            h[k.strip().lower()] = v.strip()
    return h


def tmpdir():
    d = os.path.join(CACHE, "tmp")
    os.makedirs(d, exist_ok=True)
    return d


def blob_path(digest):
    if not DIGEST.match(digest):
        raise SystemExit(f"bad digest {digest!r}")
    return os.path.join(CACHE, "blobs", "sha256", digest[7:])


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return "sha256:" + h.hexdigest()


def store(digest, data):
    if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
        raise SystemExit(f"digest mismatch for {digest}")
    p = blob_path(digest)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p + ".part", "wb") as f:
        f.write(data)
    os.replace(p + ".part", p)


def cached(digest):
    p = blob_path(digest)
    return open(p, "rb").read() if os.path.exists(p) else None


def manifest(reg, ref):
    if DIGEST.match(ref) and cached(ref) is not None:
        return ref, cached(ref)
    code, h, data = reg.get(f"manifests/{ref}", INDEX_TYPES + MANIFEST_TYPES)
    if code != 200:
        raise SystemExit(f"manifest {reg.name}:{ref}: HTTP {code}")
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    if DIGEST.match(ref) and digest != ref:
        raise SystemExit(f"manifest {ref}: content digest {digest}")
    store(digest, data)
    return digest, data


def fetch_blob(reg, desc):
    d = desc["digest"]
    p = blob_path(d)
    if os.path.exists(p):
        return p
    os.makedirs(os.path.dirname(p), exist_ok=True)
    code, _, _ = reg.get(f"blobs/{d}", out=p + ".part")
    if code != 200 or os.path.getsize(p + ".part") != desc["size"] or sha256_file(p + ".part") != d:
        os.unlink(p + ".part")
        raise SystemExit(f"blob {d}: HTTP {code} or size/digest mismatch")
    os.replace(p + ".part", p)
    return p


def pull(ref, platform=("linux", "amd64")):
    """Returns (manifest digest, manifest, config) with all blobs cached."""
    host, name, r = split_ref(ref)
    reg = Registry(host, name)
    digest, data = manifest(reg, r)
    m = json.loads(data)
    if m.get("mediaType") in INDEX_TYPES or "manifests" in m:
        picks = [d for d in m["manifests"]
                 if (d.get("platform", {}).get("os"), d.get("platform", {}).get("architecture")) == platform]
        if not picks:
            raise SystemExit(f"{ref}: no {'/'.join(platform)} manifest")
        digest, data = manifest(reg, picks[0]["digest"])
        m = json.loads(data)
    cfg = json.loads(open(fetch_blob(reg, m["config"]), "rb").read())
    for layer in m["layers"]:
        fetch_blob(reg, layer)
    return digest, m, cfg


def resolve(ref):
    host, name, tag = split_ref(ref)
    reg = Registry(host, name)
    code, h, _ = reg.get(f"manifests/{tag}", INDEX_TYPES + MANIFEST_TYPES, head=True)
    if code != 200 or not DIGEST.match(h.get("docker-content-digest", "")):
        raise SystemExit(f"{ref}: HTTP {code}")
    return h["docker-content-digest"]


def safe_name(name):
    parts = [p for p in name.split("/") if p not in ("", ".")]
    if any(p == ".." for p in parts):
        raise SystemExit(f"layer member {name!r} escapes the root")
    return "/".join(parts)


def open_layer(desc, td):
    """The layer as an uncompressed tar file (random access for hardlinks)."""
    src = blob_path(desc["digest"])
    mt = desc.get("mediaType", "")
    out = os.path.join(td, desc["digest"][7:] + ".tar")
    if mt.endswith("zstd"):
        subprocess.run(["zstd", "-qdc", "-o", out, src], check=True)
    elif mt.endswith("gzip") or mt.endswith("tar.gzip"):
        with gzip.open(src, "rb") as i, open(out, "wb") as o:
            shutil.copyfileobj(i, o, 1 << 20)
    elif mt.endswith("tar") or mt.endswith("diff.tar"):
        return src
    else:
        raise SystemExit(f"unsupported layer type {mt}")
    return out


def merge(layers):
    """path -> (tarfile, TarInfo) of the merged tree (OCI whiteouts and opaque directories applied)."""
    tree = {}

    def drop_under(d, below):
        pre = d + "/"
        for k in [k for k, (li, _, _) in tree.items() if (k == d or k.startswith(pre)) and li < below]:
            del tree[k]

    for li, tf in enumerate(layers):
        for ti in tf.getmembers():
            path = safe_name(ti.name)
            if not path:
                continue
            base, d = os.path.basename(path), os.path.dirname(path)
            if base == ".wh..wh..opq":
                for k in [k for k, (l2, _, _) in tree.items() if k.startswith(d + "/") and l2 < li]:
                    del tree[k]
                continue
            if base.startswith(".wh."):
                drop_under(os.path.join(d, base[4:]), li)
                continue
            old = tree.get(path)
            if old and not (old[2].isdir() and ti.isdir()):
                drop_under(path, li + 1)
            tree[path] = (li, tf, ti)
    return {k: (tf, ti) for k, (_, tf, ti) in tree.items()}


class Cpio:
    def __init__(self, f):
        self.f, self.ino = f, 1

    def new_ino(self):
        self.ino += 1
        return self.ino

    def entry(self, name, mode, uid=0, gid=0, mtime=0, size=0, data=None, rdev=(0, 0), nlink=1, ino=None):
        if size >= 1 << 32:
            raise SystemExit(f"{name}: {size} bytes does not fit a newc entry")
        fields = [ino or self.new_ino(), mode, uid, gid, nlink, int(mtime) & 0xffffffff, size, 0, 0, rdev[0], rdev[1],
                  len(name.encode()) + 1, 0]
        hdr = b"070701" + "".join("%08X" % x for x in fields).encode() + name.encode() + b"\0"
        self.f.write(hdr + b"\0" * (-len(hdr) % 4))
        n = 0
        if isinstance(data, bytes):
            self.f.write(data)
            n = len(data)
        elif data is not None:
            for chunk in iter(lambda: data.read(1 << 20), b""):
                self.f.write(chunk)
                n += len(chunk)
        if n != size:
            raise SystemExit(f"{name}: wrote {n} of {size} bytes")
        self.f.write(b"\0" * (-size % 4))

    def trailer(self):
        self.entry("TRAILER!!!", 0)


def rootfs(ref, out, extra):
    _, m, _ = pull(ref)
    with tempfile.TemporaryDirectory(dir=tmpdir()) as td:
        tfs = [tarfile.open(open_layer(d, td)) for d in m["layers"]]
        tree = merge(tfs)
        for dst in extra:
            tree.pop(dst.lstrip("/"), None)
        paths = set(tree) | {e.lstrip("/") for e in extra}
        dirs = {os.path.dirname(p) for p in paths}
        while dirs:
            d = dirs.pop()
            if d and d not in paths:
                paths.add(d)
                dirs.add(os.path.dirname(d))
        # Hard links stay links: one inode per (layer, target), data on the first entry written (kernel's maybe_link).
        groups = {(id(tf), safe_name(ti.linkname)) for tf, ti in tree.values() if ti.islnk()}
        inos, filled = {}, set()
        with open(out, "wb") as f:
            c = Cpio(f)
            for p in sorted(paths, key=lambda p: p.split("/")):
                if "/" + p in extra:
                    src, mode = extra["/" + p]
                    c.entry(p, stat.S_IFREG | mode, size=os.path.getsize(src), data=open(src, "rb"))
                    continue
                if p not in tree:
                    c.entry(p, stat.S_IFDIR | 0o755, nlink=2)
                    continue
                tf, ti = tree[p]
                own = dict(uid=ti.uid, gid=ti.gid, mtime=ti.mtime)
                perm = ti.mode & 0o7777
                if ti.isdir():
                    c.entry(p, stat.S_IFDIR | perm, nlink=2, **own)
                elif ti.issym():
                    t = ti.linkname.encode()
                    c.entry(p, stat.S_IFLNK | 0o777, size=len(t), data=t, **own)
                elif ti.isreg() or ti.islnk():
                    key = (id(tf), safe_name(ti.linkname if ti.islnk() else ti.name))
                    if key not in groups:
                        c.entry(p, stat.S_IFREG | perm, size=ti.size, data=tf.extractfile(ti), **own)
                        continue
                    ino = inos.setdefault(key, c.new_ino())
                    if key in filled:
                        c.entry(p, stat.S_IFREG | perm, nlink=2, ino=ino, **own)
                    else:
                        filled.add(key)
                        src = tf.extractfile(ti)
                        data = src.read()
                        c.entry(p, stat.S_IFREG | perm, size=len(data), data=data, nlink=2, ino=ino, **own)
                elif ti.ischr() or ti.isblk():
                    kind = stat.S_IFCHR if ti.ischr() else stat.S_IFBLK
                    c.entry(p, kind | perm, rdev=(ti.devmajor, ti.devminor), **own)
                elif ti.isfifo():
                    c.entry(p, stat.S_IFIFO | perm, **own)
            c.trailer()
        for tf in tfs:
            tf.close()
    return tree


def main():
    a = sys.argv[1:]
    if len(a) >= 2 and a[0] == "resolve":
        print(resolve(a[1]))
    elif len(a) >= 2 and a[0] == "pull":
        d, m, _ = pull(a[1])
        print(d, sum(x["size"] for x in m["layers"]), "bytes in", len(m["layers"]), "layers")
    elif len(a) >= 2 and a[0] == "config":
        print(json.dumps(pull(a[1])[2].get("config", {}), indent=1))
    elif len(a) >= 3 and a[0] == "rootfs":
        extra = {}
        for e in a[3:]:
            dst, src = e.split("=", 1)
            extra[dst] = (src, 0o755)
        rootfs(a[1], a[2], extra)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
