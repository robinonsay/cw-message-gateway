#!/bin/sh
# Builds the NR7Y CW firmware for the Quansheng UV-K1 / UV-K5 v3 with hfnode's serial
# control added (README.md). Needs git, cmake, ninja, python3 and arm-none-eabi-gcc.
#
#   firmware/uv-k1/build.sh [work-dir]      (default: firmware/uv-k1/build)
#
# Fetches the upstream firmware at the commit the patch was made for, applies the
# patch, adds hfnode's files and builds. Writes <work-dir>/nr7y.cw.hfnode.bin, the
# file to flash with UVTools2. The work dir is build.sh's own: the upstream checkout
# in it is reset and cleaned on every build, so build.sh only reuses one it cloned
# itself.
set -eu
unset CDPATH

UPSTREAM=https://github.com/briand/uv-k1-k5v3-firmware-custom.git
COMMIT=47075bf64c5e7c390c16e7c265b24e38ffcea27c

here=$(cd "$(dirname "$0")" && pwd)
work=${1:-$here/build}
mkdir -p "$work"
work=$(cd "$work" && pwd)
src=$work/uv-k1-k5v3-firmware-custom

for tool in git cmake ninja arm-none-eabi-gcc; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "build.sh: $tool is not installed (README.md, \"Building\")" >&2
        exit 1
    }
done
# The upstream build runs `python` to make a .uf2 as well; some systems only have
# python3.
if ! command -v python >/dev/null 2>&1; then
    command -v python3 >/dev/null 2>&1 || {
        echo "build.sh: python3 is not installed" >&2
        exit 1
    }
    mkdir -p "$work/bin"
    ln -sf "$(command -v python3)" "$work/bin/python"
    PATH=$work/bin:$PATH
    export PATH
fi

# Marks a checkout build.sh cloned, inside .git so that `git clean` keeps it.
mark=.git/hfnode-build
if [ ! -e "$src" ]; then
    git clone "$UPSTREAM" "$src"
    : >"$src/$mark"
fi
cd "$src"
if [ ! -f "$mark" ] || [ "$(git rev-parse --git-dir 2>/dev/null)" != .git ] ||
    [ "$(git remote get-url origin 2>/dev/null)" != "$UPSTREAM" ]; then
    echo "build.sh: $src was not cloned by build.sh; it would be reset and" >&2
    echo "cleaned, so it is left alone: delete it or choose another work dir" >&2
    exit 1
fi
git cat-file -e "$COMMIT^{commit}" 2>/dev/null || git fetch origin "$COMMIT" || {
    echo "build.sh: cannot fetch upstream commit $COMMIT" >&2
    exit 1
}
# From the upstream commit every time, with nothing left from an earlier build.
git checkout --quiet --force --detach "$COMMIT"
git clean --quiet -fdx
git apply "$here/nr7y-hfnode.patch"
cp "$here/app/hfnode.c" "$here/app/hfnode.h" "$here/app/hfnode_line.c" \
    "$here/app/hfnode_line.h" App/app/

cmake --preset CW -B build/CW-HFNODE -DENABLE_HFNODE=ON -DTARGET=nr7y.cw.hfnode
cmake --build build/CW-HFNODE
bin=build/CW-HFNODE/nr7y.cw.hfnode.bin
# The patch only builds hfnode in when the CW mod and USB are on: make sure it did,
# by its HELLO name (app/hfnode.c, HF_NAME) in the binary.
LC_ALL=C grep -qaF "NR7Y-CW HFNODE" "$bin" || {
    echo "build.sh: $bin has no hfnode in it" >&2
    exit 1
}
cp "$bin" "$work/"
echo
echo "built $work/nr7y.cw.hfnode.bin ($(wc -c <"$work/nr7y.cw.hfnode.bin" | tr -d ' ') bytes)"
