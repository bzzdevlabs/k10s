# k10s on a Linux kernel: PID 1 mode (Tier 1)

The static `k10s` binary is the only userspace program of a Linux image: the kernel is a driver layer, and there is no
shell, udev, systemd, DHCP daemon or SSH. This is Tier 1 of the platform plan (docs/REPORT.md §9,
docs/research/universal-os.md §8–9); Tier 2 is the native kernel in `metal/`.

What exists today (milestone U0): PID 1 mode in `crates/k10s/src/pid1/`, the k10s kernel configuration and build
(`os/`), and QEMU tests on the k10s kernel and the Debian kernel. Bootable disk images (BIOS and UEFI), signed
updates, arm64 and the Raspberry Pi come next (U1–U3); until then an image is a kernel plus an initramfs.

## Boot sequence

The kernel runs `/init` from the initramfs; `k10s` started as PID 1 under the name `init` enters PID 1 mode (as PID 1
of a container, `docker run … k10s node`, it keeps its normal command line).

1. **Mounts:** devtmpfs on `/dev`, `/proc`, `/sys`; the console is reopened if the initramfs has no `/dev/console`.
   SIGINT, SIGTERM, SIGHUP, SIGCHLD, SIGPWR and SIGRTMIN+4 are blocked in every thread and handled by the
   supervision loop; Ctrl-Alt-Del becomes SIGINT.
2. **Drivers:** every `/sys/devices/**/modalias` is matched against `modules.alias` and the modules are loaded with
   their dependencies (`modules.dep`, `modules.softdep`) by `finit_module`, compressed modules included; scans repeat
   until nothing new appears. A netlink uevent listener does the same for devices added later, and
   `/proc/sys/kernel/modprobe` points at `/init`, which loads on-demand requests (`/init -q -- NAME`). A kernel with
   every driver built in ships no modules and skips this.
3. **Configuration:** the GPT partition named `K10S-BOOT` (FAT) is mounted read-only on `/boot`, root-only
   (files 0600), and its `k10s.conf` merged (keys below).
4. **Network:** loopback and the first Ethernet interface (`iface` to choose). A static `ip`, else an address the
   kernel set (`ip=`), else DHCPv4: the client follows RFC 2131 (renews at T1 by unicast to its server, rebinds at T2
   by broadcast, starts over on a NAK or at the end of the lease), writes `/etc/resolv.conf` from the lease, and keeps
   retrying in the background. Init waits up to 30 s for the first lease, since a cluster node advertises the
   interface's address; when the address changes, a node that advertises it is restarted. IPv6 addresses come from
   the kernel's SLAAC.
5. **Hostname:** `name`, else `k10s-` and the last three bytes of the MAC (the node's default name).
6. **Data:** the partition named `K10S-DATA` (or `data=/dev/X[:FSTYPE]`), ext4 by default, on `/data`, handed to the
   node's user when fresh; without one the node's state lives in RAM.
7. **Time:** an SNTP client (RFC 4330) sets the clock in the background from `ntp`, else the lease's NTP servers,
   else `pool.ntp.org`; offsets of 128 ms or more step the clock, smaller ones slew it, and it polls every 17 minutes.
   A reply must echo the random transmit timestamp of its request (SNTP itself is unauthenticated).
8. **Node:** `k10s node --data /data --http 0.0.0.0:80 --user 65532:65532` plus every non-init key, as a child
   process. The node reads its key files, binds, then enters the normal sandbox (user switch, no capabilities,
   Landlock, seccomp); init keeps the privileges the system work needs. When the node exits, init restarts it after
   1 s, doubling to 60 s (reset after a minute of uptime).
9. **Health and watchdog:** init probes `GET /_k10s/health` on loopback and logs when the node is first ready. With
   `watchdog=1` it opens `/dev/watchdog` and pets it every 5 s while the node answered within the last minute (or is
   still starting, up to 5 minutes), so a hung node or kernel resets the machine.
10. **Power off:** the power button (ACPI button with evdev, or the kernel's tiny power button signal), SIGTERM or
    SIGPWR stop the node (SIGTERM, 30 s to leave the cluster and drain, then SIGKILL), then init syncs, unmounts
    `/data` and `/boot` (read-only remount if busy), disarms the watchdog and powers off; Ctrl-Alt-Del reboots. If
    init itself fails, the kernel panics: boot with `panic=5` to reboot.

Console lines of init start with `k10s-init[SECONDS]:`, the time since the kernel started.

## Configuration

Keys come from, in increasing precedence: `/etc/k10s.conf` baked into the initramfs, `k10s.conf` on `K10S-BOOT`,
and `k10s.KEY=VALUE` on the kernel command line. Files hold `KEY=VALUE` lines (`#` comments, optional `k10s.`
prefix). Init consumes these keys:

| Key | Meaning | Default |
|---|---|---|
| `net` | `none` to leave networking alone | DHCP |
| `ip` | static address `A.B.C.D/P[,GATEWAY]` | DHCP |
| `dns` | name servers `A.B.C.D,…` (overrides the lease's) | the lease's |
| `iface` | interface to configure | first Ethernet interface by name |
| `netwait` | seconds to wait for an Ethernet interface | 10 |
| `ntp` | `HOST[:PORT],…`, or `none` | the lease's, else `pool.ntp.org` |
| `modules` | `none` to skip module loading | load by modalias |
| `data` | `/dev/X[:FSTYPE]`, or `none` | partition `K10S-DATA`, ext4 |
| `bootwait` | milliseconds to wait for `K10S-BOOT` to appear | 0 |
| `watchdog` | `1` to arm `/dev/watchdog` | off |
| `prune_modules` | `1` to free unused module files from the RAM initramfs after boot | off |
| `debug` | `1` to log each module match | off |

Every other key is a `k10s node` flag: `KEY=VALUE` becomes `--KEY VALUE`, and the valueless flags (`log-stderr`,
`pin`, `insecure-unauthenticated-cluster`) are on unless `0`, `false`, `no` or `off`. `http` defaults to
`0.0.0.0:80` and `user` to `65532:65532`. There is no shell to read a generated admin key, so give the node yours:
create it with `k10s keygen -o admin.key`, copy it to `K10S-BOOT` and set `admin-token-file=/boot/admin.key`
(likewise `cluster-key-file=/boot/cluster.key` for a cluster).

## Entropy

Keys, nonces and node secrets use blocking `getrandom`; non-key seeds (node ids, shard generators, shuffles, DHCP
transaction ids, SNTP nonces) use `GRND_INSECURE`, which never blocks at early boot. The k10s kernel builds the
hardware RNG drivers in (`HW_RANDOM`, `HW_RANDOM_VIRTIO`), so the kernel's generator is seeded early.

## Building

| Script | Does |
|---|---|
| `os/kbuild.sh [OUT]` | the k10s kernel, `vm` profile: Linux 6.18.57 (kernel.org tarball, SHA-256 pinned) from `tinyconfig` plus `os/kconfig/{common,x86-vm,x86_64}.config`, no modules, rootless; fails if an option does not survive `olddefconfig`; reproducible build metadata |
| `os/debian-kernel.sh` | the pinned Debian 13 kernel (6.12, SHA-256 checked) with its module tree and `depmod` output |
| `os/mkinitramfs.py OUT --init K10S [--kmods DIR --mod GLOB…] [--conf FILE]` | a reproducible newc initramfs (gzip, xz or zstd), module subsets with their dependencies, filtered `modules.alias` |

The kernel build needs make, gcc, flex, bison, bc and the libelf headers. The `vm` profile covers QEMU/KVM guests:
virtio (PCI, net, block, RNG), the i6300esb watchdog, the CMOS RTC, ACPI with the tiny power button, seccomp and
Landlock. `metal` profiles for real machines come with U1.

## Testing

`os/test.sh` (CI suite `os`, a release gate) builds the static k10s, both kernels and their initramfs images, and
runs `os/test.py` under QEMU (KVM when available), with no root:

- **boot** on the Debian and the k10s kernel: a GPT disk with `K10S-BOOT` (`k10s.conf`, the admin key) and
  `K10S-DATA` (ext4); the guest RTC starts in 2001, so the signed `k10s apply` only succeeds after SNTP (a host-side
  server) set the clock. Checks the drivers, config, DHCP lease (QEMU user networking), data partition, watchdog,
  the node's sandbox line, readiness, serving, and a power-button shutdown with the node drained; a second boot
  serves the applied services and continues their KV state.
- **dhcp** on the k10s kernel: a scripted DHCP server on a QEMU socket netdev hands out 6 s leases; the client must
  renew by unicast, rebind by broadcast when renewals go unanswered, and take a new address after a NAK; the node,
  in cluster mode without `advertise`, is restarted to advertise it.

Unit tests cover the configuration precedence, module tree parsing and modalias matching, the DHCP state machine
against a fake server, and SNTP sample checks.

Real hardware is needed for: drivers outside QEMU's virtio set (NICs, AHCI/NVMe, USB), firmware files, slow or
asynchronous USB storage (`bootwait`), hardware watchdog and RTC chips, and real DHCP servers and NTP pools.

## Limits

- An image is a kernel plus an initramfs; no disk image builder, bootloader setup or A/B updates yet (U1, U2).
- x86-64 only; the hosted node builds for Linux x86-64 only (U3 ports it).
- No DHCP address conflict detection (ARP probe), no DHCPv6; one configured interface.
- `join` host names are resolved once, when the node starts (name servers from the lease or `dns`).
