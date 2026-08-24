fn main() {
    // The hybrid candidate links the integer/LUT + K transform into the Rust
    // process. clang-cl keeps the native code ABI-compatible with Rust's MSVC target.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .compiler("clang-cl")
        .file("native_transform.cc")
        .flag("/O2")
        .flag("/EHsc")
        .flag("/std:c++17")
        .flag("/clang:-O3")
        .flag("/clang:-march=native")
        .warnings(true)
        .compile("native_transform");
    println!("cargo:rerun-if-changed=native_transform.cc");

    // Zig and Nim emit freestanding COFF objects through their explicit build
    // scripts. Link them only for the polyglot feature build so a normal Rust/C++
    // build remains self-contained and does not launch Docker implicitly.
    for (feature, relative) in [
        ("CARGO_FEATURE_ZIG", "../zig/bin/transform.obj"),
        ("CARGO_FEATURE_NIM", "../nim/bin/transform.obj"),
        ("CARGO_FEATURE_NIM", "../nim/bin/runtime_stubs.obj"),
    ] {
        if std::env::var_os(feature).is_some() {
            let path = std::fs::canonicalize(relative)
                .unwrap_or_else(|_| panic!("missing {relative}; run its build.ps1 first"));
            println!("cargo:rustc-link-arg={}", path.display());
            println!("cargo:rerun-if-changed={relative}");
        }
    }
}
