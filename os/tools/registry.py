#!/usr/bin/env python3
"""A read-only OCI distribution API server over an OCI image layout, for tests (Python standard library only).

  registry.py serve LAYOUT [--listen 127.0.0.1:5000] [--auth none|anonymous|USER:PASS] [--token-ttl SECONDS]
  registry.py --self-test            builds a layout in a temporary directory and checks the server against it
  registry.py --check-layout LAYOUT  serves LAYOUT and pulls every image through the API, verifying every digest

Plain HTTP. Endpoints: GET /v2/, GET|HEAD /v2/NAME/manifests/(TAG|DIGEST), GET|HEAD /v2/NAME/blobs/DIGEST (single
byte ranges), GET /v2/NAME/tags/list and /v2/_catalog (n/last pagination), GET /token. Repositories and tags come
from the layout's index.json annotations (`io.containerd.image.name`, else `org.opencontainers.image.ref.name` under
the repository `library/test`); a repository serves only the blobs its images reference. Auth emulates the bearer
flow of public registries: `anonymous` hands out tokens to anyone, `USER:PASS` wants HTTP Basic on /token; tokens are
HMAC-signed, scoped to repository:NAME:pull and expire. A client that does not accept image indexes gets the
linux/amd64 manifest, as distribution does.
"""
import base64
import hashlib
import hmac
import http.client
import http.server
import json
import os
import re
import secrets
import sys
import tempfile
import threading
import time
import urllib.parse

INDEX_TYPES = ("application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json")
MANIFEST_TYPES = ("application/vnd.oci.image.manifest.v1+json",
                  "application/vnd.docker.distribution.manifest.v2+json")
NAME = re.compile(r"^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*$")
TAG = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$")
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
SERVICE = "k10s-test-registry"
MAX_MANIFEST = 4 << 20


class LayoutError(Exception):
    pass


class Layout:
    """Repositories, tags, manifests and per-repository blob sets of an OCI layout, every blob verified."""

    def __init__(self, root):
        self.root = root
        self.tags, self.manifests, self.blobs, self.types = {}, {}, {}, {}
        if json.load(open(os.path.join(root, "oci-layout"))).get("imageLayoutVersion") != "1.0.0":
            raise LayoutError("not an OCI layout 1.0.0")
        index = json.load(open(os.path.join(root, "index.json")))
        for d in index.get("manifests", []):
            ann = d.get("annotations", {})
            full = ann.get("io.containerd.image.name")
            if full:
                repo, _, tag = full.rpartition(":")
                repo = repo.split("/", 1)[1] if "." in repo.split("/")[0] and "/" in repo else repo
            else:
                repo, tag = "library/test", ann.get("org.opencontainers.image.ref.name", "latest")
            if not NAME.match(repo) or not TAG.match(tag):
                raise LayoutError(f"bad reference {repo}:{tag}")
            self.tags.setdefault(repo, {})[tag] = d["digest"]
            self.walk(repo, d)

    def path(self, digest):
        return os.path.join(self.root, "blobs", "sha256", digest[7:])

    def walk(self, repo, desc):
        digest = desc["digest"]
        if not DIGEST.match(digest):
            raise LayoutError(f"bad digest {digest}")
        p = self.path(digest)
        if os.path.getsize(p) != desc["size"] or sha(open(p, "rb").read()) != digest:
            raise LayoutError(f"blob {digest} does not match its descriptor")
        mt = desc.get("mediaType", "")
        if mt in INDEX_TYPES or mt in MANIFEST_TYPES:
            self.manifests.setdefault(repo, set()).add(digest)
            self.types[digest] = mt
            body = json.load(open(p))
            for child in body.get("manifests", []):
                self.walk(repo, child)
            for child in [body["config"]] + body.get("layers", []) if "config" in body else []:
                self.walk(repo, child)
        else:
            self.blobs.setdefault(repo, set()).add(digest)


def sha(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def unb64(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class Tokens:
    def __init__(self, ttl):
        self.key, self.ttl = secrets.token_bytes(32), ttl

    def issue(self, sub, access, now=None):
        now = int(now if now is not None else time.time())
        claims = json.dumps({"sub": sub, "access": access, "iat": now, "exp": now + self.ttl},
                            sort_keys=True).encode()
        return b64(claims) + "." + b64(hmac.new(self.key, claims, "sha256").digest())

    def check(self, token, repo):
        """'' when the token grants pull on repo, else the bearer error name."""
        try:
            body, mac = token.split(".")
            claims = unb64(body)
            if not hmac.compare_digest(unb64(mac), hmac.new(self.key, claims, "sha256").digest()):
                return "invalid_token"
            c = json.loads(claims)
        except (ValueError, TypeError):
            return "invalid_token"
        if c.get("exp", 0) < time.time():
            return "invalid_token"
        if repo is None:
            return ""
        ok = any(a.get("type") == "repository" and a.get("name") == repo and "pull" in a.get("actions", [])
                 for a in c.get("access", []))
        return "" if ok else "insufficient_scope"


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = "k10s-test-registry"
    protocol_version = "HTTP/1.1"
    timeout = 30

    def log_message(self, fmt, *args):
        if self.server.verbose:
            sys.stderr.write("registry: " + fmt % args + "\n")

    def send(self, code, body=b"", ctype="application/json", headers=(), head=False):
        self.send_response(code)
        self.send_header("Docker-Distribution-API-Version", "registry/2.0")
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def error(self, code, err, msg, headers=(), head=False):
        body = json.dumps({"errors": [{"code": err, "message": msg}]}).encode()
        self.send(code, body, headers=headers, head=head)

    def challenge(self, repo, err="", head=False):
        realm = f"http://{self.headers.get('Host', '127.0.0.1')}/token"
        c = f'Bearer realm="{realm}",service="{SERVICE}"'
        if repo:
            c += f',scope="repository:{repo}:pull"'
        if err:
            c += f',error="{err}"'
        self.error(401, "UNAUTHORIZED", "authentication required", [("WWW-Authenticate", c)], head)

    def authorized(self, repo, head):
        if self.server.auth == "none":
            return True
        h = self.headers.get("Authorization", "")
        if not h.startswith("Bearer "):
            self.challenge(repo, head=head)
            return False
        err = self.server.tokens.check(h[7:].strip(), repo)
        if err:
            self.challenge(repo, err, head)
            return False
        return True

    def do_GET(self):
        self.route(head=False)

    def do_HEAD(self):
        self.route(head=True)

    def refuse(self):
        self.error(405, "UNSUPPORTED", "this registry is read-only")

    do_POST = do_PUT = do_PATCH = do_DELETE = refuse

    def route(self, head):
        url = urllib.parse.urlsplit(self.path)
        q = urllib.parse.parse_qs(url.query)
        p = url.path
        if p == "/token" and not head:
            return self.token(q)
        if p in ("/v2", "/v2/"):
            if self.authorized(None, head):
                self.send(200, b"{}", head=head)
            return
        if p == "/v2/_catalog":
            if self.authorized(None, head):
                self.listing("repositories", sorted(self.server.layout.tags), q, "/v2/_catalog", head)
            return
        m = re.match(r"^/v2/(.+)/(manifests|blobs|tags)/([^/]+)$", p)
        if not m:
            return self.error(404, "NOT_FOUND", "no such endpoint", head=head)
        repo, kind, ref = m.groups()
        if not NAME.match(repo) or len(repo) > 255:
            return self.error(400, "NAME_INVALID", "invalid repository name", head=head)
        if not self.authorized(repo, head):
            return
        lay = self.server.layout
        if repo not in lay.tags:
            return self.error(404, "NAME_UNKNOWN", "repository not known", head=head)
        if kind == "tags":
            if ref != "list":
                return self.error(404, "NOT_FOUND", "no such endpoint", head=head)
            return self.listing("tags", sorted(lay.tags[repo]), q, f"/v2/{repo}/tags/list", head, {"name": repo})
        if kind == "manifests":
            return self.manifest(repo, ref, head)
        return self.blob(repo, ref, head)

    def listing(self, key, items, q, path, head, extra=None):
        try:
            n = int(q.get("n", ["1000"])[0])
        except ValueError:
            return self.error(400, "PAGINATION_NUMBER_INVALID", "n is not a number", head=head)
        last = q.get("last", [""])[0]
        items = [i for i in items if i > last]
        page, hdr = items[:max(n, 0)], []
        if len(items) > len(page) and page:
            hdr.append(("Link", f'<{path}?n={n}&last={urllib.parse.quote(page[-1], safe="")}>; rel="next"'))
        self.send(200, json.dumps(dict(extra or {}, **{key: page})).encode(), headers=hdr, head=head)

    def manifest(self, repo, ref, head):
        lay = self.server.layout
        if DIGEST.match(ref):
            digest = ref if ref in lay.manifests.get(repo, ()) else None
        elif TAG.match(ref):
            digest = lay.tags[repo].get(ref)
        else:
            return self.error(400, "MANIFEST_INVALID", "invalid reference", head=head)
        if digest is None:
            return self.error(404, "MANIFEST_UNKNOWN", "manifest unknown", head=head)
        accept = ",".join(self.headers.get_all("Accept") or [])
        if lay.types[digest] in INDEX_TYPES and accept and "*/*" not in accept and \
                not any(t in accept for t in INDEX_TYPES):
            index = json.load(open(lay.path(digest)))
            amd64 = [d for d in index["manifests"] if d.get("platform", {}).get("architecture") == "amd64"
                     and d.get("platform", {}).get("os") == "linux"]
            if not amd64:
                return self.error(404, "MANIFEST_UNKNOWN", "no linux/amd64 manifest for this client", head=head)
            digest = amd64[0]["digest"]
        body = open(lay.path(digest), "rb").read()
        self.send(200, body, lay.types[digest], [("Docker-Content-Digest", digest), ("ETag", f'"{digest}"')], head)

    def blob(self, repo, digest, head):
        lay = self.server.layout
        if not DIGEST.match(digest):
            return self.error(400, "DIGEST_INVALID", "invalid digest", head=head)
        if digest not in lay.blobs.get(repo, ()) and digest not in lay.manifests.get(repo, ()):
            return self.error(404, "BLOB_UNKNOWN", "blob unknown to registry", head=head)
        size = os.path.getsize(lay.path(digest))
        start, end, code, hdr = 0, size - 1, 200, [("Docker-Content-Digest", digest), ("Accept-Ranges", "bytes")]
        rng = self.headers.get("Range")
        if rng:
            m = re.match(r"^bytes=(\d*)-(\d*)$", rng.strip())
            if not m or m.group(1) == m.group(2) == "":
                return self.send(416, b"", headers=[("Content-Range", f"bytes */{size}")], head=head)
            if m.group(1):
                start, end = int(m.group(1)), int(m.group(2)) if m.group(2) else size - 1
            else:
                start, end = max(size - int(m.group(2)), 0), size - 1
            end = min(end, size - 1)
            if start > end:
                return self.send(416, b"", headers=[("Content-Range", f"bytes */{size}")], head=head)
            code = 206
            hdr.append(("Content-Range", f"bytes {start}-{end}/{size}"))
        with open(lay.path(digest), "rb") as f:
            f.seek(start)
            body = f.read(end - start + 1) if size else b""
        self.send(code, body, "application/octet-stream", hdr, head)

    def token(self, q):
        if self.server.auth not in ("none", "anonymous"):
            h = self.headers.get("Authorization", "")
            user = None
            if h.startswith("Basic "):
                try:
                    user, _, pw = base64.b64decode(h[6:].strip(), validate=True).decode().partition(":")
                except (ValueError, UnicodeDecodeError):
                    user = None
            want_user, _, want_pw = self.server.auth.partition(":")
            if user is None or not (hmac.compare_digest(user, want_user) and hmac.compare_digest(pw, want_pw)):
                return self.error(401, "UNAUTHORIZED", "bad credentials",
                                  [("WWW-Authenticate", f'Basic realm="{SERVICE}"')])
        if q.get("service", [SERVICE])[0] != SERVICE:
            return self.error(400, "DENIED", "unknown service")
        access = []
        for scope in q.get("scope", [])[:16]:
            parts = scope.split(":")
            if len(parts) == 3 and parts[0] == "repository" and parts[1] in self.server.layout.tags:
                acts = [a for a in parts[2].split(",") if a == "pull"]
                access.append({"type": "repository", "name": parts[1], "actions": acts})
        tok = self.server.tokens.issue(q.get("account", ["anonymous"])[0][:64], access)
        issued = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        body = {"token": tok, "access_token": tok, "expires_in": self.server.tokens.ttl, "issued_at": issued}
        self.send(200, json.dumps(body).encode())


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, addr, layout, auth="anonymous", ttl=300, verbose=False):
        super().__init__(addr, Handler)
        self.layout, self.auth, self.tokens, self.verbose = layout, auth, Tokens(ttl), verbose


def start(layout, auth, ttl=300):
    srv = Server(("127.0.0.1", 0), layout, auth, ttl)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


# ---- client side (self-test and --check-layout) ----

class Client:
    def __init__(self, port, basic=None):
        self.port, self.basic, self.token = port, basic, None

    def raw(self, method, path, headers=None):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
        c.request(method, path, headers=headers or {})
        r = c.getresponse()
        body = r.read()
        c.close()
        return r.status, {k.lower(): v for k, v in r.getheaders()}, body

    def req(self, method, path, headers=None, repo=None):
        """A request with the bearer flow: on 401, get a token from the challenge's realm and retry once."""
        h = dict(headers or {})
        if self.token:
            h["Authorization"] = "Bearer " + self.token
        st, hdr, body = self.raw(method, path, h)
        if st == 401 and "www-authenticate" in hdr:
            ch = dict(re.findall(r'(\w+)="([^"]*)"', hdr["www-authenticate"]))
            realm = urllib.parse.urlsplit(ch["realm"])
            q = {"service": ch["service"]}
            if "scope" in ch:
                q["scope"] = ch["scope"]
            th = {"Authorization": "Basic " + base64.b64encode(self.basic.encode()).decode()} if self.basic else {}
            tst, _, tbody = self.raw("GET", realm.path + "?" + urllib.parse.urlencode(q), th)
            if tst != 200:
                return st, hdr, body
            self.token = json.loads(tbody)["token"]
            h["Authorization"] = "Bearer " + self.token
            st, hdr, body = self.raw(method, path, h)
        return st, hdr, body

    def pull(self, repo, tag):
        """Every manifest and blob of repo:tag, each checked against its digest; returns (manifests, blobs)."""
        accept = {"Accept": ", ".join(INDEX_TYPES + MANIFEST_TYPES)}
        st, hdr, body = self.req("GET", f"/v2/{repo}/manifests/{tag}", accept)
        if st != 200 or sha(body) != hdr.get("docker-content-digest"):
            raise AssertionError(f"{repo}:{tag}: manifest HTTP {st}")
        todo, manifests, blobs = [json.loads(body)], 1, 0
        while todo:
            m = todo.pop()
            for d in m.get("manifests", []):
                st, hdr, body = self.req("GET", f"/v2/{repo}/manifests/{d['digest']}", accept)
                if st != 200 or sha(body) != d["digest"] or len(body) != d["size"]:
                    raise AssertionError(f"{repo}: child manifest {d['digest']}")
                todo.append(json.loads(body))
                manifests += 1
            for d in ([m["config"]] if "config" in m else []) + m.get("layers", []):
                st, _, body = self.req("GET", f"/v2/{repo}/blobs/{d['digest']}")
                if st != 200 or sha(body) != d["digest"] or len(body) != d["size"]:
                    raise AssertionError(f"{repo}: blob {d['digest']} HTTP {st}")
                blobs += 1
        return manifests, blobs


def check_layout(root):
    layout = Layout(root)
    srv = start(layout, "anonymous")
    images = manifests = blobs = 0
    try:
        for repo, tags in sorted(layout.tags.items()):
            for tag in sorted(tags):
                m, b = Client(srv.server_address[1]).pull(repo, tag)
                images, manifests, blobs = images + 1, manifests + m, blobs + b
                print(f"registry check: {repo}:{tag}: {m} manifest(s), {b} blob(s)")
    finally:
        srv.shutdown()
    print(f"registry check: {images} image(s), {manifests} manifest(s), {blobs} blob(s) pulled and verified")


def write_test_layout(root):
    """Two repositories: test/multi (an index for amd64 and arm64) and test/single (two layers)."""
    os.makedirs(os.path.join(root, "blobs", "sha256"))

    def put(data, mt, **extra):
        d = sha(data)
        open(os.path.join(root, "blobs", "sha256", d[7:]), "wb").write(data)
        return dict(mediaType=mt, digest=d, size=len(data), **extra)

    def image(arch, layers):
        ls = [put(x, "application/vnd.oci.image.layer.v1.tar") for x in layers]
        cfg = put(json.dumps({"architecture": arch, "os": "linux", "rootfs": {"type": "layers", "diff_ids": [
            x["digest"] for x in ls]}}).encode(), "application/vnd.oci.image.config.v1+json")
        body = json.dumps({"schemaVersion": 2, "mediaType": MANIFEST_TYPES[0], "config": cfg, "layers": ls})
        return put(body.encode(), MANIFEST_TYPES[0], platform={"architecture": arch, "os": "linux"})

    amd, arm = image("amd64", [b"amd64 layer" * 100]), image("arm64", [b"arm64 layer" * 100])
    idx = put(json.dumps({"schemaVersion": 2, "mediaType": INDEX_TYPES[0], "manifests": [amd, arm]}).encode(),
              INDEX_TYPES[0], annotations={"io.containerd.image.name": "test/multi:v1"})
    single = dict(image("amd64", [b"first" * 1000, b"second" * 1000]))
    single.pop("platform")
    single["annotations"] = {"io.containerd.image.name": "test/single:v2", "org.opencontainers.image.ref.name": "v2"}
    open(os.path.join(root, "oci-layout"), "w").write('{"imageLayoutVersion": "1.0.0"}')
    open(os.path.join(root, "index.json"), "w").write(json.dumps({"schemaVersion": 2, "manifests": [idx, single]}))
    return idx, amd, arm, single


def self_test():
    checks = 0

    def ok(cond, what):
        nonlocal checks
        if not cond:
            raise AssertionError(what)
        checks += 1

    with tempfile.TemporaryDirectory() as td:
        idx, amd, arm, single = write_test_layout(td)
        layout = Layout(td)
        srv = start(layout, "k10s:secret", ttl=300)
        port = srv.server_address[1]
        anon = Client(port)
        try:
            st, hdr, _ = anon.raw("GET", "/v2/")
            ok(st == 401 and hdr["www-authenticate"].startswith("Bearer realm=") and SERVICE in hdr[
                "www-authenticate"], "/v2/ challenges")
            ok(hdr.get("docker-distribution-api-version") == "registry/2.0", "API version header")
            st, _, _ = anon.raw("GET", f"/token?service={SERVICE}&scope=repository:test/multi:pull")
            ok(st == 401, "token without credentials")
            bad = Client(port, "k10s:wrong")
            ok(bad.req("GET", "/v2/test/multi/tags/list")[0] == 401, "token with a wrong password")
            c = Client(port, "k10s:secret")
            st, hdr, body = c.req("GET", "/v2/test/multi/tags/list")
            ok(st == 200 and json.loads(body) == {"name": "test/multi", "tags": ["v1"]}, "bearer flow and tags")
            acc = {"Accept": ", ".join(INDEX_TYPES + MANIFEST_TYPES)}
            st, hdr, body = c.req("HEAD", "/v2/test/multi/manifests/v1", acc)
            ok(st == 200 and body == b"" and hdr["content-type"] == INDEX_TYPES[0] and
               hdr["docker-content-digest"] == idx["digest"] and int(hdr["content-length"]) == idx["size"],
               "HEAD index by tag")
            st, hdr, body = c.req("GET", "/v2/test/multi/manifests/v1", acc)
            ok(st == 200 and sha(body) == idx["digest"], "GET index by tag")
            st, hdr, body = c.req("GET", f"/v2/test/multi/manifests/{idx['digest']}", acc)
            ok(st == 200 and sha(body) == idx["digest"], "GET index by digest")
            st, hdr, body = c.req("GET", f"/v2/test/multi/manifests/{arm['digest']}", acc)
            ok(st == 200 and sha(body) == arm["digest"] and hdr["content-type"] == MANIFEST_TYPES[0], "child manifest")
            st, hdr, body = c.req("GET", "/v2/test/multi/manifests/v1", {"Accept": MANIFEST_TYPES[1]})
            ok(st == 200 and hdr["docker-content-digest"] == amd["digest"], "index-less client gets linux/amd64")
            ok(c.pull("test/multi", "v1") == (3, 4), "pull test/multi:v1")
            m = json.loads(open(layout.path(arm["digest"]), "rb").read())
            layer = m["layers"][0]
            st, hdr, body = c.req("GET", f"/v2/test/multi/blobs/{layer['digest']}", {"Range": "bytes=5-15"})
            full = open(layout.path(layer["digest"]), "rb").read()
            ok(st == 206 and body == full[5:16] and hdr["content-range"] == f"bytes 5-15/{len(full)}", "range")
            st, _, body = c.req("GET", f"/v2/test/multi/blobs/{layer['digest']}", {"Range": "bytes=-4"})
            ok(st == 206 and body == full[-4:], "suffix range")
            ok(c.req("GET", f"/v2/test/multi/blobs/{layer['digest']}", {"Range": "bytes=99999-"})[0] == 416,
               "unsatisfiable range")
            st, hdr, body = c.req("HEAD", f"/v2/test/multi/blobs/{layer['digest']}")
            ok(st == 200 and body == b"" and int(hdr["content-length"]) == len(full), "HEAD blob")
            st, _, body = c.req("GET", "/v2/test/multi/manifests/v9")
            ok(st == 404 and json.loads(body)["errors"][0]["code"] == "MANIFEST_UNKNOWN", "unknown tag")
            ok(c.req("GET", "/v2/test/multi/blobs/sha256:" + "0" * 64)[0] == 404, "unknown blob")
            ok(c.req("GET", "/v2/test/multi/blobs/sha256:xyz")[0] == 400, "malformed digest")
            ok(c.req("GET", "/v2/Bad..Name/tags/list")[0] == 400, "invalid repository name")
            ok(c.req("GET", "/v2/test/multi/blobs/../../../etc/passwd")[0] in (400, 404), "path traversal")
            sm = json.loads(open(layout.path(single["digest"]), "rb").read())
            other = sm["layers"][0]["digest"]
            ok(c.req("GET", f"/v2/test/multi/blobs/{other}")[0] == 404, "blobs are scoped to their repository")
            st, hdr, _ = c.raw("GET", "/v2/test/single/tags/list", {"Authorization": "Bearer " + c.token})
            ok(st == 401 and 'error="insufficient_scope"' in hdr["www-authenticate"], "token scoped to its repository")
            body, mac = c.token.split(".")
            forged = ("B" if body[0] != "B" else "C") + body[1:] + "." + mac
            ok(c.raw("GET", "/v2/test/multi/tags/list", {"Authorization": "Bearer " + forged})[0] == 401,
               "forged token")
            expired = srv.tokens.issue("k10s", [{"type": "repository", "name": "test/multi", "actions": ["pull"]}],
                                       now=time.time() - 3600)
            ok(c.raw("GET", "/v2/test/multi/tags/list", {"Authorization": "Bearer " + expired})[0] == 401,
               "expired token")
            c2 = Client(port, "k10s:secret")
            ok(c2.pull("test/single", "v2") == (1, 3), "pull test/single:v2")
            st, hdr, body = c2.req("GET", "/v2/_catalog?n=1")
            ok(st == 200 and json.loads(body) == {"repositories": ["test/multi"]} and 'rel="next"' in hdr["link"],
               "catalog pagination")
            st, _, body = c2.req("GET", "/v2/_catalog?n=1&last=test/multi")
            ok(json.loads(body) == {"repositories": ["test/single"]}, "catalog second page")
            ok(c2.req("DELETE", f"/v2/test/single/manifests/{single['digest']}")[0] == 405, "read-only")
            ok(c2.req("PUT", "/v2/test/single/manifests/v3")[0] == 405, "no push")
        finally:
            srv.shutdown()
        srv = start(layout, "none")
        try:
            ok(Client(srv.server_address[1]).raw("GET", "/v2/")[0] == 200, "auth none")
            ok(Client(srv.server_address[1]).pull("test/multi", "v1") == (3, 4), "pull without auth")
        finally:
            srv.shutdown()
        srv = start(layout, "anonymous")
        try:
            ok(Client(srv.server_address[1]).pull("test/single", "v2") == (1, 3), "anonymous token")
        finally:
            srv.shutdown()
    print(f"registry self-test: {checks} checks passed")


def main():
    a = sys.argv[1:]
    if a == ["--self-test"]:
        self_test()
    elif len(a) == 2 and a[0] == "--check-layout":
        check_layout(a[1])
    elif len(a) >= 2 and a[0] == "serve":
        opts = dict(zip(a[2::2], a[3::2]))
        host, _, port = opts.get("--listen", "127.0.0.1:5000").rpartition(":")
        srv = Server((host, int(port)), Layout(a[1]), opts.get("--auth", "anonymous"),
                     int(opts.get("--token-ttl", "300")), verbose=True)
        print(f"registry: serving {a[1]} on http://{host}:{srv.server_address[1]}/ (auth {srv.auth.split(':')[0]})",
              flush=True)
        srv.serve_forever()
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
