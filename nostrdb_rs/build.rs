// build.rs
use cc::Build;
use std::env;
use std::path::PathBuf;

fn secp256k1_build(base_config: &mut Build) {
    // Actual build
    //let mut base_config = cc::Build::new();
    base_config
        .include("nostrdb/deps/secp256k1/")
        .include("nostrdb/deps/secp256k1/include")
        .include("nostrdb/deps/secp256k1/src")
        .flag_if_supported("-Wno-unused-function") // some ecmult stuff is defined but not used upstream
        .flag_if_supported("-Wno-unused-parameter") // patching out printf causes this warning
        .define("SECP256K1_STATIC", "1")
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

    //base_config.compile("libsecp256k1.a");
}

/// bolt11 deps with portability issues, exclude these on windows build
fn bolt11_deps() -> &'static [&'static str] {
    &[
        "nostrdb/ccan/ccan/likely/likely.c",
        "nostrdb/ccan/ccan/list/list.c",
        "nostrdb/ccan/ccan/mem/mem.c",
        "nostrdb/ccan/ccan/str/debug.c",
        "nostrdb/ccan/ccan/str/str.c",
        "nostrdb/ccan/ccan/take/take.c",
        "nostrdb/ccan/ccan/tal/str/str.c",
        "nostrdb/ccan/ccan/tal/tal.c",
        "nostrdb/ccan/ccan/utf8/utf8.c",
        "nostrdb/src/bolt11/bolt11.c",
        "nostrdb/src/bolt11/amount.c",
        "nostrdb/src/bolt11/hash_u5.c",
    ]
}

fn main() {
    // Compile the C file
    let mut build = Build::new();

    build
        .files([
            "nostrdb/src/nostrdb.c",
            "nostrdb/src/invoice.c",
            "nostrdb/src/nostr_bech32.c",
            "nostrdb/src/content_parser.c",
            "nostrdb/ccan/ccan/crypto/sha256/sha256.c",
            "nostrdb/src/bolt11/bech32.c",
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
        .include("nostrdb/ccan")
        .include("nostrdb/src");
    // Add other include paths
    //.flag("-Wall")
    //.flag("-Werror")
    //.flag("-g")

    // Link Security framework on macOS
    if !cfg!(target_os = "windows") {
        build.files(bolt11_deps());
        build
            .flag("-Wno-sign-compare")
            .flag("-Wno-misleading-indentation")
            .flag("-Wno-unused-function")
            .flag("-Wno-unused-parameter");
    } else {
        // need this on windows
        println!("cargo:rustc-link-lib=bcrypt");
    }

    if env::var("PROFILE").unwrap() == "debug" {
        build.flag("-DDEBUG");
        build.flag("-O1");
    }

    // Print out the path to the compiled library
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rustc-link-search=native={}", out_path.display());

    secp256k1_build(&mut build);

    build.compile("libnostrdb.a");

    println!("cargo:rustc-link-lib=static=nostrdb");

    // Re-run the build script if any of the C files or headers change
    for file in &["nostrdb/src/nostrdb.c", "nostrdb/src/nostrdb.h"] {
        println!("cargo:rerun-if-changed={}", file);
    }

    // Link Security framework on macOS
    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=Security");
    }

    //
    // We only need bindgen when we update the bindings.
    // I don't want to complicate the build with it.
    //

    #[cfg(feature = "bindgen")]
    {
        let bindings = bindgen::Builder::default()
            .header("nostrdb/src/nostrdb.h")
            .clang_arg("-Inostrdb/ccan")
            .clang_arg("-Inostrdb/src")
            .generate()
            .expect("Unable to generate bindings");

        #[cfg(target_os = "windows")]
        let filename = "src/bindings_win.rs";

        #[cfg(not(target_os = "windows"))]
        let filename = "src/bindings_posix.rs";

        bindings
            .write_to_file(filename)
            .expect("Couldn't write bindings!");
    }
}
