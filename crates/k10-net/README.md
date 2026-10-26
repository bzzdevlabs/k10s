# k10-net

A TCP/IP stack for the k10s native kernel, built per CPU from the start: one instance per NIC queue, each
owning the flows its queue receives, the listeners and port table shared. It is **sans-IO** and `no_std + alloc`
(`--no-default-features`): no clocks, no sockets, no threads, no randomness — time is an argument, frames go in
and out through the caller, and every secret value (ISNs, cookies, ports, hash keys) derives from a node secret
with SipHash. Same inputs give the same outputs, so the stack runs in a deterministic simulator and under fuzzing
on the host. The crate has no dependencies. The `std` feature (default) adds the simulator (`sim`) and a
multi-instance node (`multi`).

`metal/` runs smoltcp; this crate is the stack that the native kernel's network phase moves to
([docs/research/oci.md](../../docs/research/oci.md) §8).

## Model

* **Instances and RSS.** A flow's owner is a pure function of its 4-tuple: the Toeplitz hash NICs compute (Microsoft
  RSS key and verification vectors) through a 128-entry indirection table. Multi-queue NICs steer in hardware;
  for single-queue NICs `Rss::classify(frame)` gives the queue in software. An outgoing connection picks its
  ephemeral port (RFC 6056 algorithm 4) among those whose hash maps back to the connecting instance, so its
  replies land on the CPU that owns it.
* **Shared state is immutable snapshots plus atomics.** `Ports` is the node's control-plane copy of the port table
  (listeners and UDP bindings, per view); every instance holds the `Arc<PortTable>` it publishes. `IfaceConfig`
  (addresses, gateway, routers, prefixes, MTU) is published the same way: instance 0 runs SLAAC and takes the DHCP
  result, the embedder hands the new snapshot to the others. Listener backlog counters and memory quotas are
  atomics, so their caps hold across instances.
* **Instances talk through events** that the embedder routes (`Stack::poll_event`): `Forward` (a frame another
  instance owns: a datagram reassembled here, a UDP socket homed elsewhere, a router advertisement for instance 0),
  `Iface`, `Neighbor` (link addresses learned from ARP/NDP replies, which arrive on whatever queue). Readiness
  comes out the same way: `Tcp { handle, ready }`, `Accept { listener }`, `Udp { handle }`.
* **Driving an instance:** `receive(now, frame)` per received frame; `transmit(now, &mut impl TxSink)` builds
  frames straight into the NIC's buffers (`TxSink::send(len, fill)`, the shape of metal's `NetTx`);
  `poll_at(now)` is the next deadline. Output is event-driven: a connection limited by its windows waits for an ACK
  or a timer instead of being polled.
* **Buffers.** Applications write into and read from the socket rings in place (`tcp_send_with`,
  `tcp_recv_with`, which can also peek); TCP payload is copied once into the frame being built. Rings start small
  and grow geometrically on demand (receive autotuning per round trip, send buffer to twice the congestion window),
  each byte charged to the node budget and the socket's view budget.

| module | what |
|---|---|
| `wire` | zero-copy parsers and emitters: Ethernet, ARP, IPv4, IPv6 with extension headers, ICMPv4/ICMPv6/ND/MLDv2, UDP, TCP and options, checksums |
| `stack` | `Stack` (one instance): demultiplexing, flow table, listeners, SYN cookies, timers, output, events |
| `tcp` | connections (`tcb`), SACK scoreboard and RACK (`sendq`), NewReno/CUBIC/PRR (`cc`), RTO (`rtt`), ISNs/cookies/ports (`secrets`) |
| `udp`, `ndp` | UDP sockets; neighbor discovery, SLAAC, DAD, router advertisements, MLD (as `impl Stack`) |
| `iface`, `neigh`, `frag` | interface snapshot and source/next-hop selection; ARP/NDP neighbor cache; IPv4/IPv6 reassembly |
| `ports`, `buf`, `rss` | views, listeners, UDP bindings; quotas, rings, out-of-order assembler, frame pool; Toeplitz hash |
| `sim`, `multi` (std) | deterministic link simulator, test host; a node of N instances with software RSS and event routing |

## Protocols

* **IPv4**: header checksum, options accepted, broadcast for UDP only, DF on TCP, no transmit fragmentation.
  **IPv6**: extension-header chains (hop-by-hop first only, unknown options per their action bits, routing headers
  with segments left refused, at most 8 headers), first fragments must carry the whole chain (RFC 7112), atomic
  fragments (RFC 6946), flow labels per flow (RFC 6437).
* **Reassembly** of both families: datagrams, bytes and ranges per datagram capped, 30 s (IPv4) / 60 s (IPv6)
  deadlines, any overlap other than an exact duplicate discards the datagram (RFC 5722, applied to IPv4 as well).
* **ARP** (RFC 826) and **NDP** (RFC 4861: hop limit 255, no fragmented ND per RFC 6980) through one neighbor cache:
  resolution retries, reachability probes, a few frames held per unresolved neighbor. **SLAAC** (RFC 4862) with
  DAD on every address, router solicitations, prefix and lifetime handling with the two-hour rule, MTU option,
  RDNSS (RFC 8106), MLDv2 reports for the solicited-node groups (RFC 3810), source address selection by scope and
  longest match.
* **ICMP**: echo replies, port unreachable (rate limited, never for broadcast/multicast), fragmentation needed /
  packet too big lowering a connection's segment size when the quoted sequence number is in flight (RFC 5927).
* **UDP**: bounded receive queues; an explicit source address (`0.0.0.0` towards a broadcast), broadcast
  permission and reception before an address is configured (`UdpOptions`) for DHCP; random ephemeral ports for DNS
  and SNTP.
* **TCP** (RFC 9293): window scaling and timestamps with PAWS (RFC 7323), SACK and D-SACK (RFC 2018, 2883) with the
  RFC 6675 pipe, RACK-TLP loss detection (RFC 8985, time-ordered transmissions, reordering window adapted by
  D-SACK), proportional rate reduction (RFC 6937), CUBIC (RFC 9438, integer arithmetic) or NewReno (RFC 6582; the
  recovery for peers without SACK), byte-counted slow start (RFC 3465), IW10 (RFC 6928), RTO per RFC 6298 (200 ms
  minimum), restart after idle, delayed ACKs (every second segment or 40 ms, quick ACKs at the start), Nagle and
  `nodelay`, receiver and sender silly-window avoidance, zero-window probes, keepalive (reset after the probes,
  like Linux), TIME-WAIT (60 s, capped; RFC 1337: resets ignored; a newer SYN reopens, RFC 6191), FIN-WAIT-2
  timeout for closed sockets, a reset when a socket closes with unread data or receives data after close.
  Hardening: RFC 5961 challenge ACKs for in-window resets and SYNs and for old ACKs (rate limited per connection),
  RFC 6528 ISNs, SYN cookies (time slot, MSS index and a 24-bit MAC; window scale and SACK in the timestamp's low
  bits) once a listener's half-open backlog is full, accept-queue caps.

## Limits

Every cap is explicit; nothing grows without one.

| where | caps |
|---|---|
| `StackConfig` | connections per instance (65536) and TIME-WAIT among them (16384), UDP sockets (1024), UDP receive queue (256 datagrams / 256 KiB per socket), control frames waiting (1024), events waiting (4096), ICMP messages per second (1000, burst 50) |
| `FragConfig` | datagrams in reassembly (64), bytes held (512 KiB), ranges per datagram (32) |
| `NeighborConfig` | entries (512), frames held per unresolved neighbor (4) |
| `TcpConfig` | receive buffer 64 KiB → 4 MiB, send buffer 16 KiB → 4 MiB, out-of-order ranges (32), segments tracked in flight (16384), retries (6 SYN, 5 SYN-ACK, 15 data), challenge ACKs per second (10) |
| `ListenerConfig` | accept backlog (128) and half-open backlog (256) across all instances; SYN cookies beyond it |
| `Quota` | bytes and sockets: one node budget shared by all instances, optionally one per view |
| `PortsConfig`, `iface` | ephemeral range (49152–65535), listeners and UDP bindings (1024 each); 8 IPv6 addresses, 4 routers, 8 prefixes, 4 name servers |

## Socket views and ingress handoff

Sockets belong to a view (`ViewId`, one per container; `ViewId::NODE` for the node). Each view has its own TCP
and UDP port spaces, so two containers can both listen on port 80; only node-view listeners take connections
from the wire. `tcp_connect(view, quota, …)` opens an egress connection from the node's address and port range,
charged to the view's quota. `tcp_hand_off(handle, listener, unread)` moves an accepted connection into another
view's accept queue: the bytes the kernel already consumed (a request head, a TLS ClientHello) go back in front of
the stream, the memory is recharged to the target's quota, the old handle stops working, and `tcp_endpoints` still
reports the real client. A peek (`tcp_recv_with` consuming nothing) lets the kernel route without consuming.

## Tests

| suite | what |
|---|---|
| unit (`cargo test -p k10-net --lib`) | per module: wire formats, RSS (Microsoft vectors), SipHash vectors, RFC 6298, CUBIC, PRR, SACK/RACK, cookies, quotas, rings, reassembly |
| `tests/rfc.rs` | TCP per RFC against a scripted peer: options, resets for closed ports, RFC 5961, PAWS, SACK and D-SACK generation, delayed ACK, Nagle, zero-window probes, SYN cookies, backlog caps, TIME-WAIT, backoff and timeouts, keepalive, window scaling |
| `tests/ip.rs` | ARP, ICMP, UDP and port unreachable, IPv4 fragments, PMTU, DHCP-style broadcast, SLAAC/DAD/RA/NDP, ingress handoff into a view, quotas |
| `tests/sim.rs` | simulated links with loss, reordering, duplication, delay and bandwidth limits; multi-megabyte transfers both ways checked byte for byte; NewReno without SACK; IPv6 |
| `tests/multi.rs` | 4-instance nodes: flows spread by RSS, ephemeral ports steered back, UDP homed on one instance, configuration reaching every instance |
| `tests/fuzz.rs` | parsers on arbitrary bytes, option round trips, ring and assembler against models, reassembly exact-or-nothing, 150 000 mutated frames through a live stack (all memory returned at the end) |
| `tests/interop.rs` (`interop/run.sh`) | Linux 6.1's own stack in a QEMU guest on a UDP socket netdev, k10-net on real time in the test process: handshake options, bulk both ways clean and with 2 % loss (SACK recovery on both sides), Linux's reset and a refused connect, FIN to TIME-WAIT, IPv6 with loss, Linux connecting to us over both families, UDP |

`interop/run.sh` needs QEMU, a readable `/boot/vmlinuz-$(uname -r)` and the `x86_64-unknown-linux-musl` target;
no privileges.

## Benchmark

`bench/` (a standalone tool, so smoltcp never enters the workspace) drives k10-net and smoltcp 0.14 through the
same simulated links, one connection, 4 MiB buffers: `cargo run --release --manifest-path bench/Cargo.toml`.
Goodput is in virtual time (the protocol: windows, congestion control, loss recovery); CPU per byte is wall time
inside the two stacks and the application's socket calls, on this machine under load (±20 %).

Goodput in Mbit/s, CPU in ns per byte (both stacks and the application together):

| link, transfer | k10-net CUBIC | k10-net NewReno | smoltcp CUBIC | smoltcp Reno | smoltcp, no congestion control (metal's build) |
|---|---|---|---|---|---|
| 10 Gbit/s, 0.1 ms RTT, 256 MiB | 7885, 1.17 | 7885, 1.08 | 1729, 0.97 | 1729, 0.90 | 1.0, 1.54 |
| same, 1 % loss, 64 MiB | 1502, 0.91 | 1117, 0.90 | 20.2, 0.87 | 9.0, 0.87 | 0.1 (44 MiB in the hour limit), 4.71 |
| 1 Gbit/s, 20 ms RTT, 0.1 % loss, 64 MiB | 43.7, 1.16 | 28.6, 1.08 | 23.2, 0.95 | 17.9, 0.90 | 0.8, 2.09 |

The CPU cost per byte is 5–25 % above smoltcp's; the protocol moves 4.6× more on the clean link and 74× more at
1 % loss (CUBIC against CUBIC; smoltcp has no SACK or RACK). On the clean 10 Gbit/s link the k10-net
retransmissions (2832) are slow-start overshoot dropped by the link's 4 MiB queue.
