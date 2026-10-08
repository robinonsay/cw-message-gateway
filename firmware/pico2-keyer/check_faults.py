#!/usr/bin/env python3
"""Check, in the built firmware, that the fault and panic handlers open the key first.

The safety audit's KB-9: rustos's HardFault handler (`OnHardFault`), the handler
of every other unexpected exception (`DefaultHandler`) and the panic handler
must drive the key pin, GP16, low before they do anything else, rather than
leave it as it was until the watchdog resets the chip, and before they turn off
the PWM outputs, so that the key never waits behind another peripheral. The
firmware names `safe_state` in `pico2::entry!`, and rustos's handlers call it;
the panic handler calls it too. Nothing on a computer can run a fault handler,
so this reads the machine code instead. It follows each handler, and any
function it calls, in a straight line from its first instruction, and passes
only if the first store it makes, other than to the stack, puts a value with
bit 16 set in SIO GPIO_OUT_CLR (0xd0000020, RP2350 datasheet, SIO registers).
A branch, a return, a store anywhere else or a store it cannot follow, before
that store, fails the handler.

The panic handler has no symbol of its own once it is inlined: this checks
`rust_begin_unwind` (whatever its mangling) if it exists, and `core::panicking::panic_fmt`, which calls
or inlines it, otherwise.

Usage: check_faults.py ELF

It needs llvm-objdump: the rustup toolchain's (component llvm-tools), or one on
the PATH.
"""

import os
import re
import shutil
import subprocess
import sys

GPIO_OUT_CLR = 0xD000_0020
KEY_BIT = 1 << 16
HANDLERS = ["OnHardFault", "DefaultHandler"]


def objdump() -> str:
    try:
        sysroot = subprocess.run(
            ["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True
        ).stdout.strip()
        for root, _dirs, files in os.walk(os.path.join(sysroot, "lib", "rustlib")):
            if "llvm-objdump" in files:
                return os.path.join(root, "llvm-objdump")
    except (OSError, subprocess.CalledProcessError):
        pass
    found = shutil.which("llvm-objdump")
    if not found:
        sys.exit("check_faults.py: no llvm-objdump (rustup component add llvm-tools)")
    return found


def panic_symbol(tool: str, elf: str) -> str:
    """The symbol the panic handler's code is in."""
    out = subprocess.run([tool, "-t", elf], capture_output=True, text=True, check=True).stdout
    names = [line.split()[-1] for line in out.splitlines() if " F " in line]
    unwind = [n for n in names if n.endswith("rust_begin_unwind")]
    if len(unwind) == 1:
        return unwind[0]
    fmt = [n for n in names if re.search(r"panicking9panic_fmt|panicking::panic_fmt", n)]
    if len(fmt) != 1:
        sys.exit(f"check_faults.py: cannot find the panic handler in {elf}")
    return fmt[0]


def disassemble(tool: str, elf: str, symbol: str) -> list[tuple[int, str, str]]:
    """(address, mnemonic, operands) for each instruction of `symbol`."""
    out = subprocess.run(
        [tool, "-d", "--no-show-raw-insn", f"--disassemble-symbols={symbol}", elf],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    insns = []
    for line in out.splitlines():
        m = re.match(r"\s*([0-9a-f]+):\s+(\S+)\s*(.*)$", line)
        if m:
            insns.append((int(m.group(1), 16), m.group(2), m.group(3).split("@")[0].strip()))
    if not insns:
        sys.exit(f"check_faults.py: no {symbol} in {elf}")
    return insns


def imm(s: str) -> int:
    return int(s.lstrip("#"), 0)


# What a straight-line walk to the first store meets.
OPENED, RETURNED, FAILED = "opened", "returned", "failed"


def first_store(tool: str, elf: str, symbol: str, depth: int = 0) -> tuple[str, str]:
    """Whether the first store `symbol` makes opens the key, and why not."""
    regs: dict[str, int] = {}
    for addr, op, args in disassemble(tool, elf, symbol):
        a = [x.strip() for x in re.split(r",(?![^\[{]*[\]}])", args)] if args else []
        where = f"{op} {args} at {addr:#x} in {symbol}"
        if op in ("movs", "mov", "mov.w", "movw") and len(a) == 2 and a[1].startswith("#"):
            regs[a[0]] = imm(a[1]) & 0xFFFF_FFFF
        elif op == "movt" and len(a) == 2 and a[0] in regs:
            regs[a[0]] = (imm(a[1]) << 16) | (regs[a[0]] & 0xFFFF)
        elif op.startswith("st") or op.startswith("vst"):
            m = re.match(r"\[(\w+)(?:,\s*#(-?(?:0x)?[0-9a-f]+))?\]", a[1]) if len(a) == 2 else None
            if m and m.group(1) == "sp":
                continue
            if op in ("str", "str.w") and m and m.group(1) in regs and a[0] in regs:
                at = regs[m.group(1)] + (int(m.group(2), 0) if m.group(2) else 0)
                if at == GPIO_OUT_CLR and regs[a[0]] & KEY_BIT:
                    return OPENED, where
            return FAILED, f"a store before the key is opened: {where}"
        elif op in ("bl", "blx"):
            callee = re.search(r"<([^>+]+)>", args)
            if not callee or depth >= 2:
                return FAILED, f"a call it cannot follow before the key is opened: {where}"
            got, why = first_store(tool, elf, callee.group(1), depth + 1)
            if got != RETURNED:
                return got, why
            # The callee may have changed the caller-saved registers.
            for r in ("r0", "r1", "r2", "r3", "r12"):
                regs.pop(r, None)
        elif op in ("bx", "pop", "pop.w") and ("lr" in args or "pc" in args) and depth > 0:
            return RETURNED, where
        elif op.startswith("b") or op.startswith("cb") or op in ("pop", "pop.w", "bkpt", "udf"):
            return FAILED, f"leaves the straight line before the key is opened: {where}"
        elif op in ("push", "push.w") or (op == "mov" and a == ["r7", "sp"]):
            continue
        elif a:
            # Anything else may change its first operand.
            regs.pop(a[0], None)
    return FAILED, f"{symbol} ends before the key is opened"


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: check_faults.py ELF")
    tool = objdump()
    elf = sys.argv[1]
    failed = False
    for name, symbol in [(h, h) for h in HANDLERS] + [("panic", panic_symbol(tool, elf))]:
        got, why = first_store(tool, elf, symbol)
        if got == OPENED:
            print(f"{name}: opens the key (GP16) with its first store ({why})")
        else:
            print(f"{name}: FAILS to open the key (GP16) first: {why}")
            failed = True
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
