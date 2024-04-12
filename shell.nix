{ pkgs ? import <nixpkgs> {} }:
with pkgs;
mkShell {
  nativeBuildInputs = [ rustPlatform.bindgenHook cargo clippy rustc rustfmt libiconv pkg-config valgrind ];

  LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib";
}
