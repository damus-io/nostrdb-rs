{ pkgs ? import <nixpkgs> {} }:
with pkgs;
mkShell {
  nativeBuildInputs = [ rustPlatform.bindgenHook rustup libiconv pkg-config valgrind gdb ];

  LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib";
}
