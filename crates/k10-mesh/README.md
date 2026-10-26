# k10-mesh

The k10s cluster layer, with no control plane (see `docs/DESIGN.md` §2.6–2.8). The desired state is a small
manifest (native services, image services, the OS plan) plus content-addressed code blobs, spread by gossip. Membership and failure detection use SWIM-style
gossip. Placement is a pure function that every node computes on its own.

The core is **sans-IO** and `no_std + alloc` (`--no-default-features`). It has no sockets, threads or clocks:
time is passed in as `now_ms`, and packets and events come back as return values. The `std` feature (default)
adds a UDP driver, a thread runner and a deterministic simulator. The crate has no dependencies.

| module | what |
|---|---|
| `gossip` | `Gossip` state machine: SWIM probes, suspicion, refutation, dissemination, push-pull sync, desired-state pointer |
| `manifest` | `Manifest`/`ServiceSpec`/`OsPlan`, canonical binary codec, `hash()`, operator text format parser (`parse_document`) |
| `image` | `ImageService` (an OCI image converted to `K10I`, run under the native kernel's Linux personality) and its fields |
| `placement` | `place(&Manifest, &[Member]) -> Placement` (capacity-aware weighted rendezvous hashing over capable nodes), `check_members`, `image_note` |
| `member` | `NodeId`, `Addr` (= `core::net::SocketAddr`), `Capacity`, `Member`, `MemberState`, `OsInfo` (extension record) |
| `caps` | `Arch` (codes shared by manifests, `K10I` and gossip), `NodeCaps` (the `EXT_CAPS` extension record) |
| `osimage` | signed OS images (Tier 1 updates): `sign`, `inspect`, `verify` |
| `rollout` | `rollout::step(&OsPlan, &[Member], me)`: the OS update waves, evaluated by every node |
| `wire` | TCP peer protocol framing: `Msg` (borrowed, copy-free), `FrameDecoder` (incremental) |
| `sha256` | SHA-256, `to_hex`/`from_hex` |
| `crypto` | ChaCha20 (AVX2 or SSE2 on x86-64), Poly1305, (X)ChaCha20-Poly1305, HMAC/HKDF-SHA256, SHA-512, Ed25519, `ct_eq` (RFC vectors) |
| `secure` | cluster keys (current + next), `GossipCipher` (sealed datagrams, anti-replay), `Link` (peer-link handshake, sealed records; hosted and metal), sealed image-service secrets |
| `admin_auth` | signed admin requests: `AdminKey` (client), `AdminVerifier` (clock window, nonce cache), `check_body` |
| `driver` (std) | `UdpDriver` (caller-owned epoll loop) and `MeshThread` (self-contained thread) |
| `sim` (std) | discrete-event simulator: virtual time, loss, latency, partitions |

## Integrating (node runtime)

```rust
let mut g = Gossip::new(local_member, Config::default(), seed);   // or UdpDriver::bind(local, cfg, seed)
let out = g.join(&seed_addrs, now);           // send out.packets, handle out.events
let out = g.on_packet(src_addr, &datagram, now);
let out = g.tick(now);                         // when now >= g.next_deadline()
let out = g.set_desired(version, manifest_hash); // after `k10s apply` stored the manifest locally
let out = g.leave(now);
let placement = place(&manifest, &g.live_members());
```

Events are `MemberUp`, `MemberDown`, `MemberUpdated` and `DesiredState { version, hash, origin }`.
- On a membership change, recompute `place()` and start or stop local replicas (`placement.on_node(me)`).
- On `DesiredState`, fetch the manifest with `GetManifest` and any missing blobs with `GetBlob` (wire protocol)
  from `origin` or any live member. Check `sha256(bytes) == hash`, then apply.

`UdpDriver` gives the same calls over a non-blocking socket: `raw_fd()`, `on_readable(now)`, `on_tick(now)`,
`next_deadline()`, `poll_event()`.

## Gossip protocol (UDP, every datagram ≤ `max_packet` = 1400 bytes)

With a cluster key every datagram below travels sealed (`secure::GossipCipher`, `UdpDriver::set_cipher`):
`'K' | 1 | nonce[24] = boot ctr ts_ms | XChaCha20-Poly1305(datagram) | tag[16]`; forged, stale and replayed ones are
dropped before decoding (docs/NODE.md "Cluster authentication").

```
datagram = magic:u8=0x6b version:u8=1 type:u8 sender:u64 | body | n:u8 item*n
Ping    (1) seq:u32 target:u64 relay:(0 | 1 addr)   target replies Ack to the datagram source
Ack     (2) seq:u32 target:u64 relay:(0 | 1 addr)   relay set → the intermediary forwards it to `relay`
PingReq (3) seq:u32 target:u64 target_addr:addr     "ping target for me" (indirect probe)
Sync    (4) flags:u8(1 want_reply, 2 has_desired) [desired] n:u8 member*n   push-pull state chunk
Gossip  (5) —                                        piggyback only
item    = 1 Alive member | 2 Suspect id inc from | 3 Dead id inc from | 4 Left id inc
        | 5 Desired version:u64 hash:[32] origin:u64          (ids/inc: u64 LE)
member  = id:u64 inc:varint state:u8 gossip:addr rpc:raddr http:raddr mem_bytes:varint cores:varint [ext]
addr    = 4 ip[4] port:u16 | 6 ip[16] port:u16;  raddr = 0 port:u16 (same IP as gossip) | addr
ext     = len:u8 (type:u8 len:u8 value)*        present when bit 30 of cores is set; unknown types skipped
          type 1 OS: platform_len:u8 platform image:[8] failed:[8] flags:u8 (1 committed, 2 updatable)
```

- **Probing.** Every `probe_interval` (200 ms), ping the next member of a shuffled round-robin list. With no
  ack after `probe_timeout` (80 ms), re-ping and send ping-req to 3 random members. Their pings carry a relay
  address, so acks come back through them with no state kept on the intermediary. With no ack by the end of the
  period, the target becomes **Suspect**.
- **Suspicion (Lifeguard).** The timeout is `max = 4·min` with no confirmation and shrinks to `min` after 3
  independent confirmations, where `min = 4 · probe_interval · digits(n)`. When it expires the member is
  **Dead**. A member that hears it is suspected or dead (including a node we consider dead pinging us, and
  pings to a suspect) refutes by bumping its incarnation.
- **Dissemination.** Each update is piggybacked on every datagram, least-sent first, with items about the
  destination first. While any update is queued, it is also pushed to 3 random members every 200 ms. Each
  update is sent `4·(digits(n)+1)` times. One item per member is kept, so the queue is bounded by the number
  of members.
- **Push-pull sync.** Full state goes to one random member every 5 s, scaled by `log2(n)−4` above 32 members.
  The state is split into self-contained chunks, and the first chunk asks for a reply. A joiner also syncs at
  about +1, +2, +4 … s until it reaches the normal interval. A datagram from a sender we have never heard of
  triggers a sync with it (at most one per second). Remote "dead" records only make us *suspect*, which leaves
  room for refutation. Reconnects heal partitions and restarts: one sync per `join_retry_ms`, round-robin over the
  addresses of dead members, tombstones (which keep the last address of failed members, also learned from syncs),
  seeds that are not live and, while isolated, the fallback seeds; each address backs off 1 s → 60 s.
- **Placement version and extensions.** The top byte of a member's `cores` varint carries the placement algorithm
  version it implements in its low 6 bits (0 from older nodes = 1; `place` uses the lowest live version), bit 7
  "decodes extensions" and bit 6 "extension records follow". Nodes that predate extensions read bit 7 as a high
  placement version (they keep their own, lower one) and would reject the records, so a node sends records with
  extensions only to members whose bit 7 is set. Capable members always get the extension block (empty: no OS
  report); a copy without it (sent before the receiver was known, or relayed by an older node) leaves the report
  unknown, never erases one of the same incarnation, and makes `rollout` wait. The OS record (`EXT_OS`) is a Tier 1
  node's report for the update waves (`rollout`, docs/OS.md). The capability record (`EXT_CAPS`, every node of
  placement version 3) carries the architecture, word size, largest native slot (`max_mem_log2`), JIT flag,
  Linux-personality level (0: no image services) and the reason for a 0 (hosted node, kernel without the
  personality, CPU gate, architecture), image-cache and object-area bytes and the tenant the node is dedicated to;
  nodes that predate it skip it as an unknown record, and a copy of the same incarnation that carries it adds it to
  one relayed without it (capabilities are fixed for an incarnation).
- **Leave.** `Left` is sent to 16 random members, which re-gossip it.
- **Desired-state pointer.** `(version, hash, origin)`: the highest version wins, and ties go to the larger
  hash.
- **Bounded memory.** Dead and left records are kept `dead_retain_ms` (5 min), so that syncs spread them.
  They are then compacted to `(id, incarnation)` tombstones for `tombstone_ttl_ms` (1 h, at most 4096), so stale
  gossip cannot bring them back. Members are capped at `max_members`, and hints at 256. Malformed datagrams
  are counted and dropped; they never panic.

## Peer protocol framing (TCP, `wire`)

`frame = len:u32 LE | kind:u8 | request_id:u64 LE | payload`, with `len = 9 + payload`.
Messages: Hello{member}, GetManifest{version}/Manifest{bytes}, GetBlob{hash}/Blob{hash,bytes}/BlobMissing{hash},
Call{service,path,body,deadline_ms,method}/CallResult{status,body}, Ping/Pong. With a cluster key
(`secure::Link`) each side first sends `Auth` (kind 11: version, id, fresh nonce, held key ids); the frame stream
then travels in records, `len | ChaCha20-Poly1305(stream bytes) | tag[16]` (one per write, ≤ 64 KiB of plaintext, a
frame may span records), under a per-direction session key; the first record holds only `Hello` (≤ 256 bytes) and
nothing else is sent before the peer's has opened. Payload layouts are documented in
`src/wire.rs`. `FrameDecoder` takes partial input (`feed`, or `spare`/`commit` to read in place) and caps the
frame size (default: the largest bundle blob plus headers). Errors are sticky, and the decoder never allocates from a claimed length. `encode_header` allows
vectored writes of large blobs.

## Manifest

The binary format is canonical: services are sorted by name and varints minimal, so `hash = sha256(encode())`.
The decoder is strict and bounded. Format 2 adds each service's `calls` grant: an "any" flag bit, or strictly
increasing indices into the sorted service list (existing and canonical by construction, 4 bytes each in memory).
A manifest without any grant is encoded as format 1 (the original bytes, so older manifests keep their hash and
decode with nothing granted). Format 3 adds the OS plan after the services: one image hash per platform (sorted) and
the first wave's size. Format 4 adds image services: native and image services share one name order, each entry
`name | kind | body` (kind 0: the native fields; kind 1: an image service), `calls` index that common order, and the
OS plan may be empty. The encoder writes the oldest format that expresses a manifest (`Manifest::format`), so a
manifest without image services keeps its format and hash. An image service's lists are canonical in memory
(`ImageService::canonicalize` sorts them, `validate` refuses any other order or a duplicate), and route and public
port claims are unique across the manifest. The operator text format (`k10s apply`) looks like this:

```
service echo {
  code = "echo.ten"   # path; the CLI resolves/compiles it
  replicas = 3        # default 1
  memory = 64KiB      # power of two, 4KiB..64GiB (B, KiB, MiB, GiB); default 1MiB
  max_instances = 64  # per node; default 64
  cpu_ms = 50         # per-request CPU budget; default 1000
  fresh = false       # default false
  config = "greeting=hi"
  calls = ["kv-store"]  # services it may `call`; default none (not even itself); ["*"] = any (discouraged)
}
```

```
service api {
  images = { x86_64 = "sha256:…", aarch64 = "sha256:…" }  # K10I image digests, one per architecture
  replicas = 3
  memory = 384MiB        # exact limit, a multiple of 4KiB (required)
  cpu = 0.5              # cores (quota); default none
  entrypoint = ["/app"]  # replaces the image's Entrypoint; args = [...] replaces its Cmd
  env = { LOG_LEVEL = "info" }
  secrets = { DB_PASSWORD = "db-password" }  # files, sealed with the cluster key by the CLI
  workdir = "/srv"       # user = "app:app"
  http = 8080            # container port for routes and native callers
  routes = ["api.example.com", "example.com/api", "tls:secure.example.com"]
  public = [{ port = 5432, to = 5432 }]  # node ports, cluster-wide
  calls = ["db"]
  egress = ["0.0.0.0/0:443", "dns"]      # default none
  health = "http:8080/healthz"           # tcp:PORT, exec:CMD, default process
  stop_grace = 10s       # scratch = 64MiB (default memory/4); restart = always|on-failure|never
  init = false           # pids_max = 1024, fds_max = 4096, tenant = "acme"
}
```

```
os {
  images = ["k10s-os-0.1.0-x86_64-vm.efi"]  # signed OS images, at most one per platform
  wave = 1                                  # nodes in the first update wave (default 1), then doubling
}
```

A block with `images` is an image service, otherwise a native one (`code`); keys of the other kind are errors.
`calls` names must be services of the same manifest (an error otherwise: typos are caught); the list is sorted
and deduplicated. `parse_document` returns the `ServiceDecl`s, the `ImageDecl`s and the `OsDecl` (`parse_text` the
native services of a file without `os`); errors read `line N: message`. Convert each declaration with
`decl.to_spec(code_hash)`; the CLI reads each OS image, takes its platform from the signed trailer and adds it as a
blob, and seals each secret file with `ClusterKey::seal_secret` (XChaCha20-Poly1305 under a subkey of the cluster
key, bound to the service and variable, nonce derived from the inputs so an unchanged manifest keeps its hash).

## Placement

Weighted rendezvous hashing, with the weight equal to the node's `mem_bytes`. Each service ranks the nodes, and
its replicas take the top-ranked nodes that have memory left (anti-affinity). When no such node remains,
replicas stack using per-replica scores. Replica 0 of every service is placed before any replica 1. Only Alive
and Suspect members are used, and the result does not depend on input order. Every node uses the lowest placement
version of the live members. Version 3 puts a service only on nodes able to run it: a native service on nodes
whose largest slot holds it (a 32-bit node never gets more than 256 MiB; a member whose `EXT_CAPS` is not known
yet counts as unlimited), an image service on nodes with the Linux personality, one of its images' architectures,
its tenant and none of its public ports among the node's own, charged `mem_bytes + 256 KiB`. Image services stay
unplaced under an older version; `check_members` refuses a manifest with image services while a live member runs
one (the node answers 409), and `image_note` says why an image service has missing replicas. Two tests pin the
output (versions 1–2, and version 3 with capabilities and image services); 1000 nodes × 10k services take ≈ 40 ms.

## Tests

`cargo test -p k10-mesh` takes about 25 s in debug. For the simulation and placement numbers, run:

```
cargo test -p k10-mesh --release -- --nocapture
cargo test -p k10-mesh --release --test sim -- --ignored --nocapture
```
