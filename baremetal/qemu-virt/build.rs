fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=T{root}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.S");
}
