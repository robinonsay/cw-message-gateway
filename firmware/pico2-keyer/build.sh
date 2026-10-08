#!/bin/sh
# Builds the keyer box's firmware the way CI does, so that a commit's UF2 is the
# same file whoever builds it and wherever their checkout is. It writes, next to
# this script: pico2-keyer.uf2, pico2-keyer.uf2.sha256 and pico2-keyer.elf, the
# ELF file the UF2 came from (for check_faults.py).
#
# It builds what is committed, not the working tree: `git archive` of COMMIT
# (default HEAD), unpacked at one fixed path, /tmp/pico2-keyer-build, with the
# commit's first eight characters as KEYER_BUILD_ID. The fixed path is what
# makes it reproducible. keyer-core (crates/keyer-core) is outside this crate's
# Cargo workspace, and cargo mixes the absolute path of such a dependency into
# the hash in every symbol name; the order the code is laid out in follows those
# names, so a plain `cargo build` of one commit in two checkouts can give two
# different UF2s. Two builds at once would share that path: run one at a time.
#
# Usage: sh build.sh [COMMIT]
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
commit="${1:-HEAD}"
id="$(git -C "$repo" rev-parse --short=8 "$commit")"

dir=/tmp/pico2-keyer-build
rm -rf "$dir"
mkdir -p "$dir"
git -C "$repo" archive "$commit" | tar -x -C "$dir"
fw="$dir/firmware/pico2-keyer"
elf="$fw/target/thumbv8m.main-none-eabihf/release/pico2-keyer"

# Nothing in the caller's environment may change what is built.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR
# From inside the unpacked tree, so that rustup takes its rust-toolchain.toml.
(cd "$fw" && KEYER_BUILD_ID="$id" cargo build --release --locked --manifest-path "$fw/Cargo.toml")

cp "$elf" "$here/pico2-keyer.elf"
python3 "$fw/uf2.py" "$elf" "$here/pico2-keyer.uf2"
python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest() + "  pico2-keyer.uf2")' \
  "$here/pico2-keyer.uf2" >"$here/pico2-keyer.uf2.sha256"
echo "build id $id, $(wc -c <"$here/pico2-keyer.uf2" | tr -d ' ') bytes, UF2 SHA-256 $(cut -d ' ' -f 1 "$here/pico2-keyer.uf2.sha256")"
