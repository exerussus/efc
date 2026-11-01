#!/usr/bin/env sh
# Сборка efc под Linux: статический бинарник без зависимостей (glibc static-pie).
# Для musl: rustup target add x86_64-unknown-linux-musl, затем TARGET=x86_64-unknown-linux-musl ./build.sh
set -e
TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" cargo build --release --target "$TARGET"
mkdir -p dist
cp "target/$TARGET/release/efc" "dist/efc-$TARGET"
echo "Готово: dist/efc-$TARGET"
