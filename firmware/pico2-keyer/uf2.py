#!/usr/bin/env python3
"""Turn the keyer box's firmware (an ELF file) into a UF2 file for the Pico 2.

    python3 uf2.py target/thumbv8m.main-none-eabihf/release/pico2-keyer pico2-keyer.uf2

A UF2 file is what the Pico 2 takes when it is plugged in with BOOTSEL held: it
appears as a drive called RP2350, and a .uf2 file copied onto it is written to
flash. Each 512-byte block carries 256 bytes for one flash address (UF2
specification, github.com/microsoft/uf2; RP2350 datasheet section 5.5).

This does what `picotool uf2 convert` does for a plain image with no partition
table, so the build needs nothing beyond Python. It also checks the image before
writing it, and refuses one the RP2350's boot ROM would not start:

- every loaded byte lies in the Pico 2's 4 MB of flash;
- the image starts at the start of flash with an Arm vector table: an initial
  stack pointer in SRAM, and a reset handler in the image (a Thumb address);
- the IMAGE_DEF block the boot ROM looks for is within the first 4 kB (datasheet
  5.9.5.1; without it the chip refuses to boot and comes back as the RP2350 drive).
"""

import struct
import sys

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY_ID_PRESENT = 0x00002000
# The family the RP2350's boot ROM takes for Arm Secure images ("rp2350-arm-s").
RP2350_ARM_S = 0xE48BFF59

FLASH_START = 0x10000000
FLASH_SIZE = 4 * 1024 * 1024  # Pico 2
SRAM_START = 0x20000000
SRAM_END = 0x20082000  # SRAM0-9, 520 kB
PAGE = 256
IMAGE_DEF_START = 0xFFFFDED3  # PICOBIN_BLOCK_MARKER_START
IMAGE_DEF_END = 0xAB123579  # PICOBIN_BLOCK_MARKER_END
IMAGE_DEF_SEARCH = 4096

PT_LOAD = 1
EM_ARM = 40


class ImageError(Exception):
    pass


def load_segments(elf):
    """The (address, bytes) of each loaded segment, at its load address."""
    if elf[:4] != b"\x7fELF":
        raise ImageError("not an ELF file")
    if elf[4] != 1 or elf[5] != 1:
        raise ImageError("not a 32-bit little-endian ELF file")
    (machine,) = struct.unpack_from("<H", elf, 18)
    if machine != EM_ARM:
        raise ImageError(f"not an Arm ELF file (machine {machine})")
    phoff, = struct.unpack_from("<I", elf, 28)
    phentsize, phnum = struct.unpack_from("<HH", elf, 42)
    segments = []
    for i in range(phnum):
        p_type, p_offset, _vaddr, p_paddr, p_filesz, _memsz, _flags, _align = (
            struct.unpack_from("<8I", elf, phoff + i * phentsize)
        )
        if p_type != PT_LOAD or p_filesz == 0:
            continue
        data = elf[p_offset : p_offset + p_filesz]
        if len(data) != p_filesz:
            raise ImageError(f"segment {i} runs past the end of the file")
        if not (FLASH_START <= p_paddr and p_paddr + p_filesz <= FLASH_START + FLASH_SIZE):
            raise ImageError(
                f"segment {i} at {p_paddr:#010x}+{p_filesz:#x} is not in the Pico 2's flash"
            )
        segments.append((p_paddr, data))
    if not segments:
        raise ImageError("nothing to load")
    return segments


def pages(segments):
    """The image as {page address: 256 bytes}, gaps inside a page as zeros."""
    out = {}
    for addr, data in segments:
        for i, b in enumerate(data):
            a = addr + i
            page = out.setdefault(a - a % PAGE, bytearray(PAGE))
            page[a % PAGE] = b
    return out


def check_image(image):
    """Refuse an image the boot ROM would not start."""
    first = image.get(FLASH_START)
    if first is None:
        raise ImageError("nothing at the start of flash: no vector table")
    sp, reset = struct.unpack_from("<II", first, 0)
    if not (SRAM_START < sp <= SRAM_END) or sp % 8:
        raise ImageError(f"initial stack pointer {sp:#010x} is not the top of SRAM")
    handler = reset & ~1
    if not reset & 1 or handler - handler % PAGE not in image:
        raise ImageError(f"reset handler {reset:#010x} is not Thumb code in the image")
    head = b"".join(
        bytes(image.get(FLASH_START + a, bytes(PAGE))) for a in range(0, IMAGE_DEF_SEARCH, PAGE)
    )
    words = [w for (w,) in struct.iter_unpack("<I", head)]
    for i, w in enumerate(words):
        if w == IMAGE_DEF_START and IMAGE_DEF_END in words[i + 1 :]:
            return
    raise ImageError("no IMAGE_DEF block in the first 4 kB: the RP2350 would not boot it")


def uf2(image):
    addrs = sorted(image)
    blocks = []
    for n, addr in enumerate(addrs):
        header = struct.pack(
            "<8I",
            UF2_MAGIC_START0,
            UF2_MAGIC_START1,
            UF2_FLAG_FAMILY_ID_PRESENT,
            addr,
            PAGE,
            n,
            len(addrs),
            RP2350_ARM_S,
        )
        data = bytes(image[addr]).ljust(476, b"\0")
        blocks.append(header + data + struct.pack("<I", UF2_MAGIC_END))
    return b"".join(blocks)


def main(argv):
    if len(argv) != 3:
        sys.exit(f"usage: {argv[0]} <firmware ELF> <output .uf2>")
    with open(argv[1], "rb") as f:
        elf = f.read()
    try:
        image = pages(load_segments(elf))
        check_image(image)
    except ImageError as e:
        sys.exit(f"{argv[1]}: {e}")
    out = uf2(image)
    with open(argv[2], "wb") as f:
        f.write(out)
    end = max(image) + PAGE
    print(
        f"{argv[2]}: {len(image)} blocks, flash {FLASH_START:#010x}-{end:#010x} "
        f"({end - FLASH_START} bytes), family rp2350-arm-s"
    )


if __name__ == "__main__":
    main(sys.argv)
