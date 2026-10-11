#!/usr/bin/env python3
"""Check, in the built keyer firmware, that the code core 1 runs never drives the
key, PTT or tone pins, nor reaches anything else of core 0's but the memory the two
cores share.

The box's decoder runs on the processor's second core, core 1, and must never be
able to hold the key (docs/keyer.md; the design note's §7.3, rule 1). Its memory
protection unit enforces that while it runs (src/core1.rs). This checks the same in
the machine code, with the analysis firmware/pico2-decoder/check_pins.py makes of
the decoder box's image: what each register can hold at every instruction of every
function, following every branch, jump table and call.

Core 1's code is everything reachable from its entry (rustos's `core1_start` and
`core1_run`, and `pico2_keyer::core1::main`) by direct calls and branches, and, at
a call through a register whose value the analysis does not know, every function
whose address the image takes anywhere: a word in `.rodata` or `.data`, or a
constant some instruction builds (not the vector table's entries, which only the
processor follows). It stops at the exception handlers and the panic
handler, which both cores run and which are the keyer's own safe state:
check_faults.py checks that their first store opens the key and the PTT.

In core 1's code:

1. A store to an address known here goes to SRAM (0x20000000-0x20081fff), to core
   1's own CPACR (rustos turns on its floating-point unit) or MPU registers
   (MPU_CTRL, MPU_RNR, MPU_RBAR, MPU_RLAR, MPU_MAIR0; Armv8-M ARM, D1.2.14 and
   D1.2.168-D1.2.175; each core has its own), to the ADC's FCS (0x400a0008, RP2350 datasheet Table 1119),
   or is a store to SIO GPIO_OUT_CLR (0xd0000020) of the key's and the PTT's bits
   only (`TEST CORE1 FAULT`, which the MPU must refuse). Nothing else: no SIO
   register that sets a pin (GPIO_OUT, GPIO_OUT_SET, GPIO_OUT_XOR, GPIO_OE and its
   aliases), no IO_BANK0, PADS_BANK0, PWM, USB, watchdog, RESETS or CLOCKS register.
2. No store goes at an address not known here into the 64 kB around any of those
   blocks, or into the Private Peripheral Bus, so that an address computed at run
   time cannot get round rule 1.
3. No coprocessor instruction but the FPU's (check_pins.py's rule 6): the GPIO
   coprocessor, coprocessor 0, drives GPIO_OUT with no store. A store to CPACR
   (allowed by rule 1) is rustos's enable_fpu, or a value known here with
   coprocessor 0's bits clear (check_pins.py's rule 7).
4. Core 1's code does not include core 0's: the keyer's `main` and its control loop
   are not reachable from it. If they were, the reachability above would have
   failed to tell the two apart, and the check fails rather than pass.

A store through a pointer derived from no such address (the stack, the decoder's
buffers, the rings, the ADC ring) is counted, not checked: the MPU bounds those to
core 1's own memory. What this does not follow is as for check_pins.py: an address
loaded from memory, code llvm-objdump cannot decode (it fails on any), and other bus
masters.

Usage: check_core1.py ELF
       check_core1.py --self-test

It needs llvm-objdump (component llvm-tools) and firmware/pico2-decoder/check_pins.py.
"""

import contextlib
import io
import os
import re
import struct
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "pico2-decoder"))
import check_pins as cp  # noqa: E402

SRAM = range(0x2000_0000, 0x2008_2000)
# Core 1's own: its floating-point unit (rustos's core1_run) and its MPU (core1.rs).
SYSTEM = {0xE000_ED88: "CPACR", 0xE000_ED94: "MPU_CTRL", 0xE000_ED98: "MPU_RNR",
          0xE000_ED9C: "MPU_RBAR", 0xE000_EDA0: "MPU_RLAR", 0xE000_EDC0: "MPU_MAIR0"}
ADC_FCS = 0x400A_0008
CPACR = 0xE000_ED88
GPIO_OUT_CLR = 0xD000_0020
OPEN_BITS = (1 << 16) | (1 << 17)
# The blocks (address >> 16) no store may go into at an address not known: the SIO
# (and its Non-secure mirror), IO_BANK0 and RESETS, CLOCKS, PADS_BANK0, the ADC and
# PWM, the watchdog, USB, and the Private Peripheral Bus.
WATCHED = {0xD000: "the SIO", 0xD001: "the SIO", 0xD002: "the SIO", 0xD003: "the SIO",
           0x4001: "CLOCKS' 64 kB", 0x4002: "IO_BANK0's 64 kB", 0x4003: "PADS_BANK0's 64 kB",
           0x400A: "the ADC's and PWM's 64 kB", 0x400D: "the watchdog's 64 kB",
           0x5010: "USB's 64 kB", 0x5011: "USB's 64 kB", 0xE000: "the Private Peripheral Bus",
           0xE002: "the Private Peripheral Bus's Non-secure mirror"}
ROOTS = [r"9multicore11core1_start$", r"9multicore9core1_run$", r"11pico2_keyer5core14main$"]
# Core 0's: the keyer's main (rustos's __rustos_main calls it) and its control loop.
CORE0 = [r"11pico2_keyer4main$", r"__rustos_main$", r"7control.*Control4pass"]


def adr_target(i: cp.Insn) -> int | None:
    """Where an `adr` points: Align(PC, 4) + imm, PC being its address + 4."""
    k = cp.imm(i.ops[1]) if i.base == "adr" and len(i.ops) == 2 else None
    return None if k is None else ((i.addr + 4) & ~3) + k


def taken(image: cp.Image, fns: dict) -> set[str]:
    """Every function whose address the image takes: a word in its data, a constant
    an instruction builds, or an `adr`."""
    out = set()

    def note(v: int) -> None:
        if v & 1 and v & ~1 in image.starts:
            out.add(image.starts[v & ~1])

    for lo, hi in image.data:
        for at in range(lo, hi - 3, 4):
            raw = image.read(at, 4)
            if raw:
                note(int.from_bytes(raw, "little"))
    for f in fns.values():
        for k, i in enumerate(f.insns):
            t = adr_target(i)
            if t is not None:
                note(t | 1)
            for v in f.states.get(k, {}).values():
                for a in v:
                    if isinstance(a, int):
                        note(a)
    return out


def core1_code(image: cp.Image, fns: dict) -> tuple[set[str], list[str]]:
    """Core 1's functions, and the failures in finding them."""
    failures = []
    roots = []
    for pattern in ROOTS:
        found = [n for n in image.funcs if re.search(pattern, n)]
        if len(found) != 1:
            failures.append(f"cannot find core 1's entry {pattern!r} in the image")
        roots += found
    stop = {image.starts.get(v & ~1) for v in image.vectors} | {cp.panic_symbol(image)}
    anywhere = None
    seen, work = set(), list(roots)
    while work:
        name = work.pop()
        if name in seen or name in stop:
            continue
        seen.add(name)
        f = fns[name]
        for k, i in enumerate(f.insns):
            if i.base == "bl" or i.is_branch():
                callee = image.starts.get(i.target)
                if callee:
                    work.append(callee)
            elif i.base == "blx" or (i.base == "bx" and i.args != "lr"):
                v = f.states.get(k, {}).get(cp.reg(i.ops[0]), cp.ANYTHING)
                if v and all(isinstance(a, int) and a & ~1 in image.starts for a in v):
                    work += [image.starts[a & ~1] for a in v]
                else:
                    if anywhere is None:
                        anywhere = taken(image, fns)
                    work += sorted(anywhere)
    for pattern in CORE0:
        for name in sorted(seen):
            if re.search(pattern, name):
                failures.append(f"core 0's {name} is reachable from core 1's code: the check "
                                "cannot tell the two cores' code apart")
    return seen, failures


def check_store(f: cp.Function, k: int, i: cp.Insn, where: str, failures: list) -> int:
    """Rules 1-3 at the store `i`, the `k`th instruction of `f`: 1 if it can go to an
    address not known, else 0."""
    s = f.states.get(k, {})
    b, ops = i.base, i.ops
    if b.startswith(("stm", "vstm")):
        if ops[0].rstrip("!") == "sp":
            return 0
        at = s.get(cp.reg(ops[0]), cp.ANYTHING)
        regs = re.findall(r"\b(r\d+|lr|sb|sl|fp|ip|[sd]\d+)\b", ops[1] if len(ops) > 1 else "")
        width = 8 if regs and regs[0].startswith("d") else 4
        every = set()
        for a in at:
            if isinstance(a, int):
                start = a - width * len(regs) if "db" in i.op else a
                every |= {start + width * k for k in range(len(regs))}
            else:
                every.add(a)
        values = [cp.ANYTHING] * len(every)
    else:
        rn, at = f.address(s, i)
        if rn == "sp":
            return 0
        every = set(at)
        value = cp.ANYTHING
        if not b.startswith(("strex", "strd", "vst")) and cp.reg(ops[0]):
            value = s.get(cp.reg(ops[0]), cp.ANYTHING)
        values = [value] * len(every)
    unknown = 0
    for a, value in zip(sorted(every, key=str), values):
        if isinstance(a, int):
            if a & ~3 == CPACR and not (b == "str" and cp.enable_fpu(f, k, a)) and not (
                    value and all(isinstance(x, int) and x & cp.CP0 == 0 for x in value)):
                failures.append(f"a store to CPACR that could turn the GPIO coprocessor on: {where}")
                continue
            if a in SRAM or a & ~3 in SYSTEM or a & ~3 == ADC_FCS:
                continue
            if a & ~3 == GPIO_OUT_CLR and value and all(
                    isinstance(x, int) and x & ~OPEN_BITS == 0 for x in value):
                continue
            failures.append(f"a store to {a:#x}, which core 1 may not write: {where}")
        elif isinstance(a, tuple):
            failures.append(f"a store into {WATCHED[a[1]]} at an address not known here: {where}")
        else:
            unknown = 1
    return unknown


def run(image: cp.Image) -> list[str]:
    """Every failure, printing what passed."""
    cp.WATCHED.clear()
    cp.WATCHED.update(WATCHED)
    fns = cp.solve_image(image)
    code, failures = core1_code(image, fns)
    unknown = 0
    for name in sorted(code):
        f = fns[name]
        for k, i in enumerate(f.insns):
            where = f"{i} in {name}"
            if i.op == "<unknown>":
                failures.append(f"an instruction llvm-objdump cannot decode: {where}")
            elif i.base.startswith("ldr") and re.search(r"\[pc\b", i.args):
                failures.append(f"a load from a literal pool, which this check does not follow: {where}")
            elif cp.COPROCESSOR.fullmatch(i.base) and not (i.ops and i.ops[0] in cp.FPU_COPROCESSORS):
                failures.append(f"a coprocessor instruction, which can drive GPIO_OUT with no store "
                                f"(coprocessor 0): {where}")
            elif i.base.startswith(("str", "stm", "vst")):
                unknown += check_store(f, k, i, where, failures)
    print(f"core 1's code: {len(code)} functions")
    print(f"{unknown} stores in it through pointers derived from no watched address "
          "(the stack, buffers, the rings), not checked")
    return failures


def elf_image(path: str) -> cp.Image:
    image = cp.elf_image(path)
    image.data = []
    with open(path, "rb") as f:
        data = f.read()
    (shoff,) = struct.unpack_from("<I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x2E)
    headers = [struct.unpack_from("<IIIIII", data, shoff + k * shentsize) for k in range(shnum)]
    strtab = headers[shstrndx][4]
    for name_off, kind, flags, addr, _offset, size in headers:
        name = data[strtab + name_off:data.index(b"\0", strtab + name_off)].decode()
        # Loaded data, but not the vector table, whose words are the exception
        # entries: the reset handler is core 0's, and the others stop the search.
        if kind == 1 and flags & 2 and name not in (".text", ".vector_table"):
            image.data.append((addr, addr + size))
    return image


def self_test() -> None:
    """The check against small made-up images, so that it is seen to fail."""
    names = {"core1_start": "_RNvNtCs0_5pico29multicore11core1_start",
             "core1_run": "_RNvNtCs0_5pico29multicore9core1_run",
             "core1": "_RNvNtCs0_11pico2_keyer5core14main",
             "main": "_RNvCs0_11pico2_keyer4main",
             "handler": "OnHardFault",
             "panic": "core::panicking::panic_fmt"}

    def image(core1: str, extra: dict | None = None, data: dict | None = None) -> cp.Image:
        """Core 1's main is `core1`; core 0's main sets the key (GPIO_OUT_SET), as
        the keyer does; each line 4 bytes. `@name@` in a line is that function's
        address."""
        funcs = {"core1_start": "b @core1_run@\n", "core1_run": "ldr r0, [r1]\nblx r0\n",
                 "core1": core1 + "bx lr\n",
                 "main": "movs r0, #0x18\nmovt r0, #0xd000\nmov.w r1, #0x10000\nstr r1, [r0]\nbx lr\n",
                 "handler": "b 0x0\n", "panic": "b 0x0\n", **(extra or {})}
        starts, addr = {}, 0x1000
        for name, code in funcs.items():
            starts[name] = addr
            addr += 0x100 * (1 + code.count("\n") // 64)
        text = ""
        for name, code in funcs.items():
            at = starts[name]
            text += f"{at:08x} <{names.get(name, name)}>:\n"
            for line in code.strip().splitlines():
                for other, where in starts.items():
                    line = line.replace(f"@{other}@", f"{where:#x} <{names.get(other, other)}>")
                op, _, args = line.partition(" ")
                text += f"{at:08x}:      \t{op}\t{args}\n"
                at += 4
        mem = dict(data or {})
        # core1_run's entry pointer, in .data.
        for k, byte in enumerate((starts["core1"] | 1).to_bytes(4, "little")):
            mem[0x2000_0000 + k] = byte

        def read(a, n):
            return bytes(mem[a + k] for k in range(n)) if all(a + k in mem for k in range(n)) else None

        img = cp.Image(cp.parse(text), [starts["handler"] | 1],
                       {at: [names.get(n, n)] for n, at in starts.items()}, read)
        img.data = [(0x2000_0000, 0x2000_0004)]
        return img

    def failures(img: cp.Image) -> list[str]:
        with contextlib.redirect_stdout(io.StringIO()):
            return run(img)

    def fails(img: cp.Image, why: str) -> None:
        got = failures(img)
        assert any(why in g for g in got), f"expected a failure with {why!r}, got {got}"

    def passes(img: cp.Image) -> None:
        got = failures(img)
        assert got == [], got

    allowed = ("movs r0, #0x20\nmovt r0, #0xd000\nmov.w r1, #0x30000\nstr r1, [r0]\n"
               "movw r0, #0xed94\nmovt r0, #0xe000\nmovs r1, #1\nstr r1, [r0]\n"
               "movs r0, #0x8\nmovt r0, #0x400a\nstr r1, [r0]\n"
               "movw r0, #0x4000\nmovt r0, #0x2000\nstr r1, [r0, #0x8]\n"
               "ldr r0, [r2]\nstr r1, [r0]\n")
    # Core 0 sets the key; core 1's allowed stores, and one through a pointer from
    # memory.
    passes(image(allowed))
    cannot = "which core 1 may not write"
    cases = {
        "GPIO_OUT_SET": "movs r0, #0x18\nmovt r0, #0xd000\nmovs r1, #1\nstr r1, [r0]\n",
        "GPIO_OUT_CLR of the tone": "movs r0, #0x20\nmovt r0, #0xd000\nmov.w r1, #0x40000\nstr r1, [r0]\n",
        "GPIO_OE_SET": "movs r0, #0x38\nmovt r0, #0xd000\nstr r1, [r0]\n",
        "GPIO16_CTRL": "movw r0, #0x8084\nmovt r0, #0x4002\nmovs r1, #5\nstr r1, [r0]\n",
        "a pad": "movw r0, #0x8044\nmovt r0, #0x4003\nstr r1, [r0]\n",
        "PWM": "movw r0, #0x800c\nmovt r0, #0x400a\nstr r1, [r0]\n",
        "the watchdog": "movs r0, #0x0\nmovt r0, #0x400d\nstr r1, [r0]\n",
        "VTOR": "movw r0, #0xed08\nmovt r0, #0xe000\nstr r1, [r0]\n",
    }
    for why, body in cases.items():
        got = failures(image(body))
        assert any(cannot in g for g in got), f"{why}: {got}"
    fails(image("ldr r2, [r3]\nmovs r0, #0x0\nmovt r0, #0xd000\nadd r0, r2\nstr r1, [r0]\n"),
          "into the SIO at an address not known")
    fails(image("ldr r2, [r3]\nmovw r0, #0x8000\nmovt r0, #0x400a\nstr r1, [r0, r2]\n"),
          "into the ADC's and PWM's 64 kB at an address not known")
    # Through a direct call, a tail call, and a call through a register whose
    # target only the image's data names.
    helper = {"helper": "movs r0, #0x18\nmovt r0, #0xd000\nstr r1, [r0]\nbx lr\n"}
    fails(image("bl @helper@\n", extra=helper), cannot)
    fails(image("b @helper@\n", extra=helper), cannot)
    probe = image("bx lr\n", extra=helper)
    helper_at = next(a for a, n in probe.starts.items() if n == "helper")
    word = (helper_at | 1).to_bytes(4, "little")
    via_data = image("ldr r3, [r4]\nblx r3\n", extra=helper,
                     data={0x2000_0004 + k: b for k, b in enumerate(word)})
    via_data.data = [(0x2000_0000, 0x2000_0008)]
    fails(via_data, cannot)
    # A call through a register to a constant the code builds.
    fails(image(f"movw r3, #{helper_at | 1:#x}\nmovt r3, #0x0\nblx r3\n", extra=helper), cannot)
    # The handlers are not core 1's code here (check_faults.py's); core 0's main
    # reachable from core 1 fails.
    passes(image("b @handler@\n", extra={"handler": "movs r0, #0x18\nmovt r0, #0xd000\nstr r1, [r0]\nb 0x0\n"}))
    fails(image("bl @main@\n"), "core 0's")
    fails(image("<unknown>\n"), "cannot decode")
    # The GPIO coprocessor (rule 3): any coprocessor instruction but the FPU's fails.
    for insn in ("mcr p0, #0x1, r0, c0, c0", "mcr2 p0, #0x0, r0, c0, c0",
                 "mcrr p0, #0x0, r0, r1, c0", "mrc p0, #0x0, r0, c0, c0",
                 "it ne\nmcrne p0, #0x1, r0, c0, c0"):
        fails(image(insn + "\n"), "a coprocessor instruction")
    passes(image("vmov s0, r1\nvadd.f32 s0, s0, s1\nmcr p10, #0x7, r0, c1, c0, #0x0\n"))
    # CPACR (rule 3): rustos's read-modify-write and core1.rs's known value pass;
    # a value with coprocessor 0's bits, or one not known, fails, and so does its
    # Non-secure mirror.
    cpacr = "movw r0, #0xed88\nmovt r0, #0xe000\n"
    passes(image(cpacr + "ldr r1, [r0]\norr r1, r1, #0xf00000\nstr r1, [r0]\n"))
    passes(image(cpacr + "mov.w r1, #0xf00000\nstr r1, [r0]\n"))
    cpacr_fail = "a store to CPACR that could turn the GPIO coprocessor on"
    fails(image(cpacr + "movs r1, #0x3\nstr r1, [r0]\n"), cpacr_fail)
    fails(image(cpacr + "ldr r1, [r2]\nstr r1, [r0]\n"), cpacr_fail)
    fails(image(cpacr + "ldr r1, [r0]\norr r1, r1, #0xf00003\nstr r1, [r0]\n"), cpacr_fail)
    fails(image(cpacr + "ldr r1, [r0]\norr r1, r1, #0xf00000\nadds r1, #0x1\nstr r1, [r0]\n"),
          cpacr_fail)
    fails(image("movw r0, #0xed88\nmovt r0, #0xe002\nmov.w r1, #0xf00000\nstr r1, [r0]\n"), cannot)
    print("self-test passed")


def main() -> None:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    if len(sys.argv) != 2:
        sys.exit("usage: check_core1.py ELF | --self-test")
    failures = run(elf_image(sys.argv[1]))
    for f in failures:
        print(f"FAIL: {f}")
    if failures:
        sys.exit(1)
    print("core 1's code: no store to a pin, a pad, the PWM or anything of core 0's "
          "but the memory the cores share")


if __name__ == "__main__":
    main()
