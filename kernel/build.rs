fn main() {
    println!("cargo:rerun-if-changed=src/linker.ld");
    println!("cargo:rerun-if-changed=../boot/boot.asm");
    println!("cargo:rustc-link-arg=-Tsrc/linker.ld");
    println!("cargo:rustc-link-arg=../boot/boot.o");
}
