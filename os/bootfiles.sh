#!/usr/bin/env bash
# Fetches the pinned boot files of the disk images (os/mkimage.py) and prints DIR, the packages extracted:
#   os/bootfiles.sh [x86_64]   DIR/usr/lib/grub/i386-pc (GRUB modules, boot.img), DIR/usr/bin/grub-mkimage (BIOS),
#                              DIR/usr/lib/systemd/boot/efi/{systemd-bootx64.efi,linuxx64.efi.stub} (UEFI loader, UKI stub)
#   os/bootfiles.sh i686       GRUB as above (BIOS) and DIR/usr/lib/systemd/boot/efi/linuxia32.efi.stub (the UKI stub
#                              of the slot images)
#   os/bootfiles.sh aarch64    DIR/usr/lib/systemd/boot/efi/{systemd-bootaa64.efi,linuxaa64.efi.stub} and the Raspberry
#                              Pi firmware DIR/usr/lib/raspi-firmware (bootcode.bin, start_cd.elf, fixup_cd.dat,
#                              start4cd.elf, fixup4cd.dat; licence in DIR/usr/share/doc/raspi-firmware/copyright)
#   os/bootfiles.sh armv7      DIR/usr/lib/systemd/boot/efi/{systemd-bootarm.efi,linuxarm.efi.stub}
#   os/bootfiles.sh shim-x86_64|shim-aarch64
#                              Debian's shim signed by Microsoft and its MokManager signed by Debian (Secure Boot with a
#                              MOK, docs/OS.md; not in the images): DIR/usr/lib/shim/{shimx64,mmx64}.efi.signed, or
#                              {shimaa64,mmaa64}.efi.signed
# Debian packages, each checked against its SHA-256 (deb.debian.org, else snapshot.debian.org). grub-mkimage is the
# build host's (amd64 or arm64); the i386-pc modules are the same for both.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cache=${K10S_OS_CACHE:-$here/../target/os}
# name  sha256  snapshot sha1  pool path
GRUB_PC="grub-pc-bin_2.06-13+deb12u2_amd64.deb db6aced5a89f02e10e150140b4df9621196f311c26203baf8558b4c6808e1298 60659aaa4bba685bf505a6baea2e775d2c9436ba main/g/grub2"
grub_common() {
    case $(uname -m) in
    x86_64) echo "grub-common_2.06-13+deb12u2_amd64.deb fa2a91e48bb85effb1d9e22e16d83774e6de0ae6167268e8150e76ae963ec671 8bab2583c19be89ffec50d3622acb4a7d00cdb05 main/g/grub2" ;;
    aarch64) echo "grub-common_2.06-13+deb12u2_arm64.deb d6eb6faf42b158f34e5add960e2083ca847237dbaea378a5f4e365ec2c305aa9 364fc4751f8b963b2422aa04bf25e320737cda29 main/g/grub2" ;;
    *)
        echo "bootfiles: no pinned grub-mkimage for a $(uname -m) host" >&2
        exit 2
        ;;
    esac
}
case ${1:-x86_64} in
shim-x86_64)
    PKGS=(
        "shim-signed_1.51~1+deb13u1+16.1-2~deb13u1_amd64.deb 3c802fa303c0e6bf126adee74028d1042e3120360d159c2c05181dc9b2f61005 622376cb844d11bbb2758011f48c6572564b6375 main/s/shim-signed"
        "shim-helpers-amd64-signed_1+16.1+2~deb13u1_amd64.deb 7e950d75d5a40ecc0a99f727452ce35780b47a04d9a2cc3b1ff7c2bb316c033e 5f224eaa14e6a4f3f6658e98f20349225abebabf main/s/shim-helpers-amd64-signed"
    )
    ;;
shim-aarch64)
    PKGS=(
        "shim-signed_1.51~1+deb13u1+16.1-2~deb13u1_arm64.deb e228a68b298865e0f1b35f93dac8fc9644a47bde3487419efe305777e2d4a1f0 c3bdd94eeaadfcca045504f6fe73771f2c51829b main/s/shim-signed"
        "shim-helpers-arm64-signed_1+16.1+2~deb13u1_arm64.deb 0563a90af1f9990a592a79f52845747daff51c1d345769adc07830d2547da325 dfc6425ebeb3ecc54f04caf00b43cd1222709eba main/s/shim-helpers-arm64-signed"
    )
    ;;
x86_64)
    PKGS=(
        "$GRUB_PC"
        "$(grub_common)"
        "systemd-boot-efi_262.1-1_amd64.deb e9c1784c2bfec533c1e9666f9a52e2184abff77dcdc95d99cbf7633775b01235 4d4cd3efed72db289934b5de35e2bcc7b9c13e27 main/s/systemd"
    )
    ;;
i686)
    PKGS=(
        "$GRUB_PC"
        "$(grub_common)"
        "systemd-boot-efi_262.1-1_i386.deb 3b711509f96efc06802accc60efbab937f5a386507c09d51ea240b6e9997c153 b10aabd2c1e9d3abcf490105c49971068197b293 main/s/systemd"
    )
    ;;
aarch64)
    PKGS=(
        "systemd-boot-efi_262.1-1_arm64.deb 42cc452616fdf85018a4bd0b5d3cc441e19a0b0613c06747f2073e0ead82cb1a e49b6815208ead553c71eb726c68754e72c8c6ed main/s/systemd"
        "raspi-firmware_1.20260408+ds-1_all.deb 9e215abb588e9adadf69a67107736fc2fa108ef3f17fa93589919c5e66ffe209 c0af61df71c0b3d1d7c411dfe24c5f7355e4db3e non-free-firmware/r/raspi-firmware"
    )
    ;;
armv7)
    PKGS=(
        "systemd-boot-efi_262.1-1_armhf.deb 06503cf3189627fd00121b6c90035b4612fd7b669f266973b8bec695fb904ea2 2d7221c877f1de362cd77ecc1f4ed71ec7ed47f9 main/s/systemd"
    )
    ;;
*)
    echo "bootfiles: unknown arch $1" >&2
    exit 2
    ;;
esac
stamp=$(printf '%s\n' "${PKGS[@]}" | sha256sum | cut -c1-16)
dir=$cache/boot-$stamp
if [[ ! -f $dir/.done ]]; then
    mkdir -p "$cache"
    rm -rf "$dir" && mkdir -p "$dir"
    for p in "${PKGS[@]}"; do
        read -r name sha256 sha1 pool <<<"$p"
        deb=$cache/$name
        if [[ ! -f $deb ]]; then
            for u in "https://deb.debian.org/debian/pool/$pool/$name" "https://snapshot.debian.org/file/$sha1"; do
                curl -fsSL --retry 3 -o "$deb.part" "$u" && break
            done
            mv "$deb.part" "$deb"
        fi
        echo "$sha256  $deb" | sha256sum -c --quiet
        dpkg-deb -x "$deb" "$dir"
    done
    touch "$dir/.done"
fi
echo "$dir"
