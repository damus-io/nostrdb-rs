// build.rs
use cc::Build;
use std::env;
use std::path::PathBuf;

fn secp256k1_build() {
    // Actual build
    let mut base_config = cc::Build::new();
    base_config
        .include("nostrdb/deps/secp256k1/")
        .include("nostrdb/deps/secp256k1/include")
        .include("nostrdb/deps/secp256k1/src")
        .flag_if_supported("-Wno-unused-function") // some ecmult stuff is defined but not used upstream
        .flag_if_supported("-Wno-unused-parameter") // patching out printf causes this warning
        //.define("SECP256K1_API", Some(""))
        .define("ENABLE_MODULE_ECDH", Some("1"))
        .define("ENABLE_MODULE_SCHNORRSIG", Some("1"))
        .define("ENABLE_MODULE_EXTRAKEYS", Some("1"));
    //.define("ENABLE_MODULE_ELLSWIFT", Some("1"))

    // WASM headers and size/align defines.
    if env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "wasm32" {
        base_config.include("wasm/wasm-sysroot").file("wasm/wasm.c");
    }

    // secp256k1
    base_config
        .file("nostrdb/deps/secp256k1/contrib/lax_der_parsing.c")
        .file("nostrdb/deps/secp256k1/src/precomputed_ecmult_gen.c")
        .file("nostrdb/deps/secp256k1/src/precomputed_ecmult.c")
        .file("nostrdb/deps/secp256k1/src/secp256k1.c");

    if env::var("PROFILE").unwrap() == "debug" {
        base_config.flag("-O1");
    }

    if base_config.try_compile("libsecp256k1.a").is_err() {
        // Some embedded platforms may not have, eg, string.h available, so if the build fails
        // simply try again with the wasm sysroot (but without the wasm type sizes) in the hopes
        // that it works.
        base_config.include("wasm/wasm-sysroot");
        base_config.compile("libsecp256k1.a");
    }
}

fn main() {
    // Compile the C file
    let mut build = Build::new();

    build
        .files([
            "nostrdb/src/nostrdb.c",
            "nostrdb/src/sha256.c",
            "nostrdb/src/bolt11/bolt11.c",
            "nostrdb/src/bolt11/amount.c",
            "nostrdb/src/bolt11/bech32.c",
            "nostrdb/src/bolt11/hash_u5.c",
            "nostrdb/src/bolt11/list.c",
            "nostrdb/src/bolt11/take.c",
            "nostrdb/src/bolt11/tal.c",
            "nostrdb/src/bolt11/talstr.c",
            "nostrdb/src/bolt11/utf8.c",
            "nostrdb/src/invoice.c",
            "nostrdb/src/nostr_bech32.c",
            "nostrdb/src/content_parser.c",
            "nostrdb/src/block.c",
            "nostrdb/deps/flatcc/src/runtime/json_parser.c",
            "nostrdb/deps/flatcc/src/runtime/verifier.c",
            "nostrdb/deps/flatcc/src/runtime/builder.c",
            "nostrdb/deps/flatcc/src/runtime/emitter.c",
            "nostrdb/deps/flatcc/src/runtime/refmap.c",
            "nostrdb/deps/lmdb/mdb.c",
            "nostrdb/deps/lmdb/midl.c",
        ])
        .include("nostrdb/deps/lmdb")
        .include("nostrdb/deps/flatcc/include")
        .include("nostrdb/deps/secp256k1/include")
        // Add other include paths
        //.flag("-Wall")
        .flag("-Wno-sign-compare")
        .flag("-Wno-misleading-indentation")
        .flag("-Wno-unused-function")
        .flag("-Wno-unused-parameter");
    //.flag("-Werror")
    //.flag("-g")

    if env::var("PROFILE").unwrap() == "debug" {
        build.flag("-DDEBUG");
        build.flag("-O1");
    }

    build.compile("libnostrdb.a");

    secp256k1_build();

    // Re-run the build script if any of the C files or headers change
    for file in &[
        "nostrdb/src/nostrdb.c",
        "nostrdb/src/sha256.c",
        "nostrdb/src/nostrdb.h",
        "nostrdb/src/sha256.h",
    ] {
        println!("cargo:rerun-if-changed={}", file);
    }

    println!("cargo:rustc-link-lib=secp256k1");

    // Print out the path to the compiled library
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rustc-link-search=native={}", out_path.display());
    println!("cargo:rustc-link-lib=static=nostrdb");

    // Link Security framework on macOS
    //if cfg!(target_os = "macos") {
        //println!("cargo:rustc-link-lib=framework=Security");
    //}

    //
    // We only need bindgen when we update the bindings.
    // I don't want to complicate the build with it.
    //

    #[cfg(feature = "bindgen")]
    {
        let bindings = bindgen::Builder::default()
            .header("nostrdb/src/nostrdb.h")
            .generate()
            .expect("Unable to generate bindings");

        bindings
            .write_to_file("src/bindings.rs")
            .expect("Couldn't write bindings!");
    }
}
