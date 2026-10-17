#!/usr/bin/env bash
# Run the /dev/uhid PlayStation pad tests as root in a throwaway KVM guest
# that boots this NixOS machine's kernel, so they need no root or udev rule
# on the host:
#
#   cargo test --no-run --bin teleport        # prints the test binary
#   tools/uhid-vm-test.sh target/debug/deps/teleport-<hash>
#
# The guest shares the host's /nix/store and the binary's directory
# read-only over 9p, loads uhid and hid-playstation, and runs
# gamepad::tests::uhid_playstation_pads_bind_hid_playstation and
# gamepad::tests::sdl_reads_uhid_pads_as_playstation_controllers (SDL
# without udev, as the guest has none). Needs /dev/kvm and qemu.
set -euo pipefail
binary=$(readlink -f "$1")
shift
tests=("$@")
if [ ${#tests[@]} -eq 0 ]; then
  tests=(gamepad::tests::uhid_playstation_pads_bind_hid_playstation
    gamepad::tests::sdl_reads_uhid_pads_as_playstation_controllers)
fi
cd "$(dirname "$0")/.."
kernel=$(readlink -f /run/current-system/kernel)
modules=$(readlink -f /run/current-system/kernel-modules)
release=$(uname -r)
kmod=$(readlink -f "$(command -v modprobe)")
busybox=$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#pkgsStatic.busybox)/bin/busybox
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/initramfs
mkdir -p "$root"/{bin,lib,proc,sys,dev,tmp,nix/store,test,out} "$work/out"
cp "$busybox" "$root/bin/busybox"
early=(virtio_ring virtio virtio_pci_modern_dev virtio_pci_legacy_dev virtio_pci netfs 9pnet 9pnet_virtio 9p)
for module in "${early[@]}"; do
  file=$(modinfo -b "$modules" -k "$release" -n "$module")
  case "$file" in
    *.xz) xz -dc "$file" ;;
    *.zst) zstd -dc "$file" ;;
    *) cat "$file" ;;
  esac >"$root/lib/$module.ko"
done
cat >"$root/init" <<EOF
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp
for module in ${early[*]}; do insmod /lib/\$module.ko; done
mount -t 9p -o trans=virtio,version=9p2000.L,ro store /nix/store
mount -t 9p -o trans=virtio,version=9p2000.L,ro test /test
mount -t 9p -o trans=virtio,version=9p2000.L out /out
ln -s $kmod /tmp/modprobe
/tmp/modprobe -d $modules -a uhid hid_playstation hid_generic evdev
cd /tmp
status=0
for test in ${tests[*]}; do
  HOME=/tmp SDL_JOYSTICK_DISABLE_UDEV=1 SDL_HIDAPI_UDEV=0 /test/$(basename "$binary") \
    --ignored --exact \$test --nocapture --test-threads=1 >>/out/result.txt 2>&1 || status=1
done
echo "EXIT \$status" >>/out/result.txt
dmesg | grep -i -E 'playstation|uhid' >/out/dmesg.txt
sync
poweroff -f
EOF
chmod +x "$root/init"
(cd "$root" && find . | cpio -o -H newc --quiet) >"$work/initramfs.cpio"
timeout 300 qemu-system-x86_64 -enable-kvm -cpu host -m 1536 -smp 2 \
  -kernel "$kernel" -initrd "$work/initramfs.cpio" \
  -append "console=ttyS0 panic=-1 quiet" -nographic -no-reboot \
  -virtfs local,path=/nix/store,mount_tag=store,security_model=none,readonly=on \
  -virtfs "local,path=$(dirname "$binary"),mount_tag=test,security_model=none,readonly=on" \
  -virtfs "local,path=$work/out,mount_tag=out,security_model=none" >"$work/console.txt" 2>&1 ||
  { cat "$work/console.txt"; exit 1; }
cat "$work/out/dmesg.txt" "$work/out/result.txt"
grep -q '^EXIT 0$' "$work/out/result.txt"
