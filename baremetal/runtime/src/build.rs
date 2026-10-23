// Board linker scripts `INCLUDE runtime-sections.ld`; the search path
// propagates to every binary that links this crate.
fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={root}/link");
    println!("cargo:rerun-if-changed=link/runtime-sections.ld");
    println!("cargo:rerun-if-changed=src/arch/aarch64/entry.S");
}

