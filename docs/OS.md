# k10s on a Linux kernel: PID 1 mode (Tier 1)

The static `k10s` binary is the only userspace program of a Linux image: the kernel is a driver layer, and there is no
shell, udev, systemd, DHCP daemon or SSH. This is Tier 1 of the platform plan (docs/REPORT.md §9,
docs/research/universal-os.md §8–9); Tier 2 is the native kernel in `metal/`.

What exists today (milestones U0 to U2): PID 1 mode in `crates/k10s/src/pid1/`, the k10s kernels (`vm` and `metal`
profiles, `os/`), x86-64 disk images that boot on BIOS and UEFI, signed A/B updates of the image rolled out by the
cluster in waves, and QEMU tests of all of them. arm64 and the Raspberry Pi come next (U3).

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
   kernel set (`ip=`), else DHCPv4 once the link is up (carrier, up to `netwait`): the client follows RFC 2131 (renews at T1 by unicast to its server, rebinds at T2
   by broadcast, starts over on a NAK or at the end of the lease), writes `/etc/resolv.conf` from the lease, and keeps
   retrying in the background. Init waits up to 30 s for the first lease, since a cluster node advertises the
   interface's address; when the address changes, a node that advertises it is restarted. IPv6 addresses come from
   the kernel's SLAAC.
5. **Hostname:** `name`, else `k10s-` and the last three bytes of the MAC (the node's default name).
6. **Data:** the partition named `K10S-DATA` (or `data=/dev/X[:FSTYPE]`), ext4 by default, on `/data`, handed to the
   node's user when fresh; without one the node's state lives in RAM. When `K10S-DATA` is the last partition of a GPT
   disk with free space behind it (an image written to a larger disk), init grows it to the end of the disk first:
   backup GPT moved to the disk's end, partition extended to the last 1 MiB boundary (backup table written and synced
   before the primary), `BLKPG_RESIZE_PARTITION`, then the mounted ext4 is grown online (`EXT4_IOC_RESIZE_FS`). A
   filesystem smaller than its partition (power lost between the two steps) is grown on the next boot. Other
   partitions are never touched; `grow=0` keeps the size.
7. **Time:** an SNTP client (RFC 4330) sets the clock in the background from `ntp`, else the lease's NTP servers,
   else `pool.ntp.org`; offsets of 128 ms or more step the clock, smaller ones slew it, and it polls every 17 minutes.
   A reply must echo the random transmit timestamp of its request (SNTP itself is unauthenticated).
8. **Node:** `k10s node --data /data --http 0.0.0.0:80 --user 65532:65532` plus every non-init key, as a child
   process (with `--os-agent-fd 3`, the update channel, when the image booted from one of its slots: "Updates").
   The node reads its key files, binds, then enters the normal sandbox (user switch, no capabilities, Landlock,
   seccomp); init keeps the privileges the system work needs. When the node exits, init restarts it after 1 s,
   doubling to 60 s (reset after a minute of uptime).
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
| `netwait` | seconds to wait for an Ethernet interface, then for its link | 10 |
| `ntp` | `HOST[:PORT],…`, or `none` | the lease's, else `pool.ntp.org` |
| `modules` | `none` to skip module loading | load by modalias |
| `data` | `/dev/X[:FSTYPE]`, or `none` | partition `K10S-DATA`, ext4 |
| `bootwait` | milliseconds to wait for `K10S-BOOT` to appear | 0 |
| `grow` | `0` to keep the size of `K10S-DATA` | grow to the end of the disk |
| `watchdog` | `1` to arm `/dev/watchdog` | off |
| `prune_modules` | `1` to free unused module files from the RAM initramfs after boot | off |
| `os_key` | public keys (`HEX,…`) trusted for OS images besides the k10s release key | none |
| `os_trial` | seconds a trial boot of a new image has to commit before init falls back | 600 |
| `debug` | `1` to log each module match | off |

GRUB sets `slot` on the command line of BIOS boots (the slot booted, "Updates").

Every other key is a `k10s node` flag: `KEY=VALUE` becomes `--KEY VALUE`, and the valueless flags (`log-stderr`,
`pin`, `insecure-unauthenticated-cluster`) are on unless `0`, `false`, `no` or `off`. `http` defaults to
`0.0.0.0:80` and `user` to `65532:65532`. There is no shell to read a generated admin key, so give the node yours:
create it with `k10s keygen -o admin.key`, copy it to `K10S-BOOT` and set `admin-token-file=/boot/admin.key`
(likewise `cluster-key-file=/boot/cluster.key` for a cluster).

## Entropy

Keys, nonces and node secrets use blocking `getrandom`; non-key seeds (node ids, shard generators, shuffles, DHCP
transaction ids, SNTP nonces) use `GRND_INSECURE`, which never blocks at early boot. The k10s kernel builds the
hardware RNG drivers in (`HW_RANDOM`, `HW_RANDOM_VIRTIO`), so the kernel's generator is seeded early.

## Disk images

`os/images.sh` builds the two x86-64 images of a release; both boot on BIOS and on UEFI (Secure Boot off) from any
disk, and a release publishes them with their UKIs:

| Release file | For | Kernel | Compressed size |
|---|---|---|---|
| `k10s-os-V-x86_64-vm.img.xz` | QEMU/KVM guests and clouds | `vm` profile, every driver built in; console on serial | 4.3 MB |
| `k10s-os-V-x86_64-metal.img.xz` | PCs, mini-PCs, servers | `metal` profile, signed driver modules, NIC firmware; console on the screen | 6.7 MB |
| `k10s-os-V-x86_64-{vm,metal}.efi` | updates (`os` in `k10s apply`), PXE or another UEFI loader | the signed slot-A UKI of the image | 4.2 / 6.6 MB |

Write an image to the whole disk (`xz -dc IMG.xz | dd of=/dev/sdX bs=4M conv=fsync`, or any image writer that reads
`.xz`), copy `k10s.conf` and key files to the `K10S-BOOT` partition (FAT, readable on any PC), and boot. The first boot
grows `K10S-DATA` to the end of the disk.

Layout (GPT, the shared layout of docs/research/universal-os.md §8.4):

```
MBR  GRUB boot.img in the first 440 bytes (BIOS), protective MBR
p1   bios-boot   1 MiB    GRUB core.img (BIOS)
p2   K10S-BOOT   256 MiB  EFI system partition, FAT32:
       /EFI/BOOT/BOOTX64.EFI   systemd-boot (UEFI)
       /EFI/Linux/k10s-a.efi   slot A: UKI = systemd-stub + kernel + initramfs + command line, signed
       /loader/loader.conf     no menu, command-line editor off, default k10s-a.efi
       /boot/grub/grub.cfg     BIOS: boots the slot of grubenv (k10s_slot), or a trial slot counted down
       /boot/grub/grubenv      k10s_slot=a
       /k10s/a/                vmlinuz, initrd, boot.cfg: the payload of the UKI and GRUB's lines for it
       /k10s/tier              auto (for the Tier 2 loader)
       /k10s.conf              node configuration (a commented template)
p3   K10S-DATA   256 MiB  ext4, grown to the end of the disk on first boot
```

UEFI firmware starts systemd-boot, which starts the UKI; BIOS starts GRUB from the MBR, which reads `grub.cfg` from
the ESP of the boot disk; GRUB shows nothing unless it fails, and then prints to the screen and the first serial
port. Slot B (`k10s-b.efi`, `/k10s/b/`) appears with the first update. The kernel command line is fixed in both
(`console=ttyS0,115200 quiet panic=5` for `vm`; `metal` adds `console=tty0` last, so init's lines appear on the
screen); everything else is configured in `k10s.conf`. Images are reproducible: fixed GUIDs, volume ids, ext4 hash seed, and every timestamp from
`SOURCE_DATE_EPOCH` (the commit time). On the ESP, `BOOTX64.EFI` becomes the Tier 2 loader (`k10boot`, which reads
`/k10s/tier`) with milestone M1.

## Updates

A node installs a new image when the cluster's desired state names one for its platform, in waves, and keeps it only
once it is back in the cluster; anything else ends on the image it ran before.

**Images.** An OS image is a UKI with an Ed25519 signature appended (`crates/k10-mesh/src/osimage.rs`): the file
stays bootable, and its SHA-256 is the image hash. It names its architecture, kernel profile and version; the
platform of a node is `ARCH-PROFILE` of the image it runs (`x86_64-vm`, `x86_64-metal`). A release signs its images
(the `.efi` files) with the k10s release key, whose public key (`os/release-key.pub`) is built into `k10s`;
`os_key` adds keys of your own, and `k10s image sign --key KEY --arch x86_64 --profile vm --version V IN OUT` signs an
image you built (`k10s keygen -o KEY` makes a key, `k10s image pubkey KEY` prints the public key to pin,
`k10s image verify FILE` checks one).

**Rolling out.** The images go in the desired state like code, one per platform, with the size of the first wave:

```
os {
    images = ["k10s-os-0.1.0-x86_64-vm.efi", "k10s-os-0.1.0-x86_64-metal.efi"]
    wave = 1    # nodes in the first wave; each next wave is twice as large
}
```

`k10s apply` sends the files with the manifest (they must carry a valid signature), and every node fetches them from
its peers like code blobs. Nodes report the image they run in gossip (platform, image, trial or committed, the last
image that failed there). Every node evaluates the same rollout on its own view of the membership
(`crates/k10-mesh/src/rollout.rs`): the nodes of a platform, ranked by `sha256(image | node id)`, form waves of
`wave`, `2·wave`, `4·wave`, … nodes; a node starts when every node of the earlier waves runs the image, committed.
Nodes that left or died stay in the ranking for 5 minutes (the time gossip remembers them), so a node rebooting into
the image keeps its place, and a node that just started with peers (configured or remembered) judges its turn only
once it sees one and after 15 s, when gossip has given it the membership. When a node falls back from an image, every node sees it failed and the rollout stops
until the desired state names another image. Nodes that did not boot from a slot (another loader, a `-kernel` boot)
report nothing and never update; neither do nodes outside cluster mode.

**Installing** (init, `crates/k10s/src/pid1/update.rs`). On its turn the node asks init over the update channel to
install the image from `DATA/blobs/HASH`. Init reads it without following links, checks the hash, the signature
against the trusted keys and the platform, refuses images that failed here before, and writes the other slot on
`K10S-BOOT` (remounted read-write for the writes only): the kernel, initramfs and GRUB lines under `/k10s/X/`, then
the UKI as `EFI/Linux/k10s-X+3.efi`. It then arms a trial boot in both loaders and reboots (the node first leaves the
cluster and drains):

- UEFI: `loader.conf` says `default k10s-X+[1-9]*`; systemd-boot counts the tries down in the file name
  (`k10s-X+2-1.efi`, …) and, once none are left, boots the committed slot (entries without tries sort last).
- BIOS: `grubenv` says `k10s_try=X3`; `grub.cfg` counts it down to `X0`, then boots `k10s_slot`.

Every file is written under a temporary name, synced, renamed and synced again (vfat fills the new name's directory
entry only when the file itself is written back), and the GRUB environment block is rewritten in place within its
first sector, so a power cut at any point boots a complete image.

**Committing.** A trial boot (init knows it from systemd-boot's `LoaderEntrySelected` in efivarfs, or from GRUB's
`slot` and `k10s_try`) starts the node as usual. Once the node holds and applied the cluster's desired state, sees a
peer again (if it had peers before) and stayed so for `--os-commit-grace` seconds (a node flag, default 30), it asks
init to commit: init drops the counter from the file name, sets `default k10s-X.efi` and `k10s_slot=X`. A trial that
crashes or hangs (the watchdog, `watchdog=1`, resets it) uses up its 3 boots; one that does not commit within
`os_trial` seconds is abandoned by init. Either way the machine boots the previous slot, whose init records the image
in `K10S-BOOT/k10s/failed` (the node reports it, so the rollout stops), removes the other slot's files and points both
loaders at itself.

### Kernels

Linux 6.18.57 (kernel.org tarball, SHA-256 pinned) from `tinyconfig` plus `os/kconfig/common.config`, a profile and
`x86_64.config`; kernel lockdown in integrity mode (no unsigned modules, `/dev/mem`, kexec, MSR writes or ACPI table
overrides), seccomp, Landlock, the hardware RNG drivers.

- **`vm`** (3.1 MB): QEMU/KVM guests; virtio (PCI, net, block, RNG), e1000/e1000e, the i6300esb watchdog, the CMOS
  RTC, the ACPI power button, no modules.
- **`metal`** (3.8 MB): real machines. Built in: ACPI (power button, processor idle, thermal, frequency scaling), PCI
  and MSI, IOMMU (Intel VT-d and AMD-Vi, on), AHCI and legacy Intel SATA, NVMe, USB host (xHCI, EHCI) and USB
  storage, eMMC/SD, virtio, serial, VGA text and EFI framebuffer consoles, PC keyboard. Modules (63, signed): Ethernet
  (Intel e1000, e1000e, igb, igc, ixgbe, i40e, ice; Realtek r8169 family, including RTL8125; Broadcom tg3, bnx2;
  Aquantia; Marvell sky2; Atheros alx, atl1c; virtio-net), USB Ethernet (Realtek r8152, ASIX, CDC ECM/NCM, SMSC,
  Microchip LAN78xx), watchdogs (Intel TCO through the LPC bridge or the SMBus controller, AMD SP5100/FCH, ACPI WDAT,
  i6300esb), OHCI/UHCI.

Modules are signed (RSA-4096, SHA-512) with a key that `os/kbuild.sh` creates for the build and deletes after
`modules_install`: no other module can ever be signed for that kernel, and the kernel refuses unsigned or foreign
modules (`MODULE_SIG_FORCE`, lockdown). They are zstd-compressed and decompressed by the kernel. `K10S_MODULE_KEY=PEM`
signs with a key you keep instead, which makes the kernel build reproducible.

### Firmware

`os/firmware.sh` builds the firmware archive of the `metal` image: the 43 files that its modules declare
(`modinfo -F firmware`: Realtek `rtl_nic/`, Broadcom `tigon/` and `bnx2/`, Intel `ice` DDP) from linux-firmware
`20261019`, each checked against its SHA-256 in `os/firmware.list`, with their licence files
(`/lib/firmware/LICENSES/`). It is a separate cpio appended to the initramfs; the files keep their own redistributable
licences, not k10s's Apache-2.0. Intel e1000e/igb/igc/ixgbe/i40e need no firmware.

## Building

| Script | Does |
|---|---|
| `os/images.sh [OUT]` | the release images and their signed UKIs (above), `SHA256SUMS.os`; fails when a compressed image exceeds its budget (`vm` 5 MB, `metal` 40 MB). `K10S_BIN` picks the static k10s, `K10S_IMAGE_KEY` the signing key file (default: a throwaway `target/os/image-test.key`, trusted only where pinned with `os_key`) |
| `os/kbuild.sh [vm\|metal] [OUT]` | a k10s kernel, rootless; fails if a fragment option does not survive `olddefconfig`; reproducible build metadata; `metal` also installs the signed module tree with its `depmod` output |
| `os/mkimage.py OUT.img --kernel K --initrd I… --cmdline C --boot DIR [--conf F] [--uki OUT.efi] [--version V --sign KEY --k10s K10S --profile P]` | a disk image: GPT, GRUB patched into the MBR and `bios-boot` (no `grub-install`), the UKI built with `objcopy` and signed with `k10s image sign` (unsigned without `--sign`: it boots but cannot update), the A/B slot layout, FAT with mtools, ext4 with `mkfs.ext4`; no root, no loop devices |
| `os/bootfiles.sh` | the pinned Debian 12 GRUB (`i386-pc`, `grub-mkimage`) and systemd-boot packages (systemd-boot, systemd-stub), SHA-256 checked |
| `os/firmware.sh` | the firmware archive (above) |
| `os/debian-kernel.sh` | the pinned Debian 13 kernel (6.12, SHA-256 checked) with its module tree and `depmod` output |
| `os/mkinitramfs.py OUT [--init K10S] [--kmods DIR --mod GLOB…] [--firmware DIR --fw GLOB…] [--conf FILE]` | a reproducible newc initramfs (gzip, xz or zstd), module subsets with their dependencies, filtered `modules.alias`; without `--init`, an archive to append to another |

The kernel builds need make, gcc, flex, bison, bc, openssl, depmod and the libelf and libssl headers; the images
need objcopy, sfdisk, mkfs.fat, mtools, mkfs.ext4, dpkg-deb, zstd and xz.

## Testing

`os/test.sh` (CI suite `os`, a release gate) builds the static k10s, the kernels, the Debian kernel, the initramfs
images and the release images, then runs `os/test.py` under QEMU (KVM when available), with no root. The release runs
it on the k10s binary of the release's musl archive and publishes the images it tested.

- **boot-debian, boot-k10s**: kernel and initramfs on a GPT disk with `K10S-BOOT` (`k10s.conf`, the admin key) and
  `K10S-DATA` (ext4); the guest RTC starts in 2001, so the signed `k10s apply` only succeeds after SNTP (a host-side
  server) set the clock. Checks the drivers, config, DHCP lease (QEMU user networking), data partition, watchdog,
  the node's sandbox line, readiness, serving, and a power-button shutdown with the node drained; a second boot
  serves the applied services and continues their KV state.
- **dhcp** on the k10s kernel: a scripted DHCP server on a QEMU socket netdev hands out 6 s leases; the client must
  renew by unicast, rebind by broadcast when renewals go unanswered, and take a new address after a NAK; the node,
  in cluster mode without `advertise`, is restarted to advertise it.
- **image-bios, image-uefi**: the `vm` image on SeaBIOS and on OVMF, written to a 4 GiB disk, configured the way an
  operator does it (`k10s.conf` and the admin key copied to `K10S-BOOT` with mtools). The first boot grows
  `K10S-DATA` and its ext4 to the end of the disk (checked on the host afterwards: both GPT headers and the entry
  array valid, the backup in the last sector, ext4 filling the partition), applies and serves as above; the second
  boot keeps the state and grows nothing.
- **image-metal**: the same on the `metal` image (with the console on serial for the test) on OVMF `q35`, booting
  from NVMe; its only NIC (e1000e) and watchdog (Intel TCO behind `lpc_ich`) are signed modules.
- **unsigned**: the `metal` kernel with an initramfs whose e1000e lost its signature refuses it ("Key was rejected by
  service") and comes up without network.
- **boottime**: QEMU start → first HTTP 200 from the node, `vm` image on SeaBIOS (GRUB, KVM, 512 MiB, fresh disk
  each time), median of 5 below 1 s (measured 0.50–0.54 s).
- **repro**: the same image built twice is bit-identical, UKI included.
- **update-bios, update-uefi**: a single-node cluster on the `vm` image applies a re-signed copy of it (another
  version, so another hash; the test signing key is pinned with `os_key`). On a fresh disk per step, the power is
  cut (QEMU killed within a millisecond of the console line) at every step: image verified, slot written, trial
  armed, trial booted, commit requested, committed; each next boot comes up on the old or the new image and serves,
  the update completes, and the KV state survives. Then an image
  whose kernel finds no init (it panics; `panic=5` reboots) falls back after 3 boots, and one whose node never asks
  to commit is abandoned after `os_trial`; both are recorded as failed and stop the rollout.
- **rollout**: 20 BIOS VMs (192 MiB each) in one cluster, reaching each other through QEMU's forwarded ports. The
  panicking image goes to the first wave only (one node), which falls back; every other node sees the rollout stopped.
  Then a good image reaches all 20 nodes in waves of 1, 2, 4, 8 and 5, each wave starting after the previous one
  committed.

Unit tests cover the configuration precedence, module tree parsing and modalias matching, the DHCP state machine
against a fake server, SNTP sample checks and the GPT rewrite that grows `K10S-DATA` (512-byte and 4 KiB sectors,
checksums, partitions that are not last, corrupt tables). The slot logic is tested against models of systemd-boot's
entry selection and of `grub.cfg`, with the power cut after every durable step of an install and of a commit on both
firmware types, and with images that never come up or never commit; the image format, Ed25519 (RFC 8032 vectors),
the manifest's OS plan, the gossip extension (never sent to nodes that predate it) and the rollout waves have their
own tests in `k10-mesh`.

Real hardware is needed for: a real machine serving after a cold boot from the `metal` image (the U1 exit criterion
for a Realtek or Intel mini-PC), power cuts that reach the disk's own write cache (QEMU's kill loses only what the
guest had not written), GRUB's `save_env` and systemd-boot's renames on real firmware and disks, NIC drivers and firmware outside QEMU's models (r8169/RTL8125, igc, tg3, USB
Ethernet), AHCI and eMMC boot disks, the screen console, firmware quirks of real BIOS and UEFI implementations
(boot drive numbers, removable-media boot paths), slow USB storage (`bootwait`), the AMD and newer Intel (SMBus)
watchdogs, RTC chips, real DHCP servers, switches (link and STP delays) and NTP pools.

## Limits

- Updates need cluster mode and an image booted through its own loaders; nodes are not grouped into zones yet.
- A rollback is an update to the older image (signed images of any version install); the cluster key is the trust
  boundary for which signed image a cluster runs.
- Secure Boot must be off (U6 documents signing the image with an own key).
- The image uses 512-byte logical sectors; disks with 4 KiB logical sectors (rare outside servers) need a rebuilt
  image.
- x86-64 only; the hosted node builds for Linux x86-64 only (U3 ports it).
- No DHCP address conflict detection (ARP probe), no DHCPv6; one configured interface.
- `join` host names are resolved once, when the node starts (name servers from the lease or `dns`).
