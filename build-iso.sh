#!/bin/bash
set -e
cd ~/nexus-os/kernel

# Assemble boot
nasm -f elf64 ../boot/boot.asm -o ../boot/boot.o

# Build kernel
cargo +nightly-2026-04-01 build --release --target x86_64-unknown-none 2>&1 | tail -3

# Create ISO
rm -rf /tmp/nexus-iso
mkdir -p /tmp/nexus-iso/boot/limine /tmp/nexus-iso/EFI/BOOT
cp target/x86_64-unknown-none/release/nexus-kernel /tmp/nexus-iso/boot/
cp ~/limine/limine-bios-cd.bin /tmp/nexus-iso/boot/limine/
cp ~/limine/limine-bios.sys /tmp/nexus-iso/boot/limine/
cp ~/limine/limine-uefi-cd.bin /tmp/nexus-iso/boot/limine/
cp ~/limine/BOOTX64.EFI /tmp/nexus-iso/EFI/BOOT/

cat > /tmp/nexus-iso/boot/limine/limine.cfg << 'LIMEOF'
DEFAULT_ENTRY=0
TIMEOUT=3

:Noxisa
    PROTOCOL=limine
    KERNEL_PATH=boot:///boot/nexus-kernel
LIMEOF

xorriso -as mkisofs \
  -b boot/limine/limine-bios-cd.bin \
  -no-emul-boot -boot-load-size 4 -boot-info-table \
  --efi-boot boot/limine/limine-uefi-cd.bin \
  -efi-boot-part --efi-boot-image --protective-msdos-label \
  /tmp/nexus-iso -o ~/nexus-os/nexus.iso 2>/dev/null

~/limine/limine bios-install ~/nexus-os/nexus.iso 2>/dev/null
echo "ISO built: $(ls -lh ~/nexus-os/nexus.iso)"
