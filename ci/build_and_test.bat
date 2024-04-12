set "RUSTFLAGS=-D warnings"

:: Print version information
rustc -Vv || exit /b 1
cargo -V || exit /b 1

cargo build || exit /b 1
cargo test || exit /b 1
