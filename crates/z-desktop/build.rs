fn main() {
    #[cfg(target_os = "windows")]
    {
        // GPUI + orchard proofs + sqlite debuginfo overflows MSVC's PDB
        // module/type cap (LNK1140). This binary still runs; backtraces
        // just will not have a companion .pdb.
        println!("cargo:rustc-link-arg-bins=/PDB:NONE");
        // GPUI layout/text recurse past the 1 MiB MSVC default stack.
        println!("cargo:rustc-link-arg-bins=/stack:8388608");
    }
}
