#!/usr/bin/env bash
# Run the KMS capture end-to-end test (`--source kms`) as root in a
# throwaway KVM guest, against `vkms` (a virtual display controller), so it
# never reads a real display:
#
#   cargo test --no-run --bin teleport        # prints the test binary
#   tools/kms-vm-test.sh target/debug/deps/teleport-<hash>
#
# Run it from `nix develop` (the guest gets the shell's GStreamer plugin
# path). The guest boots this NixOS machine's kernel, shares the host's
# /nix/store and the binary's directory read-only over 9p, loads vkms, shows
# modetest's test pattern on it and runs kms::tests::captures_a_real_framebuffer.
# Needs /dev/kvm and qemu.
set -euo pipefail
binary=$(readlink -f "$1")
cd "$(dirname "$0")/.."
: "${GST_PLUGIN_SYSTEM_PATH_1_0:?run inside nix develop}"
kernel=$(readlink -f /run/current-system/kernel)
modules=$(readlink -f /run/current-system/kernel-modules)
release=$(uname -r)
kmod=$(readlink -f "$(command -v modprobe)")
busybox=$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#pkgsStatic.busybox)/bin/busybox
modetest=$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#libdrm.bin)/bin/modetest
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
/tmp/modprobe -d $modules -a vkms
sleep 1
ls -l /dev/dri >>/out/result.txt 2>&1
# The test pattern stays on screen while modetest waits for input.
mkfifo /tmp/hold
$modetest -M vkms -s Virtual-1:1024x768 </tmp/hold >/out/modetest.txt 2>&1 &
modetest=\$!
exec 3>/tmp/hold
sleep 2
cd /tmp
status=0
HOME=/tmp GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0 TELEPORT_TEST_KMS=/dev/dri/card0 \
  /test/$(basename "$binary") --ignored --exact kms::tests::captures_a_real_framebuffer \
  --nocapture --test-threads=1 >>/out/result.txt 2>&1 || status=1
# Control: with modetest gone the display shows nothing, and the capture
# must fail rather than report a pass.
exec 3>&-
kill \$modetest 2>/dev/null
sleep 1
HOME=/tmp GST_PLUGIN_SYSTEM_PATH_1_0=$GST_PLUGIN_SYSTEM_PATH_1_0 TELEPORT_TEST_KMS=/dev/dri/card0 \
  /test/$(basename "$binary") --ignored --exact kms::tests::captures_a_real_framebuffer \
  --nocapture --test-threads=1 >/out/control.txt 2>&1 && status=1
echo "EXIT \$status" >>/out/result.txt
sync
poweroff -f
EOF
chmod +x "$root/init"
(cd "$root" && find . | cpio -o -H newc --quiet) >"$work/initramfs.cpio"
timeout 300 qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 \
  -kernel "$kernel" -initrd "$work/initramfs.cpio" \
  -append "console=ttyS0 panic=-1 quiet" -nographic -no-reboot \
  -virtfs local,path=/nix/store,mount_tag=store,security_model=none,readonly=on \
  -virtfs "local,path=$(dirname "$binary"),mount_tag=test,security_model=none,readonly=on" \
  -virtfs "local,path=$work/out,mount_tag=out,security_model=none" >"$work/console.txt" 2>&1 ||
  { cat "$work/console.txt"; exit 1; }
echo "--- modetest"; cat "$work/out/modetest.txt"
echo "--- capture"; cat "$work/out/result.txt"
echo "--- control (nothing on the display; must fail)"
grep -E "^Error|panicked|test result" "$work/out/control.txt"
grep -q '^EXIT 0$' "$work/out/result.txt"
