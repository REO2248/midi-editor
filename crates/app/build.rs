fn main() {
    // gpui renders views inside the layout pass, so a deep element tree is
    // constructed on top of the full request_layout chain. Menu construction
    // peaks just over MSVC's default 1 MiB main-thread stack in debug builds;
    // give the binary headroom (link arg is per-binary, tests are unaffected).
    #[cfg(target_env = "msvc")]
    println!("cargo:rustc-link-arg-bins=/STACK:8388608");
}
