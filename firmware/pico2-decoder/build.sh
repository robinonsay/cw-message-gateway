#!/bin/sh
# Builds the decoder box's firmware the way CI does, so that a commit's UF2 is the
# same file whoever builds it and wherever their checkout is. It writes, next to
# this script: pico2-decoder.uf2, pico2-decoder.uf2.sha256 and pico2-decoder.elf,
# the ELF file the UF2 came from (for check_pins.py).
#
# As the keyer box's build.sh (firmware/pico2-keyer/build.sh, which says why): it
# builds what is committed, not the working tree, `git archive` of COMMIT (default
# HEAD) unpacked at one fixed path, /tmp/pico2-decoder-build, with the commit's
# first eight characters as DECODER_BUILD_ID. decoder-core, cw and keyer-core are
# outside this crate's Cargo workspace, so the fixed path is what makes the UF2
# the same in every checkout. Two builds at once would share that path: run one at
# a time. The UF2 is made by the keyer's uf2.py, from the same commit.
#
# Usage: sh build.sh [COMMIT]
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
commit="${1:-HEAD}"
id="$(git -C "$repo" rev-parse --short=8 "$commit")"

dir=/tmp/pico2-decoder-build
rm -rf "$dir"
mkdir -p "$dir"
git -C "$repo" archive "$commit" | tar -x -C "$dir"
fw="$dir/firmware/pico2-decoder"
elf="$fw/target/thumbv8m.main-none-eabihf/release/pico2-decoder"

# Nothing in the caller's environment may change what is built.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR
# From inside the unpacked tree, so that rustup takes its rust-toolchain.toml.
(cd "$fw" && DECODER_BUILD_ID="$id" cargo build --release --locked --manifest-path "$fw/Cargo.toml")

cp "$elf" "$here/pico2-decoder.elf"
python3 "$dir/firmware/pico2-keyer/uf2.py" "$elf" "$here/pico2-decoder.uf2"
python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest() + "  pico2-decoder.uf2")' \
  "$here/pico2-decoder.uf2" >"$here/pico2-decoder.uf2.sha256"
echo "build id $id, $(wc -c <"$here/pico2-decoder.uf2" | tr -d ' ') bytes, UF2 SHA-256 $(cut -d ' ' -f 1 "$here/pico2-decoder.uf2.sha256")"
