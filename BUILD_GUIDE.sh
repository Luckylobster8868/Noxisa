# Noxisa — Build & Boot Guide (Windows 11)
# ============================================================
# Prerequisites: Windows 11, 24 GB RAM, 256 GB storage
# Time to first boot: ~45 minutes
# ============================================================

## STEP 1 — Install WSL2 (Windows Subsystem for Linux)
# Open PowerShell as Administrator and run:

wsl --install
# Reboot when prompted.
# After reboot, Ubuntu opens — create a username + password.

## STEP 2 — Inside WSL2: Install all build tools

sudo apt update && sudo apt upgrade -y

# Rust toolchain (nightly required for no_std kernel)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add x86_64-unknown-none --toolchain nightly

# Build tools
sudo apt install -y \
    gcc \
    make \
    nasm \
    qemu-system-x86 \
    ovmf \
    git \
    curl \
    build-essential \
    xorriso \
    mtools

# Go (for pkg_manager)
wget https://go.dev/dl/go1.22.0.linux-amd64.tar.gz
sudo tar -C /usr/local -xzf go1.22.0.linux-amd64.tar.gz
echo 'export PATH=$PATH:/usr/local/go/bin' >> ~/.bashrc
source ~/.bashrc

## STEP 3 — Copy your project into WSL2

# In Windows Explorer, open: \\wsl$\Ubuntu\home\<yourname>\
# Drag the nexus-os-fixed folder into it.
# OR from WSL terminal:
cp -r /mnt/c/Users/laksh/Downloads/nexus-os-fixed ~/nexus-os

## STEP 4 — Build the kernel

cd ~/nexus-os/kernel

# First build — downloads dependencies, compiles everything
cargo +nightly build 2>&1 | tee build.log

# Expected output (last lines):
#   Compiling nexus-kernel v0.1.0
#   Finished dev profile [unoptimized + debuginfo]
#   target/x86_64-nexus/debug/nexus-kernel  ← your kernel ELF

# If build fails, check:
cat build.log | grep "^error"

## STEP 5 — Test boot in QEMU (minimal mode — no bootloader needed)

# This uses QEMU's -kernel flag which loads ELF directly.
# Our kernel detects null BootInfo and prints via serial, then halts.

qemu-system-x86_64 \
    -kernel target/x86_64-nexus/debug/nexus-kernel \
    -serial stdio \
    -display none \
    -m 512M \
    -no-reboot

# Expected output:
#   Noxisa v0.1 booting...
#   [boot] IDT ready
#   [boot] WARNING: no BootInfo pointer (QEMU direct boot)
#   [boot] Running in minimal mode — serial OK, halting.
#   Noxisa minimal boot successful! Build with Limine for full boot.

# If you see these lines: YOUR KERNEL IS WORKING.

## STEP 6 — Full UEFI boot with Limine bootloader

# Install Limine (handles UEFI, page tables, gives kernel valid BootInfo)
git clone https://github.com/limine-bootloader/limine.git --branch=v7.x-binary --depth=1
cd limine && make && cd ..

# Create bootable ISO
mkdir -p iso_root/EFI/BOOT
mkdir -p iso_root/boot

# Copy kernel
cp ~/nexus-os/kernel/target/x86_64-nexus/release/nexus-kernel iso_root/boot/

# Limine config
cat > iso_root/boot/limine.cfg << 'EOF'
TIMEOUT=0
VERBOSE=yes

:Noxisa
    PROTOCOL=limine
    KERNEL_PATH=boot:///boot/nexus-kernel
    KASLR=no
EOF

# Copy Limine UEFI files
cp limine/BOOTX64.EFI iso_root/EFI/BOOT/
cp limine/limine-uefi-cd.bin iso_root/boot/

# Build ISO
xorriso -as mkisofs \
    -b boot/limine-uefi-cd.bin \
    -no-emul-boot \
    -boot-load-size 4 \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part \
    --efi-boot-image \
    --protective-msdos-label \
    iso_root -o nexus-os.iso

# Boot the ISO in QEMU with UEFI firmware
qemu-system-x86_64 \
    -cdrom nexus-os.iso \
    -bios /usr/share/ovmf/OVMF.fd \
    -serial stdio \
    -m 2G \
    -smp 4 \
    -enable-kvm \
    -no-reboot

# Expected output (full boot with real BootInfo):
#   Noxisa v0.1 booting...
#   [boot] IDT ready
#   [boot] PMM: XXXX MiB free
#   [boot] Paging ready (HHDM @ 0xffff800000000000)
#   [boot] Heap ready
#   [boot] Hardware ready
#   [boot] All 6 layers active (HW+ASLR+Caps+NS+Seccomp+MAC)
#   [boot] Services ready
#   [boot] Entering scheduler

## STEP 7 — Build other components

# AI engine (runs on host for now, will be userspace daemon)
cd ~/nexus-os && cargo build -p ai_engine

# Shell
cargo build -p nexus-shell

# Compositor
cargo build -p nexus-compositor

# Package manager
cd pkg_manager && go build -o nexpkg . && cd ..

## STEP 8 — Build release kernel (optimised, smaller)

cd ~/nexus-os/kernel
cargo +nightly build --release

# Release kernel is in:
# target/x86_64-nexus/release/nexus-kernel (~500 KB stripped)

## STEP 9 — Run QEMU from Windows directly (optional)
# Download QEMU for Windows: https://www.qemu.org/download/#windows
# Then from WSL you can copy the ISO to Windows and run it natively.
# But WSL2 QEMU with -enable-kvm is faster for development.

## COMMON ERRORS AND FIXES

# Error: "error[E0463]: can't find crate for `core`"
# Fix: rustup component add rust-src --toolchain nightly

# Error: "OVMF.fd not found"
# Fix: sudo apt install ovmf
#      ls /usr/share/ovmf/  ← find the correct path

# Error: "could not compile `cc`" in build.rs
# Fix: sudo apt install gcc build-essential

# Error: "linker `rust-lld` not found"
# Fix: rustup component add llvm-tools-preview --toolchain nightly

# Error: kernel panics immediately in QEMU
# Check: qemu output with -d int,cpu_reset to see which interrupt fires
# qemu-system-x86_64 -kernel nexus-kernel -serial stdio -d int,cpu_reset -no-reboot

## WHAT EACH COMMAND DOES (for reference)

# cargo +nightly build
#   Uses nightly Rust (required for abi_x86_interrupt, naked_functions)
#   Reads kernel/Cargo.toml, runs build.rs (assembles boot.s),
#   compiles all .rs files, links with our linker.ld

# qemu-system-x86_64 -kernel <elf>
#   QEMU loads ELF directly, bypassing UEFI
#   Puts CPU in 32-bit protected mode, jumps to Multiboot2 _start
#   Our boot.s switches to 64-bit, calls kernel_main(NULL)

# -enable-kvm
#   Uses hardware virtualisation (Intel VT-x / AMD-V)
#   10-50x faster than software emulation
#   Requires: egrep -c '(vmx|svm)' /proc/cpuinfo > 0
