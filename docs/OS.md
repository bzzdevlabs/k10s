# k10s on a Linux kernel: PID 1 mode (Tier 1)

The static `k10s` binary is the only userspace program of a Linux image: the kernel is a driver layer, and there is no
shell, udev, systemd, DHCP daemon or SSH. This is Tier 1 of the platform plan (docs/REPORT.md §9,
docs/research/universal-os.md §8–9); Tier 2 is the native kernel in `metal/`.

What exists today (milestones U0 to U6): PID 1 mode in `crates/k10s/src/pid1/`, the k10s kernels (`vm` and
`metal` profiles for x86-64, one `arm64` kernel, and the best-effort `i386` and `armv7` kernels, `os/`), x86-64 disk
images that boot on BIOS and UEFI, an arm64 UEFI image, a Raspberry Pi 3/4/5 image, an i686 BIOS image for old PCs and
an armv7 UEFI image, signed A/B updates of every image rolled out by the cluster in waves, Secure Boot with your own
keys or a MOK (U6), and QEMU tests of all of them. The node runs services on the JIT on x86-64 and arm64 (U4), and
on the interpreter on i686 and armv7 (no 32-bit JIT).

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
3. **Configuration:** the partition named `K10S-BOOT` (FAT; the GPT partition name, or on an MBR disk the FAT volume
   label) is mounted read-only on `/boot`, root-only (files 0600), and its `k10s.conf` merged (keys below).
4. **Network:** loopback and the first Ethernet interface (`iface` to choose). A static `ip`, else an address the
   kernel set (`ip=`), else DHCPv4 once the link is up (carrier, up to `netwait`): the client follows RFC 2131 (renews at T1 by unicast to its server, rebinds at T2
   by broadcast, starts over on a NAK or at the end of the lease), writes `/etc/resolv.conf` from the lease, and keeps
   retrying in the background. Init waits up to 30 s for the first lease, since a cluster node advertises the
   interface's address; when the address changes, a node that advertises it is restarted. IPv6 addresses come from
   the kernel's SLAAC.
5. **Hostname:** `name`, else `k10s-` and the last three bytes of the MAC (the node's default name).
6. **Data:** the partition named `K10S-DATA` (GPT name, or the ext4 label on an MBR disk; or
   `data=/dev/X[:FSTYPE]`), ext4 by default, on `/data`, handed to the node's user when fresh; without one the node's
   state lives in RAM. When `K10S-DATA` is the last partition of its disk with free space behind it (an image written
   to a larger disk), init grows it to the end of the disk first: on GPT the backup GPT is moved to the disk's end and
   the partition extended to the last 1 MiB boundary (backup table written and synced before the primary); on MBR the
   partition entry is extended in sector 0 (up to 2^32 sectors, 2 TiB); then `BLKPG_RESIZE_PARTITION`, and the
   mounted ext4 is grown online (`EXT4_IOC_RESIZE_FS`). A filesystem smaller than its partition (power lost between
   the two steps) is grown on the next boot. Other partitions are never touched; `grow=0` keeps the size.
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
10. **Power off:** the power button (ACPI button or a devicetree `gpio-keys` key with evdev, or the kernel's tiny
    power button signal), SIGTERM or
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
transaction ids, SNTP nonces) use `GRND_INSECURE`, which never blocks at early boot. The k10s kernels build the
hardware RNG drivers in (virtio, Intel, AMD, and the Raspberry Pi's `bcm2835-rng` and `iproc-rng200`), so the kernel's
generator is seeded early.

## Disk images

`os/images.sh` builds the images of a release, per architecture (`--arch x86_64|aarch64|i686|armv7`); the x86-64 ones
boot on BIOS and on UEFI from any disk (with Secure Boot on, see "Secure Boot"). A release publishes them with their
update files:

| Release file | For | Kernel | Compressed size |
|---|---|---|---|
| `k10s-os-V-x86_64-vm.img.xz` | QEMU/KVM guests and clouds | `vm` profile, every driver built in; console on serial | 4.3 MB |
| `k10s-os-V-x86_64-metal.img.xz` | PCs, mini-PCs, servers | `metal` profile, signed driver modules, NIC firmware; console on the screen | 6.7 MB |
| `k10s-os-V-x86_64-{vm,metal}.efi` | updates (`os` in `k10s apply`), PXE or another UEFI loader | the signed slot-A UKI of the image | 4.2 / 6.6 MB |
| `k10s-os-V-aarch64-vm.img.xz` | QEMU virt (UEFI) and arm64 cloud guests | `arm64`, every driver built in; console on the PL011 serial port | 3.9 MB |
| `k10s-os-V-aarch64-vm.efi` | updates, another UEFI loader | the signed slot-A UKI of the image | 9.5 MB |
| `k10s-os-V-aarch64-pi.img.xz` | Raspberry Pi 3 B and 3 B+, 4 B, 400, CM4 and Pi 5 (SD card, USB or NVMe boot) | `arm64`; console on the serial pins (Pi 3, Pi 4) or the debug UART connector (Pi 5) | 5.3 MB |
| `k10s-os-V-aarch64-pi.fat` | updates of Pi nodes | the signed boot slot of the image (FAT) | 8.4 MB |
| `k10s-os-V-i686-metal.img.xz` | old PCs: BIOS, a 32-bit or 64-bit CPU with PAE and SSE2 (best effort) | `i386`, signed driver modules, NIC firmware; console on the screen | 5.7 MB |
| `k10s-os-V-i686-metal.efi` | updates of i686 nodes | the signed slot-A image (a UKI, booted through GRUB) | 5.7 MB |
| `k10s-os-V-armv7-vm.img.xz` | QEMU virt and other 32-bit ARM UEFI machines (best effort) | `armv7`, every driver built in; console on the PL011 serial port | 3.5 MB |
| `k10s-os-V-armv7-vm.efi` | updates, another UEFI loader | the signed slot-A UKI of the image | 3.5 MB |

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
       /loader/loader.conf     no menu, command-line editor off, default k10s-a.efi, reboot-on-error auto
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

The `aarch64-vm` image has the same layout without the BIOS parts: p1 `K10S-BOOT` (systemd-boot as
`/EFI/BOOT/BOOTAA64.EFI`, the UKI, `loader.conf`, `k10s.conf`), p2 `K10S-DATA`. UEFI arm64 machines boot it with ACPI
(the power button is the ACPI one, the clock the firmware's RTC); its command line is `console=ttyAMA0,115200 quiet
panic=5`. The `armv7-vm` image is the same with systemd-boot as `/EFI/BOOT/BOOTARM.EFI`; 32-bit ARM UEFI firmware
passes a devicetree (the power key is `gpio-keys`, the clock the EFI RTC, since the firmware keeps the PL031 for
itself).

The `i686-metal` image is for BIOS only, on an MBR disk (old BIOSes may not boot a GPT disk): GRUB's boot.img in the
MBR, its core.img in the sectors before p1, p1 `K10S-BOOT` (FAT32, marked active: `boot/grub/`, `k10s/a/`, the slot's
signed UKI as `EFI/Linux/k10s-a.efi`, the record of the slot that updates replace, and `k10s.conf`), p2 `K10S-DATA`
(ext4, grown to the end of the disk). Its kernel needs an i686 CPU with PAE (for the NX bit) and SSE2 (the i686 k10s
needs it): a Pentium 4, a Pentium M with PAE, a Core, Atom, Athlon 64 or VIA C7, or anything later; its command line is
that of `metal`.

The Raspberry Pi firmware boots a FAT partition by number, so the Pi image is an MBR disk whose boot slots are whole
partitions:

```
MBR
p1   K10S-BOOT   64 MiB   FAT32: autoboot.txt (A/B), k10s.conf, k10s/ (update records), bootcode.bin (the Pi 3's
                          second stage), an empty start.elf
p2   slot A      128 MiB  the signed slot image: a FAT16 filesystem followed by its signature
       start_cd.elf, fixup_cd.dat   Pi 3 firmware, and start4cd.elf, fixup4cd.dat, the Pi 4's (cut-down: no GPU
                                    features; the Pi 5 has its firmware in EEPROM)
       config.txt, tryboot.txt      arm_64bit, kernel8.img + initramfs8, enable_uart, gpu_mem=16 (the same file twice)
       cmdline.txt                  console=ttyS1,115200 console=ttyAMA10,115200 quiet panic=5
       kernel8.img, initramfs8      the arm64 kernel (gzip) and k10s
       bcm2710-rpi-3-b.dtb, bcm2710-rpi-3-b-plus.dtb, bcm2711-rpi-4-b.dtb, bcm2711-rpi-400.dtb,
       bcm2711-rpi-cm4-io.dtb, bcm2712-rpi-5-b.dtb, bcm2712d0-rpi-5-b.dtb
       LICENCE.broadcom             the firmware's licence
p3   slot B      128 MiB  empty until the first update
p4   K10S-DATA   256 MiB  ext4, grown to the end of the disk on first boot
autoboot.txt:  [all] tryboot_a_b=1 boot_partition=2   [tryboot] boot_partition=3
```

The Pi 4 bootloader (EEPROM from 2021 on) reads `autoboot.txt` from partition 1 and boots `boot_partition`, loading
everything else from that partition; the devicetrees are the mainline ones built with the kernel (the firmware picks
the board's; the Pi 3's under the firmware's own names, `bcm2710-*`). Write the image to an SD card, USB drive or NVMe
disk like any other. The Pi firmware comes from Debian's `raspi-firmware` package (pinned, SHA-256 checked) and keeps
its own licence. The Pi 400 and CM4 use the Pi 4 files; the Pi 500 and CM5 have no mainline devicetree in Linux 6.18.

The Pi 3 has no bootloader EEPROM: its boot ROM loads `bootcode.bin` from partition 1, which reads `autoboot.txt`
there (on every model, Raspberry Pi's bootloader maintainers state; the documentation names no models) from a
partition it deems bootable, one with a `start.elf`, hence the empty one, and loads `start_cd.elf` and the rest from
the slot partition. Every model supports the one-shot tryboot flag (Raspberry Pi documentation, "Fail-safe
OS updates"); a `bootcode.bin` that did not apply `tryboot_a_b` would read `tryboot.txt` in the trial partition, which
is why it is a copy of `config.txt`. Its Ethernet is USB (the LAN9514 of the 3 B, smsc95xx; the LAN7515 of the 3 B+,
lan78xx), behind the dwc2 controller: the interface appears a few seconds after boot (`netwait`, 10 s by default,
covers it).

## Updates

A node installs a new image when the cluster's desired state names one for its platform, in waves, and keeps it only
once it is back in the cluster; anything else ends on the image it ran before.

**Images.** An OS image is a boot payload with an Ed25519 signature appended (`crates/k10-mesh/src/osimage.rs`): a
UKI, or on the Pi a slot's FAT filesystem; either stays bootable, and the file's SHA-256 is the image hash. It names
its architecture, kernel profile and version; the platform of a node is `ARCH-PROFILE` of the image it runs
(`x86_64-vm`, `x86_64-metal`, `aarch64-vm`, `aarch64-pi`, `i686-metal`, `armv7-vm`). A release signs its images (the
`.efi` and `.fat` files)
with the k10s release key, whose public key (`os/release-key.pub`) is built into `k10s`;
`os_key` adds keys of your own, and `k10s image sign --key KEY --arch x86_64 --profile vm --version V IN OUT` signs an
image you built (`--arch aarch64 --profile pi` for a Pi slot image) (`k10s keygen -o KEY` makes a key, `k10s image pubkey KEY` prints the public key to pin,
`k10s image verify FILE` checks one).

**Rolling out.** The images go in the desired state like code, one per platform, with the size of the first wave:

```
os {
    images = ["k10s-os-0.1.0-x86_64-vm.efi", "k10s-os-0.1.0-x86_64-metal.efi", "k10s-os-0.1.0-aarch64-pi.fat"]
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

- UEFI: `loader.conf` says `preferred k10s-X.efi` and `default k10s-X+[1-9]*`; systemd-boot counts the tries down
  in the file name (`k10s-X+2-1.efi`, …) and, once none are left, boots the committed slot (entries without tries
  sort last). The images' systemd-boot (262) names the entry `k10s-X.efi` and prefers it while it has tries left;
  the systemd-boot 252 of machines installed from earlier images (updates never replace the loader) ignores
  `preferred` and matches the counted name with `default`. A trial that the firmware refuses to start (Secure Boot)
  uses a try too: systemd-boot 262 then reboots (`reboot-on-error auto`), where 252 hands the machine back to the
  firmware, which tries its other boot options.
- BIOS: `grubenv` says `k10s_try=X3`; `grub.cfg` counts it down to `X0`, then boots `k10s_slot`.

Every file is written under a temporary name, synced, renamed and synced again (vfat fills the new name's directory
entry only when the file itself is written back), and the GRUB environment block is rewritten in place within its
first sector, so a power cut at any point boots a complete image. On the arm64 and armv7 UEFI images only
systemd-boot's files are written, on the BIOS-only i686 image only GRUB's (and the UKI, the record of the slot).

On a Pi (init knows it from the firmware's `/chosen/bootloader` devicetree node, which also gives the booted
partition and the tryboot flag; the Pi 3 the same) init writes the signed slot image raw to the other slot partition,
syncs it and reads
it back from the device, records the trial in `K10S-BOOT/k10s/trying` (image hash and tries left) and reboots with
`reboot(RESTART2, "0 tryboot")`. The firmware's tryboot flag is one-shot: that boot uses the `[tryboot]` partition
of `autoboot.txt`, and any later boot (a crash, a hang reset by the watchdog, a power cut) the committed one, which
is never written during an update. Mainline Linux has no way to set the flag: `os/kpatches/` adds the Raspberry Pi
kernel's reboot hook (`RPI_FIRMWARE_SET_REBOOT_FLAGS` from the firmware driver's reboot notifier).

**Committing.** A trial boot (init knows it on UEFI from the boot counter still in the file name of the slot that
systemd-boot's `LoaderEntrySelected` in efivarfs names, from GRUB's `slot` and `k10s_try`, or from the Pi firmware's
tryboot flag) starts the node as usual. Once the node holds and applied the cluster's desired state, sees a
peer again (if it had peers before) and stayed so for `--os-commit-grace` seconds (a node flag, default 30), it asks
init to commit: init drops the counter from the file name, sets `default k10s-X.efi` and `k10s_slot=X`. A trial that
crashes or hangs (the watchdog, `watchdog=1`, resets it) uses up its 3 boots; one that does not commit within
`os_trial` seconds is abandoned by init. Either way the machine boots the previous slot, whose init records the image
in `K10S-BOOT/k10s/failed` (the node reports it, so the rollout stops), removes the other slot's files and points both
loaders at itself. On a Pi the commit rewrites `autoboot.txt` (`[all]` the new slot, `[tryboot]` the other) and
removes `k10s/trying`; a committed boot that finds `k10s/trying` reboots into the trial again (3 tries in all, like
the boot counters above), then records the image as failed.

### Kernels

Linux 6.18.57 (kernel.org tarball, SHA-256 pinned, with the patches of `os/kpatches/`) from `tinyconfig` plus
`os/kconfig/common.config` and a profile (and `x86_64.config` on x86-64); kernel lockdown in integrity mode (no unsigned modules, `/dev/mem`, kexec, MSR writes or ACPI table
overrides), seccomp, Landlock, the hardware RNG drivers. The 32-bit kernels keep the time32 syscalls
(`COMPAT_32BIT_TIME`): musl calls them when a value fits in 32 bits.

- **`vm`** (3.1 MB): QEMU/KVM guests; virtio (PCI, net, block, RNG), e1000/e1000e, the i6300esb watchdog, the CMOS
  RTC, the ACPI power button, no modules.
- **`metal`** (3.8 MB): real machines. Built in: ACPI (power button, processor idle, thermal, frequency scaling), PCI
  and MSI, IOMMU (Intel VT-d and AMD-Vi, on), AHCI and legacy Intel SATA, NVMe, USB host (xHCI, EHCI) and USB
  storage, eMMC/SD, virtio, serial, VGA text and EFI framebuffer consoles, PC keyboard. Modules (63, signed): Ethernet
  (Intel e1000, e1000e, igb, igc, ixgbe, i40e, ice; Realtek r8169 family, including RTL8125; Broadcom tg3, bnx2;
  Aquantia; Marvell sky2; Atheros alx, atl1c; virtio-net), USB Ethernet (Realtek r8152, ASIX, CDC ECM/NCM, SMSC,
  Microchip LAN78xx), watchdogs (Intel TCO through the LPC bridge or the SMBus controller, AMD SP5100/FCH, ACPI WDAT,
  i6300esb), OHCI/UHCI.

- **`arm64`** (3.6 MB gzip): one kernel for QEMU virt and arm64 cloud guests (UEFI with ACPI, or a devicetree:
  PCIe ECAM, GICv3, virtio, PL011, PL031 and the EFI RTC, the ACPI or `gpio-keys` power key, i6300esb) and the
  Raspberry Pi 3, 4 and 5 (firmware mailbox and clocks, pin control, PCIe with the Pi 5's RP1 south bridge, SD/eMMC
  (the Pi 3's SDHOST with its DMA controller), NVMe, USB host (dwc2, xHCI) and storage, GENET and the RP1's Cadence
  GEM Ethernet with their Broadcom PHY, USB Ethernet (the Pi 3's smsc95xx and lan78xx), `bcm2835_wdt`, the mini UART,
  the Cortex-A53 erratum 843419), every driver built in, and `membarrier` (the JIT context-synchronizes every thread
  after mapping new code). `os/kbuild.sh arm64` cross-compiles it with `aarch64-linux-gnu-` on other hosts and also
  builds the Pi devicetrees.
- **`i386`** (3.1 MB, best effort): old PCs. i686 (Pentium Pro instruction set, generic tuning) with PAE (NX),
  HIGHMEM up to 4 GiB, up to 8 CPUs. Built in like `metal`: ACPI, PCI, IDE/PATA and SATA of the common chipsets (Intel
  PIIX/ICH, AMD, VIA, SiS, ATI, nVidia, JMicron, Marvell, Silicon Image, Promise, ALi, ServerWorks) and AHCI, USB host
  (UHCI, OHCI, EHCI, xHCI) and storage, virtio, serial and VGA text consoles, PC keyboard. Modules (72, signed like
  `metal`'s): Ethernet (Realtek RTL8139 and r8169, Intel e100, e1000, e1000e, igb, Broadcom b44, tg3, bnx2, VIA Rhine
  and Velocity, SiS 900/190, nVidia forcedeth, 3Com 3c59x, AMD PCnet32, Marvell skge and sky2, Atheros atl1, atl1e,
  atl1c, alx, NE2000 PCI, DEC Tulip, National Semiconductor, virtio-net), the USB Ethernet and watchdog modules of
  `metal`. `i686-linux-gnu-` on non-x86 hosts.
- **`armv7`** (2.2 MB zImage, best effort): 32-bit ARM (ARMv7-A, hard-float: VFPv3, NEON), multiplatform with the
  generic virtual machine (GIC, architected timer, PSCI), UEFI (zImage EFI stub, EFI RTC, efivarfs), PCIe ECAM,
  virtio, PL011, PL031, the PL061 `gpio-keys` power key, i6300esb, HIGHMEM; every driver built in. It boots QEMU virt
  and UEFI machines with these devices; real boards need their SoC's clock, pin control and Ethernet drivers, which it
  does not have (the Raspberry Pi 3 runs the `arm64` Pi image instead; the Pi 2 is not supported).
  `arm-linux-gnueabihf-` on other hosts.

Modules are signed (RSA-4096, SHA-512) with a key that `os/kbuild.sh` creates for the build and deletes after
`modules_install`: no other module can ever be signed for that kernel, and the kernel refuses unsigned or foreign
modules (`MODULE_SIG_FORCE`, lockdown). They are zstd-compressed and decompressed by the kernel. `K10S_MODULE_KEY=PEM`
signs with a key you keep instead, which makes the kernel build reproducible.

### Firmware

`os/firmware.sh` builds the firmware archive of the `metal` images (x86-64 and i686): the 43 files that the x86-64
modules declare
(`modinfo -F firmware`: Realtek `rtl_nic/`, Broadcom `tigon/` and `bnx2/`, Intel `ice` DDP) from linux-firmware
`20261019`, each checked against its SHA-256 in `os/firmware.list`, with their licence files
(`/lib/firmware/LICENSES/`). It is a separate cpio appended to the initramfs; the files keep their own redistributable
licences, not k10s's Apache-2.0. Intel e1000e/igb/igc/ixgbe/i40e need no firmware. The i386 modules need a subset of
them (rtl_nic, tigon, bnx2).

## Secure Boot

The images are not signed for UEFI Secure Boot (docs/REPORT.md §9: k10s has no shim of its own signed by Microsoft),
so firmware with Secure Boot on refuses their systemd-boot ("Access Denied", or a security violation screen). BIOS
boots (the i686 image is BIOS-only) and the Raspberry Pi (its firmware has no Secure Boot) are not concerned; the armv7
image is meant for firmware without Secure Boot (the own-keys flow below applies to it but is untested, and Debian
ships no 32-bit ARM shim). Three ways to boot the images on UEFI, each tested under OVMF and AAVMF with Microsoft's
keys enrolled (`secureboot` in "Testing"):

- **Secure Boot off**, in the firmware setup (often Security or Boot → Secure Boot → Disabled; "OS type: Other OS" on
  some boards; some laptops allow it only once a supervisor password is set). The enrolled keys stay, so switching it
  back on restores Secure Boot for other systems.
- **Own keys**: your platform key (PK), KEK and db key enrolled next to Microsoft's; systemd-boot and every UKI signed
  with your db key.
- **A MOK** (Machine Owner Key) under Debian's shim: the shim, signed by Microsoft's UEFI CA, boots wherever
  Microsoft's third-party CA is enrolled, and trusts the keys in its MokList; systemd-boot and every UKI signed with
  your MOK. For machines whose keys you cannot or do not want to replace.

At boot init logs the firmware's state: `efi: Secure Boot on`, `off`, or `off (setup mode: no platform key)`. The
kernel is in lockdown (integrity) either way.

**Signing an image.** Signed for Secure Boot, a UKI is no longer the file the k10s release key signed, so each image
is signed twice: with your db key or MOK (Authenticode), then with an image key of yours (`k10s image sign`, the key
pinned with `os_key`). In that order: `k10s image sign` puts its signature in the UKI's certificate table, as the last
entry, of a type that firmware and shim skip (`crates/k10-mesh/src/osimage.rs`), and grows the table's size in the PE
header, neither of which the Authenticode hash covers; data after the certificate table is refused by firmware and
shim, and an Authenticode signature added after the k10s one would leave that one inside the signed data, where k10s
no longer finds it. `k10s image payload` gives the UKI of a signed image without its k10s signature. For each image
(the slot A of a new machine and every update; here the x86-64 `metal` one, the others alike):

```bash
k10s keygen -o os.key                                         # once: your image key
k10s image payload k10s-os-V-x86_64-metal.efi uki.efi         # the release's UKI without the release signature
sbctl sign uki.efi                                            # own keys; with a MOK, or without sbctl:
                                                              # sbsign --key mok.key --cert mok.crt --output uki.efi uki.efi
k10s image sign --key os.key --arch x86_64 --profile metal --version V uki.efi k10s-os-V-x86_64-metal.sb.efi
k10s image verify k10s-os-V-x86_64-metal.sb.efi --trust "$(k10s image pubkey os.key)"   # "signed for Secure Boot"
```

`sbverify --cert CERT FILE` (sbsigntool) checks the Authenticode signature of the result too (exit status 0); it also
tries to read the k10s entry as a signature and prints an error about it.

A new machine: write the image to its disk, then on its `K10S-BOOT` (mounted on `/mnt`) sign systemd-boot once
(updates never replace it), replace slot A and pin your image key:

```bash
sbctl sign /mnt/EFI/BOOT/BOOTX64.EFI                          # BOOTAA64.EFI on arm64; for a MOK see below
cp k10s-os-V-x86_64-metal.sb.efi /mnt/EFI/Linux/k10s-a.efi
echo "os_key=$(k10s image pubkey os.key)" >> /mnt/k10s.conf
```

A machine installed from an image older than U6 has systemd-boot 252, which does not reboot after a refused trial
(below): copy `EFI/BOOT/BOOTX64.EFI` from a current image before signing it.

Updates name the `.sb.efi` files in the `os` block. Init writes an image into its slot as it is, both signatures
included. An image the firmware refuses (not signed for Secure Boot, or by a key it does not trust) cannot cost more
than its trial: systemd-boot reboots after the refusal (`reboot-on-error auto` in `loader.conf`), so the 3 trial boots
end on the committed slot, which records the image as failed and stops the rollout. Whoever holds the db key or the
MOK and an image key your machines trust can make images they boot: keep those keys off the machines.

**Enrolling own keys** with [sbctl](https://github.com/Foxboron/sbctl). sbctl writes the keys from a running Linux
system while the firmware is in setup mode; k10s images have no userland, so boot a live Linux system with sbctl on
the machine (from USB), with the keys of `sbctl create-keys` in its `/var/lib/sbctl/keys`:

1. In the firmware setup, delete the Secure Boot keys to enter setup mode ("Reset to Setup Mode", "Clear Secure Boot
   keys", "Delete all Secure Boot variables"; some firmware first needs Secure Boot mode "Custom"). `sbctl status`
   then reports setup mode.
2. `sbctl enroll-keys --microsoft`: your PK, KEK and db, plus Microsoft's KEK and db certificates. Keep Microsoft's:
   the firmware drivers of graphics cards and network adapters (option ROMs) are usually signed by Microsoft's UEFI
   CA, and without it a machine may lose its screen or network before any OS runs.
3. Turn Secure Boot on and boot the k10s disk.

Some firmware setups also enroll keys from files on a USB stick instead: DER certificates (sbctl's are the `*.pem`
files under its `keys` directory: `openssl x509 -in db.pem -outform DER -out db.cer`), or the signed `.auth` files of
`sbctl enroll-keys --microsoft --export auth`.

**A MOK with Debian's shim.**

1. The shim and its MokManager come from Debian 13's packages `shim-signed` (`/usr/lib/shim/shimx64.efi.signed`,
   signed by Microsoft's UEFI CA 2011) and `shim-helpers-amd64-signed` (`/usr/lib/shim/mmx64.efi.signed`, signed by
   Debian, whose key the shim trusts); on arm64 `shimaa64.efi.signed` and `mmaa64.efi.signed` from
   `shim-helpers-arm64-signed`. Download the `.deb` files from deb.debian.org and unpack them with `dpkg-deb -x` (any
   Linux) or `ar x` and `tar`; `os/bootfiles.sh shim-x86_64` (or `shim-aarch64`) fetches the versions the tests use
   (shim 16.1), SHA-256 checked.
2. A MOK and its DER form for MokManager:
   `openssl req -new -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=k10s MOK/" -keyout mok.key -out mok.crt`,
   `openssl x509 -in mok.crt -outform DER -out mok.cer`.
3. Sign systemd-boot and the UKIs with it (`sbsign --key mok.key --cert mok.crt`; the UKIs as above), and lay out
   `K10S-BOOT` (arm64: `BOOTAA64.EFI`, `grubaa64.efi`, `mmaa64.efi`):

   ```
   /EFI/BOOT/BOOTX64.EFI   shimx64.efi.signed (the firmware starts the shim)
   /EFI/BOOT/grubx64.efi   systemd-boot signed with the MOK (the shim starts grubx64.efi from its own directory)
   /EFI/BOOT/mmx64.efi     mmx64.efi.signed (MokManager)
   /EFI/Linux/k10s-a.efi   the UKI signed with the MOK, then by k10s (above)
   /mok.cer                the MOK, for MokManager
   ```

4. Enroll the MOK at the first boot with Secure Boot on: the shim cannot verify `grubx64.efi` yet ("Verification
   failed: (0x1A) Security Violation") and starts MokManager: "Enroll key from disk", the `K10S-BOOT` volume,
   `mok.cer`, "Continue", "Yes", "Reboot". `mokutil --import mok.cer` does the same from another Linux system booted
   on the machine (it asks for a one-time password, which MokManager asks for at the next boot).

The shim then verifies systemd-boot, and systemd-boot each UKI through the shim (keys in the firmware's db count too).
Updates are signed with the MOK as above.

**On real machines.** Tested under OVMF and AAVMF only, with Debian's and Ubuntu's firmware builds whose variable
store has Microsoft's KEK CA, Windows PCA and UEFI CA 2011 (Ubuntu's also the 2023 CAs), and keys written into it
directly (`virt-fw-vars`). Not tested: real firmware setup screens (switching Secure Boot off, clearing keys,
enrolling from files), `sbctl` itself (the tests sign with `sbsign`), MokManager's screens and `mokutil`. Things to
expect:

- **Microsoft UEFI CA 2011 or 2023.** Debian's shim 16.1 is signed by the 2011 CA; since June 2026 Microsoft signs new
  shims with the "Microsoft UEFI CA 2023" only. Firmware whose db lacks the CA that signed a shim refuses it: older
  machines may know only the 2011 CA, new ones (or after an update that removed it) only the 2023 CA. Own keys do not
  depend on either.
- **Third-party CA off.** Some PCs (Secured-core) ship with Microsoft's third-party UEFI CA disabled: allow it in the
  setup ("Allow Microsoft 3rd Party UEFI CA") for the shim, or use own keys.
- **SBAT revocations.** The shim refuses components whose security generation was revoked in the `SbatLevel`
  variable (raised by newer shims and by Windows updates), and old shims are revoked through dbx updates: keep the
  shim current from Debian's `shim-signed`. systemd-boot's generation (`systemd-boot,1` in its `.sbat`) is checked the
  same way.
- **Key sizes.** sbctl's keys are RSA-4096, which some older firmware is reported to reject; RSA-2048 (the MOK above)
  is the size every implementation takes. The tests use both.

## Building

| Script | Does |
|---|---|
| `os/images.sh [--arch x86_64\|aarch64\|i686\|armv7] [OUT]` | the release images of the arch and their update files (above), `SHA256SUMS.os-ARCH`; fails when a compressed image exceeds its budget (`x86_64-vm` 5 MB, `x86_64-metal` 40 MB, `aarch64-vm` 6 MB, `aarch64-pi` 8 MB, `i686-metal` 40 MB, `armv7-vm` 6 MB). `K10S_BIN` picks the static k10s of the arch, `K10S_HOST_BIN` one the host runs to sign (default `K10S_BIN`), `K10S_IMAGE_KEY` the signing key file (default: a throwaway `target/os/image-test.key`, trusted only where pinned with `os_key`) |
| `os/kbuild.sh [vm\|metal\|arm64\|i386\|armv7] [OUT]` | a k10s kernel, rootless; fails if a fragment option does not survive `olddefconfig`; reproducible build metadata; `metal` and `i386` also install the signed module tree with its `depmod` output, `arm64` builds `Image`, `Image.gz` and the Raspberry Pi devicetrees, `armv7` a `zImage` |
| `os/mkimage.py OUT.img --kernel K --initrd I… --cmdline C --boot DIR [--arch x86_64\|aarch64\|i686\|armv7] [--pi --dtbs DIR] [--conf F] [--uki OUT] [--version V --sign KEY --k10s K10S --profile P]` | a disk image: GPT, GRUB patched into the MBR and `bios-boot` (x86-64, no `grub-install`), the UKI (systemd-stub plus sections, written by the script itself) signed with `k10s image sign` (unsigned without `--sign`: it boots but cannot update), the A/B slot layout; i686: the MBR BIOS image, GRUB's core.img before p1; `--pi`: the MBR Pi image with its signed slot image; FAT with mtools, ext4 with `mkfs.ext4`; no root, no loop devices |
| `os/bootfiles.sh [x86_64\|aarch64\|i686\|armv7\|shim-x86_64\|shim-aarch64]` | the pinned Debian packages, SHA-256 checked: GRUB (`i386-pc`, and the build host's `grub-mkimage`, amd64 or arm64) and systemd-boot (systemd-boot, systemd-stub) 262 for x86-64; GRUB and systemd-stub 262 (ia32) for i686; systemd-boot 262 for arm64 and armv7, and for arm64 the Raspberry Pi firmware (`raspi-firmware` 1.20260408); `shim-*`: Debian's shim 16.1 and MokManager ("Secure Boot"; not in the images) |
| `os/firmware.sh` | the firmware archive (above) |
| `os/debian-kernel.sh` | the pinned Debian 13 kernel (6.12, SHA-256 checked) with its module tree and `depmod` output |
| `os/mkinitramfs.py OUT [--init K10S] [--kmods DIR --mod GLOB…] [--firmware DIR --fw GLOB…] [--conf FILE]` | a reproducible newc initramfs (gzip, xz or zstd), module subsets with their dependencies, filtered `modules.alias`; without `--init`, an archive to append to another |

The kernel builds need make, gcc (or `aarch64-linux-gnu-gcc`, `i686-linux-gnu-gcc`, `arm-linux-gnueabihf-gcc` for
another architecture than the host's), flex, bison, bc, openssl,
patch, depmod and the libelf and libssl headers; the images need sfdisk, mkfs.fat, mtools, mkfs.ext4, dpkg-deb, zstd
and xz; the Secure Boot tests `sbsign` (sbsigntool), `virt-fw-vars` (virt-firmware), openssl and the firmware builds
with Microsoft's keys (`OVMF_CODE_4M.ms.fd`, `AAVMF_CODE.ms.fd` and their variable stores).

## Testing

`os/test.sh` (CI suite `os`, a release gate) builds the static k10s, the kernels, the Debian kernel, the initramfs
images and the release images, then runs `os/test.py` under QEMU (KVM when available), with no root. The release runs
it on the k10s binary of the release's musl archive and publishes the images it tested. `os/test.sh --arch aarch64`
(CI suite `os-arm64`, a release gate, on an x86-64 runner: the aarch64 guests run on QEMU's TCG) does the same for
the arm64 kernel and images with the release's aarch64 musl k10s, and `os/test.sh --arch i686` and `--arch armv7` (CI
suite `os-legacy`, a release gate, on an x86-64 runner: i686 guests with KVM, armv7 ones on TCG) for the best-effort
kernels and images with the release's i686 and armv7 musl k10s.

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
- **boot-arm64**: as boot-k10s with the arm64 kernel on QEMU `virt` (devicetree: PL031 RTC from 2001, the
  `gpio-keys` power key, i6300esb).
- **image-arm64**, **update-arm64**: as image-uefi and update-uefi with the `aarch64-vm` image on AAVMF (ACPI: the
  tiny power button, the EFI RTC), including the power cut at every step, the panicking image and the image that
  never commits.
- **pi**: the Pi image on QEMU `virt` behind a model of the Pi firmware (`PiNode` in `os/test.py`): each boot reads
  `autoboot.txt` from partition 1 and boots the `[all]` partition, or the `[tryboot]` one once after a boot that ended
  in `reboot "0 tryboot"` (the kernel logs the command); it loads `kernel8.img`, `initramfs8` and `cmdline.txt` from
  that FAT partition and passes the partition and the flag in `/chosen/bootloader` of the devicetree. Init finds
  `K10S-BOOT` and `K10S-DATA` by label on the MBR disk and grows `K10S-DATA`; then the update test of update-uefi:
  the power cut at every step, an image that panics (tried 3 times, then recorded), one that never commits.
- **pi3**: the same on QEMU `raspi3b`, a Raspberry Pi 3 (BCM2837, 1 GiB, Cortex-A53): the devicetree is the slot's
  `bcm2710-rpi-3-b.dtb` (with `/chosen/bootloader` added), the SD card is on SDHOST, and QEMU's USB CDC NIC stands in
  for the LAN9514, appearing on the dwc2 bus after boot. With no RTC the clock comes from the host-side SNTP server
  before the signed applies, and with no power button every run ends in a power cut (the KV state survives each).
  QEMU's model resets the machine as soon as the watchdog is armed (it models the reset register, not the countdown),
  so this test runs without `watchdog=1`.
- **mixed**: a Pi (as above) joins a cluster whose first member is an x86-64 k10s on the host (the x86-64 musl binary);
  both see each other's OS report and serve the example services placed on either node, each on its JIT (the Pi's
  AArch64 code under QEMU's TCG).
- **boot-i686**, **boot-armv7**: as boot-k10s with the i386 kernel (signed virtio-net and i6300esb modules) on QEMU `pc`
  with an Atom N270 (32-bit only, SSE2, PAE, NX), and with the armv7 kernel on QEMU `virt` without highmem (a
  Cortex-A15; devicetree: PL031 RTC from 2001, the `gpio-keys` power key, i6300esb).
- **image-i686**: as image-bios with the `i686-metal` image (its console on serial for the test) on SeaBIOS, an N270,
  an IDE disk and an RTL8139 NIC (`8139too`, a signed module): GRUB from the MBR, `K10S-DATA` grown on the MBR disk,
  apply, serve, state kept.
- **image-armv7**: as image-uefi with the `armv7-vm` image on AAVMF32 (UEFI, a devicetree, the EFI RTC).
- **update-i686**, **update-armv7**: as update-bios (GRUB only: the image has no systemd-boot) on IDE and an e1000, and
  as update-uefi: the power cut at every step, the panicking image and the one that never commits.
- **secureboot** (x86-64, OVMF on `q35` with SMM) and **secureboot-arm64** (AAVMF): the `vm` image under the firmware
  build with Secure Boot on and Microsoft's keys enrolled, each case on a fresh disk and a copy of its variable store
  (changed with `virt-fw-vars`, signed with `sbsign`, as "Secure Boot" describes). As released, the firmware refuses
  it ("Access Denied"). Secure Boot switched off (the setup's `SecureBootEnable`, keys kept): it serves. MOK: Debian's
  Microsoft-signed shim as `BOOTX64.EFI`, systemd-boot and the UKI signed with a MOK that is in shim's MokList: it
  serves with init logging `Secure Boot on`. Own keys (RSA-4096 PK, KEK and db enrolled next to Microsoft's,
  systemd-boot and the UKI signed with the db key): it serves with Secure Boot on; as a single-node cluster it then
  installs an update signed the same way, which commits, and one signed only by k10s, which the firmware refuses on
  each of its 3 trial boots (systemd-boot reboots after each) before the committed slot comes back, records it as
  failed and the rollout stops.

`scripts/ci-test.sh arm64` (CI and release gate, on a GitHub arm64 runner) runs every crate's unit and integration
tests natively on arm64, for the gnu and musl targets: the AArch64 JIT's suites, then the node and CLI suites on the
JIT and on the interpreter. `scripts/ci-test.sh i686` (on an x86-64 runner) does the same for
i686-unknown-linux-musl, natively and without a JIT; `scripts/ci-test.sh armv7` (on the arm64 runner) for
armv7-unknown-linux-musleabihf, natively when the CPU runs AArch32 code, else the in-process suites under qemu-arm
without the seccomp tests (qemu-user has no seccomp; the armv7 node runs under its filter in `os-legacy`) and the
timing bounds.

Unit tests cover the configuration precedence, module tree parsing and modalias matching, the DHCP state machine
against a fake server, SNTP sample checks and the GPT rewrite that grows `K10S-DATA` (512-byte and 4 KiB sectors,
checksums, partitions that are not last, corrupt tables) and its MBR variant. The slot logic is tested against models
of systemd-boot's entry selection (262, and the 252 of machines installed from earlier images), of `grub.cfg` and of
the Pi firmware's `autoboot.txt` and tryboot flag, with the power cut after every durable step of an install and of a
commit on every firmware type, updates alternating between the slots, and images that never come up or never commit;
the image format (also with an Authenticode certificate table: both signatures intact, the payload restored), Ed25519
(RFC 8032 vectors), the manifest's OS plan, the gossip extension (never sent to nodes that predate it) and the
rollout waves have their own tests in `k10-mesh`.

Real hardware is needed for: a real machine serving after a cold boot from the `metal` image (the U1 exit criterion
for a Realtek or Intel mini-PC), power cuts that reach the disk's own write cache (QEMU's kill loses only what the
guest had not written), GRUB's `save_env` and systemd-boot's renames on real firmware and disks, NIC drivers and firmware outside QEMU's models (r8169/RTL8125, igc, tg3, USB
Ethernet), AHCI and eMMC boot disks, the screen console, firmware quirks of real BIOS and UEFI implementations
(boot drive numbers, removable-media boot paths), slow USB storage (`bootwait`), the AMD and newer Intel (SMBus)
watchdogs, RTC chips, real DHCP servers, switches (link and STP delays) and NTP pools. For the Pi (the U3 exit
criteria: a Pi 4 and a Pi 5 join an x86 cluster, survive power cuts and update A/B): the real firmware reading
`autoboot.txt` and honouring the tryboot flag set by the patched kernel through the mailbox, the mainline devicetrees
under the Pi firmware, the Pi 4's GENET and the Pi 5's RP1 Ethernet, SD/USB/NVMe boot, `bcm2835_wdt` resets, the
serial consoles, and power cuts on SD cards (whose controllers may lose more than the last write). For the U5 exit
criterion (it boots and serves on a Pi 3 B+ and an old PC): on the Pi 3 B+, its `bootcode.bin` reading `autoboot.txt`
and switching to the `[tryboot]` partition (the documentation does not list the models; if it did not, a trial would
boot the committed slot, init would retry it 3 times and record the image as failed: no update, no outage), the
tryboot flag set through the mailbox, the bcm2710 devicetrees under the firmware, the LAN7515 (lan78xx) and LAN9514
(smsc95xx) behind dwc2, SDHOST on a real card, `bcm2835_wdt` resets; on an old PC, its BIOS booting GRUB from the MBR
of a USB or IDE/SATA disk, the IDE/SATA controller and NIC drivers (and the Realtek firmware) on the real chips, PAE
and the CPU's speed with the interpreter, and boards with 1-4 GiB (HIGHMEM). For Secure Boot
(the U6 exit criterion, both documented flows booting under OVMF with Microsoft's keys, holds in QEMU): real firmware
setup screens (Secure Boot off, setup mode, enrolling keys from files), `sbctl enroll-keys` and `mokutil` on a
running system, MokManager's screens, machines that ship the 2023 Microsoft CAs only or the third-party CA disabled,
and firmware that rejects RSA-4096 keys ("Secure Boot").

## Limits

- Updates need cluster mode and an image booted through its own loaders; nodes are not grouped into zones yet.
- A rollback is an update to the older image (signed images of any version install); the cluster key is the trust
  boundary for which signed image a cluster runs.
- The images are not signed for Secure Boot: turn it off, or sign them with your own keys or a MOK ("Secure Boot").
- The image uses 512-byte logical sectors; disks with 4 KiB logical sectors (rare outside servers) need a rebuilt
  image.
- The arm64 image has the virtio drivers of VMs and the Pi's; arm64 servers need their NIC drivers added. The Pi
  image has no screen console (serial only).
- i686 and armv7 are best effort and interpreter only (no 32-bit JIT): instance memory is at most 256 MiB (slots of
  2^28 bytes in a 3-4 GiB address space: a 32-bit node refuses a desired state that places a larger service on it, so
  a cluster with 32-bit members keeps its services at that size), fiber stacks are 256 KiB, and the
  speculation-safe clamps of the host functions are plain code (`k10-nospec`: guaranteed on x86-64 and arm64 only).
  The i686 image boots on BIOS only (no 32-bit UEFI); the armv7 image has no drivers for real boards' SoCs.
- No DHCP address conflict detection (ARP probe), no DHCPv6; one configured interface.
- `join` host names are resolved once, when the node starts (name servers from the lease or `dns`).
