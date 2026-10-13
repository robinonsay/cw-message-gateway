#!/usr/bin/env python3
"""Check, in the built decoder firmware, that it never drives the keyer box's key,
PTT or tone pins high, and that its fault and panic handlers drive them low first.

The decoder box never transmits, and its image must hold a radio's key open even if
it is flashed onto the keyer box by mistake: GP16 (key), GP17 (PTT) and GP18 (tone)
are driven low from boot and never high (docs/decoder.md). Nothing on a computer
can run the firmware, so this reads its machine code (llvm-objdump) and checks:

1. No store writes the SIO's GPIO_OUT, GPIO_OUT_SET or GPIO_OUT_XOR, in the SIO or
   in its Non-secure mirror (0xd0000010, 0xd0000018, 0xd0000028, and the same at
   0xd0020000; RP2350 datasheet, Table 24, p64, and §3.1.1, p39). The firmware
   never needs them: it drives pins low through GPIO_OUT_CLR, and its LED and
   regulator pin through their own output overrides.
2. A store to GPIO16_CTRL, GPIO17_CTRL or GPIO18_CTRL (IO_BANK0 0x40028084,
   0x4002808c, 0x40028094) writes a value known here whose FUNCSEL is SIO (5) or
   the null function (0x1f) and whose OUTOVER is "normal" (the SIO's level, which
   rule 1 keeps low) or "drive low": no other function (PWM, PIO, a UART) and no
   override can drive the pin. No store goes to those registers' atomic XOR, set
   or clear aliases (+0x1000, +0x2000, +0x3000), whose effect depends on what was
   there before.
3. No store goes into the SIO (0xd0000000-0xd003ffff) or IO_BANK0's 64 kB
   (0x40020000-0x4002ffff, which it shares with RESETS) at an address this check
   cannot work out, so that an address computed at run time cannot get round
   rules 1 and 2.
4. The image drives GP16-GP18 low (a store of bits 16-18 to GPIO_OUT_CLR,
   0xd0000020) somewhere outside its handlers: the loop's hold. If this finds no
   such store, the code is laid out in a way it cannot follow, and the check
   fails rather than pass by not seeing.
5. Every exception handler in the vector table but the reset handler (in this
   image rustos's HardFault handler, OnHardFault, which is also DefaultHandler,
   the handler of every other exception) and the panic handler make that store
   first, before any other store or any branch, as the keyer firmware's
   check_faults.py checks for its key and PTT.
6. No coprocessor instruction but the FPU's. The GPIO coprocessor, coprocessor
   0, sets, clears and toggles GPIO_OUT's bits with no store at all (`mcr p0, #1,
   Rt, c0, c0` toggles them; RP2350 datasheet §3.6.1, p102), so rules 1-3 cannot
   see it. Any mcr, mcr2, mcrr, mcrr2, mrc, mrc2, mrrc, mrrc2, cdp, cdp2, ldc or
   stc fails unless it names coprocessor 10 or 11, the FPU (which llvm-objdump
   shows as VFP instructions instead).
7. No store to CPACR (0xe000ed88, and its Non-secure mirror at 0xe002ed88;
   Table 229, p194, and the PPB_NONSEC_BASE note, p39) turns coprocessor 0 on
   (CP0, bits 1:0). A store passes only with a value known here whose CP0 bits are
   clear, or as rustos's enable_fpu writes it: a load from CPACR, an orr of a
   constant whose CP0 bits are clear, and the store, in a straight line, which
   leaves CP0 as the bootrom left it. A store into the Private Peripheral Bus
   (0xe0000000-0xe003ffff, CPACR's 64 kB and its mirror's) at an address not
   known fails, as rule 3 does for the SIO.

How: it works out, at every instruction of every function, what each register can
hold, following every branch, jump table and call (a fixed point). A register holds
a value known exactly (a constant the code built with mov, movw, movt, add, orr and
the like), or a value known only to point into the SIO or IO_BANK0's 64 kB (derived
from such a constant and something else, or one of several such constants
depending on the path taken), or nothing known. Rules 1, 2 and 7 are checked at every
store whose address is known exactly; rules 3 and 7 fail every store whose address
is only known to be in those blocks. A store through a pointer derived from nothing in
those blocks (the stack, buffers, the ADC ring, the USB controller) is counted, not
checked. What this does not follow: an address loaded from memory (no code in this
firmware or in rustos's drivers keeps one there: their register addresses are
constants), code it cannot disassemble (it fails on any), and other bus masters:
the firmware programs one DMA channel, whose writes go to the ADC ring.

Usage: check_pins.py ELF
       check_pins.py --self-test

It needs llvm-objdump: the rustup toolchain's (component llvm-tools), or one on the
PATH.
"""

import contextlib
import functools
import heapq
import io
import os
import re
import shutil
import struct
import subprocess
import sys

SIO = 0xD000_0000
SIO_NONSEC = 0xD002_0000
GPIO_OUT_CLR = SIO + 0x020
FORBIDDEN = {}
for _bank, _view in ((SIO, ""), (SIO_NONSEC, " (its Non-secure mirror)")):
    for _off, _name in ((0x010, "GPIO_OUT"), (0x018, "GPIO_OUT_SET"), (0x028, "GPIO_OUT_XOR")):
        FORBIDDEN[_bank + _off] = f"SIO {_name}{_view}"
HELD = (1 << 16) | (1 << 17) | (1 << 18)
IO_BANK0 = 0x4002_8000
CTRL = {IO_BANK0 + 4 + 8 * pin: pin for pin in (16, 17, 18)}
for _at, _pin in list(CTRL.items()):
    for _off, _name in ((0x1000, "XOR"), (0x2000, "set"), (0x3000, "clear")):
        FORBIDDEN[_at + _off] = f"GPIO{_pin}_CTRL's atomic {_name} alias"
FUNCSEL_OK = {0x05, 0x1F}
OUTOVER_OK = {0x0, 0x2}
CPACR = {0xE000_ED88: "CPACR", 0xE002_ED88: "CPACR's Non-secure mirror"}
CP0 = 0x3
# The 64 kB blocks (address >> 16) no store may go into at an address not known.
WATCHED = {0x4002: "IO_BANK0's 64 kB", 0xD000: "the SIO", 0xD001: "the SIO",
           0xD002: "the SIO", 0xD003: "the SIO", 0xE000: "the Private Peripheral Bus",
           0xE002: "the Private Peripheral Bus's Non-secure mirror"}

CALLER_SAVED = ("r0", "r1", "r2", "r3", "r12", "lr")
ARGS = ("r0", "r1", "r2", "r3")
RETURNS = ("r0", "r1")
ALIAS = {"sb": "r9", "sl": "r10", "fp": "r11", "ip": "r12"}
CONDS = ("eq", "ne", "cs", "hs", "cc", "lo", "mi", "pl", "vs", "vc", "hi", "ls",
         "ge", "lt", "gt", "le", "al")
COND_BRANCHES = {"b" + c for c in CONDS}
# Every coprocessor instruction (rule 6), with or without a condition.
COPROCESSOR = re.compile(r"(mcrr?|mrrc|mrc|cdp|ldc|stc)2?l?(" + "|".join(CONDS) + r")?")
FPU_COPROCESSORS = {"p10", "p11"}
REGS = r"\b(r\d+|lr|sb|sl|fp|ip)\b"
# Two-operand forms that do not read their destination.
UNARY = {"mov", "movs", "movw", "mvn", "mvns", "uxth", "uxtb", "sxth", "sxtb", "clz",
         "rbit", "rev", "rev16", "revsh", "rrx", "rrxs", "adr"}
NO_EFFECT = {"cmp", "cmn", "tst", "teq", "nop", "yield", "dmb", "dsb", "isb", "bkpt",
             "udf", "clrex", "cpsid", "cpsie", "wfi", "wfe", "sev"}


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
        sys.exit("check_pins.py: no llvm-objdump (rustup component add llvm-tools)")
    return found


class Insn:
    def __init__(self, addr: int, op: str, args: str):
        self.addr, self.op, self.args = addr, op, args
        self.ops = [x.strip() for x in re.split(r",(?![^\[{]*[\]}])", args)] if args else []
        self.base = op.split(".")[0]
        t = re.search(r"\b0x([0-9a-f]+)\b", args)
        self.target = int(t.group(1), 16) if t else None

    def is_branch(self) -> bool:
        return self.base == "b" or self.base in COND_BRANCHES or self.base in ("cbz", "cbnz")

    def __repr__(self) -> str:
        return f"{self.op} {self.args} at {self.addr:#x}"


class Image:
    """What the check reads: each function's instructions, the exception handlers
    in the vector table, the symbols' names, and the image's bytes."""

    def __init__(self, funcs, vectors, names, read):
        self.funcs: dict[str, list[Insn]] = funcs
        self.vectors: list[int] = vectors
        self.names: dict[int, list[str]] = names
        self.read = read
        self.starts = {insns[0].addr: name for name, insns in funcs.items() if insns}


def parse(text: str) -> dict[str, list[Insn]]:
    """Each function's instructions, from llvm-objdump -d --no-show-raw-insn."""
    out: dict[str, list[Insn]] = {}
    current = None
    for line in text.splitlines():
        h = re.match(r"^([0-9a-f]+) <(.+)>:$", line)
        if h:
            current = out.setdefault(h.group(2), [])
            continue
        m = re.match(r"^\s*([0-9a-f]+):(.*)$", line)
        if not m or current is None:
            continue
        parts = m.group(2).split("\t")
        if len(parts) < 2:
            continue
        op = parts[1].strip()
        if op.startswith("."):
            continue  # data among the code: a jump table, read from the image instead
        args = parts[2].split("@")[0].strip() if len(parts) > 2 else ""
        current.append(Insn(int(m.group(1), 16), op, args))
    return out


@functools.cache
def reg(s: str) -> str | None:
    s = s.strip().lower().rstrip("!")
    s = ALIAS.get(s, s)
    return s if re.fullmatch(r"r\d+|lr", s) else None


@functools.cache
def imm(s: str) -> int | None:
    s = s.strip()
    if not s.startswith("#"):
        return None
    try:
        return int(s[1:], 0)
    except ValueError:
        return None


# What a register can hold, at one point: a set of possibilities, each a value
# known exactly (an int), a pointer known only to be into a watched block
# (("block", n), n = address >> 16), or anything (UNKNOWN). A register missing
# from a state can hold anything.
UNKNOWN = "?"
ANYTHING = frozenset({UNKNOWN})
# More exact values than this, and they are kept only as their blocks.
MAX_EXACT = 16


def blur(a):
    """A possibility, with an exact value kept only as its watched block."""
    if isinstance(a, int):
        return ("block", a >> 16) if a >> 16 in WATCHED else UNKNOWN
    return a


def bounded(atoms) -> frozenset:
    if len(atoms) <= MAX_EXACT or sum(isinstance(a, int) for a in atoms) <= MAX_EXACT:
        return frozenset(atoms)
    return frozenset(blur(a) for a in atoms)


def derived(*sets) -> frozenset:
    """A value computed from these in a way not followed here: whatever watched
    blocks they point into, or anything."""
    out = {blur(a) for s in sets for a in s}
    return frozenset(out - {UNKNOWN}) or ANYTHING


def combine(x: frozenset, y: frozenset, f) -> frozenset:
    if len(x) * len(y) > 64:
        x, y = frozenset(map(blur, x)), frozenset(map(blur, y))
    out = set()
    for a in x:
        for b in y:
            if isinstance(a, int) and isinstance(b, int):
                out.add(f(a, b) & 0xFFFF_FFFF)
            else:
                out |= derived({a}, {b})
    return bounded(out)


def mapped(x: frozenset, f) -> frozenset:
    out = set()
    for a in x:
        out |= {f(a) & 0xFFFF_FFFF} if isinstance(a, int) else derived({a})
    return bounded(out)


def watched(x: frozenset) -> frozenset:
    return frozenset(a for a in x if isinstance(a, tuple))


def join_state(a: dict, b: dict) -> dict:
    out = {}
    for r in a.keys() | b.keys():
        v = bounded(a.get(r, ANYTHING) | b.get(r, ANYTHING))
        if v != ANYTHING:
            out[r] = v
    return out


def shifted(x: frozenset, shift: str | None) -> frozenset:
    if not shift:
        return x
    m = re.fullmatch(r"(lsl|lsr|asr|ror) #(\w+)", shift.strip())
    if not m:
        return derived(x)
    kind, n = m.group(1), int(m.group(2), 0)
    if kind == "lsl":
        return mapped(x, lambda v: v << n)
    if kind == "lsr":
        return mapped(x, lambda v: v >> n)
    if kind == "asr":
        return mapped(x, lambda v: (v - (1 << 32) if v & 0x8000_0000 else v) >> n)
    return mapped(x, lambda v: (v >> n) | (v << (32 - n)))


ARITH = {
    "add": lambda x, y: x + y, "adds": lambda x, y: x + y, "addw": lambda x, y: x + y,
    "sub": lambda x, y: x - y, "subs": lambda x, y: x - y, "subw": lambda x, y: x - y,
    "rsb": lambda x, y: y - x, "rsbs": lambda x, y: y - x,
    "orr": lambda x, y: x | y, "orrs": lambda x, y: x | y, "orn": lambda x, y: x | ~y,
    "eor": lambda x, y: x ^ y, "eors": lambda x, y: x ^ y,
    "and": lambda x, y: x & y, "ands": lambda x, y: x & y,
    "bic": lambda x, y: x & ~y, "bics": lambda x, y: x & ~y,
    "lsl": lambda x, y: x << y if y < 32 else 0, "lsls": lambda x, y: x << y if y < 32 else 0,
    "lsr": lambda x, y: x >> y, "lsrs": lambda x, y: x >> y,
}


@functools.cache
def memory_operand(args: str) -> tuple[str | None, str | None, str | None] | None:
    """A load's or store's [base, offset, shift], or None if it has none."""
    m = re.search(r"\[(\w+)(?:,\s*([^\]]+))?\]", args)
    if not m:
        return None
    rn = m.group(1) if m.group(1) in ("sp", "pc") else reg(m.group(1))
    if not m.group(2):
        return rn, None, None
    off = [x.strip() for x in m.group(2).split(",")]
    return rn, off[0], off[1] if len(off) > 1 else None


def boundary(s: dict, regs) -> dict:
    """What passes into or out of a function: exact values outside the watched
    blocks are not kept, so that the buffers and constants calls pass do not
    multiply."""

    def keep(a):
        return a if isinstance(a, int) and a >> 16 in WATCHED else blur(a)

    out = {}
    for r in regs:
        v = bounded(frozenset(map(keep, s.get(r, ANYTHING))))
        if v != ANYTHING:
            out[r] = v
    return out


class Function:
    """The fixed point over one function: what each register can hold at each of
    its instructions."""

    def __init__(self, image: Image, name: str, rets: dict):
        self.image, self.name, self.rets = image, name, rets
        self.insns = image.funcs[name]
        self.index = {i.addr: k for k, i in enumerate(self.insns)}
        self.conditional = [False] * len(self.insns)
        left = 0
        for k, i in enumerate(self.insns):
            if left:
                self.conditional[k] = True
                left -= 1
            if re.fullmatch(r"it[te]{0,3}", i.op):
                left = len(i.op) - 1
        self.states: dict[int, dict] = {}
        self.calls: dict[str, dict] = {}
        self.ret: dict | None = None
        self.tails: set[str] = set()
        self.seeded = 0

    @staticmethod
    def value(s: dict, operand: str) -> frozenset:
        r = reg(operand)
        if r:
            return s.get(r, ANYTHING)
        k = imm(operand)
        return ANYTHING if k is None else frozenset({k & 0xFFFF_FFFF})

    def address(self, s: dict, i: Insn) -> tuple[str | None, frozenset]:
        """A load's or store's base register and what its address can be."""
        parts = memory_operand(i.args)
        if parts is None:
            return None, ANYTHING
        rn, off, shift = parts
        if rn in ("sp", "pc"):
            return rn, ANYTHING
        base = s.get(rn, ANYTHING)
        if off is None:
            return rn, base
        return rn, combine(base, shifted(self.value(s, off), shift), ARITH["add"])

    def writeback(self, s: dict, i: Insn, rn: str | None) -> None:
        """A pre- or post-indexed access's new base register."""
        if rn is None or rn in ("sp", "pc"):
            return
        post = re.search(r"\],\s*(#-?\w+)", i.args)
        pre = re.search(r"\[\w+,\s*(#-?\w+)\]!", i.args)
        if post or pre:
            self.put(s, rn, combine(s.get(rn, ANYTHING), self.value(s, (post or pre).group(1)), ARITH["add"]))

    @staticmethod
    def put(s: dict, r: str | None, v: frozenset) -> None:
        if r is None:
            return
        if v == ANYTHING:
            s.pop(r, None)
        else:
            s[r] = v

    def transfer(self, s: dict, i: Insn) -> dict:
        """The registers after `i`, if it executes."""
        s = dict(s)
        op, ops, b = i.op, i.ops, i.base
        if i.is_branch() or b in ("tbb", "tbh") or b.startswith("bx"):
            return s
        if b.startswith(("str", "stm", "vst", "push", "vpush")):
            if b.startswith("strex"):
                self.put(s, reg(ops[0]), ANYTHING)
            if b.startswith(("stm", "vstm")):
                if ops[0].endswith("!"):
                    rn = reg(ops[0])
                    self.put(s, rn, derived(s.get(rn, ANYTHING)))
            elif not b.startswith(("push", "vpush")):
                self.writeback(s, i, self.address(s, i)[0])
            return s
        if b.startswith(("ldm", "pop", "vldm", "vpop")):
            if b.startswith(("ldm", "pop")):
                for r in re.findall(REGS, i.args.split("{", 1)[-1]):
                    self.put(s, reg(r), ANYTHING)
            rn = reg(ops[0]) if b.startswith(("ldm", "vldm")) else None
            if rn and ops[0].endswith("!"):
                self.put(s, rn, derived(s.get(rn, ANYTHING)))
            return s
        if b.startswith(("ldr", "lda", "vldr")):
            self.writeback(s, i, self.address(s, i)[0])
            if not b.startswith("vldr"):
                for r in ops[:2] if b.startswith("ldrd") else ops[:1]:
                    self.put(s, reg(r), ANYTHING)
            return s
        if b in ("bl", "blx"):
            for r in CALLER_SAVED:
                s.pop(r, None)
            callee = self.image.starts.get(i.target) if b == "bl" else None
            if callee:
                # Until the callee is seen to return, nothing: its callers are
                # solved again when it is.
                ret = self.rets.get(callee)
                for r in RETURNS:
                    s[r] = ret.get(r, ANYTHING) if ret is not None else frozenset()
                    self.put(s, r, s[r])
            return s
        if b in NO_EFFECT or re.fullmatch(r"it[te]{0,3}", op):
            return s
        if b.startswith("v"):
            # Floating point: only a move to core registers, or vmrs, writes one.
            for o in ops:
                if not reg(o):
                    break
                s.pop(reg(o), None)
            return s
        if b in ("umull", "umlal", "smull", "smlal", "umaal"):
            for o in ops[:2]:
                self.put(s, reg(o), ANYTHING)
            return s
        rd = reg(ops[0]) if ops else None
        if rd is None:
            # Nothing else here is known to write a register; forget any it names.
            for r in re.findall(REGS, i.args):
                self.put(s, reg(r), ANYTHING)
            return s
        self.put(s, rd, self.compute(s, i))
        return s

    def compute(self, s: dict, i: Insn) -> frozenset:
        """What `i` can leave in its first operand."""
        b, ops = i.base, i.ops

        def v(o):
            return self.value(s, o)

        if b == "movw" and len(ops) == 2:
            return v(ops[1])
        if b == "movt" and len(ops) == 2 and imm(ops[1]) is not None:
            hi = imm(ops[1]) << 16
            out = set()
            for a in v(ops[0]):
                out |= {hi | (a & 0xFFFF)} if isinstance(a, int) else {blur(hi)}
            return bounded(out)
        if b in ("mov", "movs") and len(ops) == 2:
            return v(ops[1])
        if b in ("mvn", "mvns") and len(ops) == 2:
            return mapped(v(ops[1]), lambda x: ~x)
        if b in ("uxth", "uxtb") and len(ops) == 2:
            return mapped(v(ops[1]), lambda x: x & (0xFFFF if b == "uxth" else 0xFF))
        if b in ARITH and len(ops) in (2, 3, 4):
            o = ops if len(ops) != 2 else [ops[0], ops[0], ops[1]]
            if len(o) == 3 or re.fullmatch(r"(lsl|lsr|asr|ror) #\w+", o[3]):
                return combine(v(o[1]), shifted(v(o[2]), o[3] if len(o) == 4 else None), ARITH[b])
        reads_rd = b in ("bfi", "bfc", "movt") or (len(ops) == 2 and b not in UNARY)
        return derived(*[v(o) for o in ops[1:] + (ops[:1] if reads_rd else []) if reg(o)])

    def successors(self, k: int) -> tuple[list[int], bool]:
        """The instructions that can follow insns[k], and whether it can leave the
        function."""
        i, b = self.insns[k], self.insns[k].base
        nxt = [k + 1] if k + 1 < len(self.insns) else []
        cond = self.conditional[k]
        if i.is_branch():
            inside = [self.index[i.target]] if i.target in self.index else []
            may_fall = b != "b" or cond
            return inside + (nxt if may_fall else []), not inside
        if b in ("tbb", "tbh"):
            return [self.index[t] for t in table(self.image, i) if t in self.index], False
        leaves = (b.startswith("bx") or (b.startswith(("pop", "ldm")) and re.search(r"\bpc\b", i.args))
                  or (i.ops[:1] == ["pc"]))
        if leaves:
            return (nxt if cond else []), True
        return nxt, False

    def args(self, s: dict) -> dict:
        return boundary(s, ARGS)

    def solve(self, entry: dict) -> None:
        self.states, self.calls, self.ret, self.tails, self.seeded = {}, {}, None, set(), 0
        if not self.insns:
            return
        self.states[0] = dict(entry)
        work = [0]
        while True:
            while work:
                k = heapq.heappop(work)
                s, i = self.states[k], self.insns[k]
                after = self.transfer(s, i)
                if self.conditional[k]:
                    after = join_state(s, after)
                succ, leaves = self.successors(k)
                callee = self.image.starts.get(i.target) if i.target is not None else None
                if i.base == "bl" and callee:
                    self.calls[callee] = join_state(self.calls.get(callee, {}), self.args(s))
                if leaves and i.is_branch() and callee:
                    # A tail call: its arguments go on, and its return value is this
                    # function's.
                    self.tails.add(callee)
                    self.calls[callee] = join_state(self.calls.get(callee, {}), self.args(s))
                elif leaves and not i.is_branch():
                    out = boundary(after, RETURNS)
                    self.ret = out if self.ret is None else join_state(self.ret, out)
                for j in succ:
                    old = self.states.get(j)
                    new = after if old is None else join_state(old, after)
                    if new != old:
                        self.states[j] = new
                        heapq.heappush(work, j)
            # Code no path followed here reaches (after a jump through a register):
            # checked from nothing known.
            missed = next((k for k in range(len(self.insns)) if k not in self.states), None)
            if missed is None:
                return
            self.seeded += 1
            self.states[missed] = {}
            work = [missed]


def table(image: Image, i: Insn) -> list[int]:
    """The targets of a tbb or tbh jump table, which starts right after it and ends
    where its first target starts."""
    size = 1 if i.base == "tbb" else 2
    start = i.addr + 4
    targets, at, first = [], start, None
    while (first is None or at < first) and len(targets) < 4096:
        raw = image.read(at, size)
        if raw is None:
            break
        t = start + 2 * int.from_bytes(raw, "little")
        at += size
        if t >= at:  # else padding after a table of odd length
            targets.append(t)
            first = t if first is None else min(first, t)
    return targets


def solve_image(image: Image) -> dict[str, Function]:
    """Every function's fixed point, with what calls pass between them. A function
    can be entered with anything in its argument registers (a caller this cannot
    see, through a pointer), or what any call seen here passes."""
    rets: dict[str, dict] = {}
    entry = {name: {} for name in image.funcs}
    fns = {name: Function(image, name, rets) for name in image.funcs}
    callers: dict[str, set[str]] = {name: set() for name in image.funcs}
    for name, insns in image.funcs.items():
        for i in insns:
            if (i.base == "bl" or i.is_branch()) and i.target in image.starts:
                callers[image.starts[i.target]].add(name)
    # Callees before their callers, so that most calls find what they return.
    rank: dict[str, int] = {}
    callees = {name: set() for name in image.funcs}
    for callee, cs in callers.items():
        for c in cs:
            callees[c].add(callee)
    for root in image.funcs:
        stack = [(root, iter(sorted(callees[root])))]
        if root in rank:
            continue
        rank[root] = -1
        while stack:
            name, it = stack[-1]
            nxt = next((c for c in it if c not in rank), None)
            if nxt is None:
                stack.pop()
                rank[name] = len(rank)
            else:
                rank[nxt] = -1
                stack.append((nxt, iter(sorted(callees[nxt]))))
    work = [(rank[name], name) for name in image.funcs]
    heapq.heapify(work)
    queued = set(image.funcs)

    def queue(name):
        if name not in queued:
            heapq.heappush(work, (rank[name], name))
            queued.add(name)

    while work:
        _, name = heapq.heappop(work)
        queued.discard(name)
        f = fns[name]
        f.solve(entry[name])
        for callee, args in f.calls.items():
            new = join_state(entry[callee], args)
            if new != entry[callee]:
                entry[callee] = new
                queue(callee)
        ret = f.ret
        for t in f.tails:
            if t in rets:
                ret = dict(rets[t]) if ret is None else join_state(ret, rets[t])
        if ret is not None and name in rets:
            ret = join_state(rets[name], ret)
        if ret is not None and ret != rets.get(name):
            rets[name] = ret
            for c in callers[name]:
                queue(c)
    return fns


def check_image(image: Image) -> tuple[list[str], int, list[tuple[str, str]], int]:
    """Rules 1-3 at every store. Returns the failures, the stores not checked, the
    holds (function, where) and the functions with code no followed path reaches."""
    failures, unknown, holds, seeded = [], 0, [], 0
    for name, f in solve_image(image).items():
        seeded += f.seeded > 0
        for k, i in enumerate(f.insns):
            where = f"{i} in {name}"
            if i.op == "<unknown>":
                failures.append(f"an instruction llvm-objdump cannot decode: {where}")
            elif i.base.startswith("ldr") and re.search(r"\[pc\b", i.args):
                failures.append(f"a load from a literal pool, which this check does not follow: {where}")
            elif COPROCESSOR.fullmatch(i.base) and not (i.ops and i.ops[0] in FPU_COPROCESSORS):
                failures.append(f"a coprocessor instruction, which can drive GPIO_OUT with no store "
                                f"(coprocessor 0): {where}")
            elif i.base.startswith(("str", "stm", "vst")):
                unknown += check_store(f, k, i, where, failures, holds)
    return failures, unknown, holds, seeded


def check_store(f: Function, k: int, i: Insn, where: str, failures: list, holds: list) -> int:
    """Rules 1-3 and 7 at the store `i`, the `k`th instruction of `f`: 1 if it can go
    to an address not known, which is not checked, else 0."""
    s = f.states.get(k, {})
    b, ops = i.base, i.ops
    if b.startswith(("stm", "vstm")):
        if ops[0].rstrip("!") == "sp":
            return 0
        base = s.get(reg(ops[0]), ANYTHING)
        regs = re.findall(r"\b(r\d+|lr|sb|sl|fp|ip|[sd]\d+)\b", ops[1] if len(ops) > 1 else "")
        width = 8 if regs and regs[0].startswith("d") else 4
        for a in base:
            if isinstance(a, int):
                start = a - width * len(regs) if "db" in i.op else a
                for n, r in enumerate(regs):
                    value = s.get(reg(r), ANYTHING) if reg(r) else ANYTHING
                    store_at(f, start + width * n, frozenset({start + width * n}), value, width,
                             where, failures, holds)
        return unchecked(base, where, failures)
    rn, at = f.address(s, i)
    if rn == "sp":
        return 0
    values = [ops[1]] if b.startswith("strex") else ops[:2] if b.startswith("strd") else ops[:1]
    if b.startswith("strh"):
        size = 2
    elif b.startswith("strb"):
        size = 1
    elif b.startswith("vstr") and ops[0].startswith("d"):
        size = 8
    else:
        size = 4
    for a in at:
        if isinstance(a, int):
            for n, val in enumerate(values):
                value = s.get(reg(val), ANYTHING) if reg(val) else ANYTHING
                fpu = n == 0 and b == "str" and a & ~3 in CPACR and enable_fpu(f, k, a)
                store_at(f, a + 4 * n, at, value, size, where, failures, holds, fpu)
    return unchecked(at, where, failures)


def unchecked(at: frozenset, where: str, failures: list) -> int:
    for _, block in sorted(watched(at)):
        failures.append(f"a store into {WATCHED[block]} at an address not known here: {where}")
    return int(UNKNOWN in at)


def store_at(f: Function, at: int, every: frozenset, value: frozenset, size: int, where: str,
             failures: list, holds: list, fpu: bool = False) -> None:
    """Rules 1, 2 and 7 at one address the store can go to (of `every` it can).
    `fpu`: the store is rustos's enable_fpu, which rule 7 passes."""
    for word in range(at & ~3, at + size, 4):
        if word in FORBIDDEN:
            failures.append(f"a store to {FORBIDDEN[word]} ({word:#x}): {where}")
        if word in CPACR and not fpu:
            if size != 4 or at & 3 or not value or not all(isinstance(x, int) for x in value):
                failures.append(f"a store to {CPACR[word]} of a value not known here, which "
                                f"could turn the GPIO coprocessor on: {where}")
            for x in sorted(x for x in value if isinstance(x, int)):
                if x & CP0:
                    failures.append(f"{CPACR[word]} set to {x:#x}, which turns the GPIO "
                                    f"coprocessor (CP0) on: {where}")
    if at & ~3 in CTRL:
        pin = CTRL[at & ~3]
        if size != 4 or at & 3 or not all(isinstance(x, int) for x in value):
            failures.append(f"a store to GPIO{pin}_CTRL of a value not known here: {where}")
        for x in sorted(x for x in value if isinstance(x, int)):
            if x & 0x1F not in FUNCSEL_OK or (x >> 12) & 3 not in OUTOVER_OK:
                failures.append(
                    f"GPIO{pin}_CTRL set to {x:#x} (FUNCSEL {x & 0x1f:#x}, "
                    f"OUTOVER {(x >> 12) & 3}), which can drive it high: {where}"
                )
    if every == {GPIO_OUT_CLR} and value and all(isinstance(x, int) and x & HELD == HELD for x in value):
        holds.append((f.name, where))


def enable_fpu(f: Function, k: int, at: int) -> bool:
    """Whether the str at `f.insns[k]`, to CPACR at `at`, is rustos's enable_fpu:
    `ldr rV, [rA]` from that CPACR, `orr rV, rV, #c` with c's CP0 bits clear, then
    the store of rV, with no other instruction naming rV, no condition, no branch
    and nothing branched to in between. Anything else is not."""
    targets = {i.target for i in f.insns if i.is_branch()}
    rv = reg(f.insns[k].ops[0])
    want = [("orr", lambda i: (len(i.ops) == 3 and reg(i.ops[1]) == rv or len(i.ops) == 2)
             and imm(i.ops[-1]) is not None and imm(i.ops[-1]) & CP0 == 0),
            ("ldr", lambda i: f.address(f.states.get(f.index[i.addr], {}), i)[1] == {at}
             and re.fullmatch(r"\[\w+(,\s*#0)?\]", i.args.split(",", 1)[1].strip()) is not None)]
    j = k
    for op, ok in want:
        while True:
            if f.insns[j].addr in targets:
                return False
            j -= 1
            if j < 0:
                return False
            i = f.insns[j]
            if (f.conditional[j] or i.is_branch() or re.fullmatch(r"it[te]{0,3}", i.op)
                    or i.base.startswith(("bl", "bx", "tb", "pop", "ldm"))):
                return False
            if rv in {reg(r) for r in re.findall(REGS, i.args)}:
                break
        if i.base != op or reg(i.ops[0]) != rv or not ok(i):
            return False
    return True


def first_store_holds(image: Image, symbol: str, depth: int = 0) -> tuple[bool, str]:
    """Whether the first store `symbol` makes, in a straight line from its first
    instruction, drives GP16-GP18 low, and why not."""
    if symbol not in image.funcs:
        return False, f"no {symbol} in the image"
    regs: dict[str, int] = {}
    for i in image.funcs[symbol]:
        op, b = i.op, i.base
        if op == "<unknown>":
            return False, f"an instruction llvm-objdump cannot decode: {i} in {symbol}"
        if b.startswith(("str", "stm", "vst")):
            m = re.search(r"\[(\w+)(?:,\s*(#-?\w+))?\]$", i.args)
            if m and m.group(1) == "sp":
                continue
            if b == "str" and m and reg(m.group(1)) in regs:
                at = regs[reg(m.group(1))] + (imm(m.group(2)) if m.group(2) else 0)
                val = regs.get(reg(i.ops[0]) or "")
                if at == GPIO_OUT_CLR and val is not None and val & HELD == HELD:
                    return True, f"{i} in {symbol}"
            return False, f"a store before GP16-GP18 are driven low: {i} in {symbol}"
        if b in ("bl", "blx"):
            callee = image.starts.get(i.target) if b == "bl" else None
            if not callee or depth >= 2:
                return False, f"a call it cannot follow before GP16-GP18 are low: {i} in {symbol}"
            ok, why = first_store_holds(image, callee, depth + 1)
            if ok or not why.startswith("returns"):
                return ok, why
            for r in CALLER_SAVED:
                regs.pop(r, None)
            continue
        if depth > 0 and ((b == "bx" and i.args == "lr") or (b.startswith("pop") and "pc" in i.args)):
            return False, f"returns: {i}"
        if (i.is_branch() or b.startswith(("bx", "tb", "pop", "ldm")) or b in ("bkpt", "udf")
                or re.fullmatch(r"it[te]{0,3}", op)):
            return False, f"leaves the straight line before GP16-GP18 are low: {i} in {symbol}"
        if b.startswith(("push", "vpush")) or (b == "add" and i.ops[:2] == ["r7", "sp"]):
            continue
        if b in ("movw", "mov", "movs") and len(i.ops) == 2 and reg(i.ops[0]) and imm(i.ops[1]) is not None:
            regs[reg(i.ops[0])] = imm(i.ops[1])
            continue
        if b == "movt" and reg(i.ops[0]) in regs and imm(i.ops[1]) is not None:
            r = reg(i.ops[0])
            regs[r] = (imm(i.ops[1]) << 16) | (regs[r] & 0xFFFF)
            continue
        for r in re.findall(REGS, i.args):
            regs.pop(reg(r), None)
    return False, f"{symbol} ends before GP16-GP18 are driven low"


def panic_symbol(image: Image) -> str:
    unwind = [n for n in image.funcs if n.endswith("rust_begin_unwind")]
    if len(unwind) == 1:
        return unwind[0]
    fmt = [n for n in image.funcs if re.search(r"panicking9panic_fmt|panicking::panic_fmt", n)]
    if len(fmt) != 1:
        sys.exit("check_pins.py: cannot find the panic handler")
    return fmt[0]


def run(image: Image) -> list[str]:
    """Every failure, printing what passed."""
    failures = []
    handlers = {}
    for v in image.vectors:
        name = image.starts.get(v & ~1)
        if name is None:
            failures.append(f"an exception vector points at {v & ~1:#x}, not at a function")
        else:
            handlers[name] = "/".join(sorted(image.names.get(v & ~1, [name])))
    if not handlers:
        failures.append("no exception handlers in the vector table")
    panic = panic_symbol(image)
    found, unknown, holds, seeded = check_image(image)
    failures += found
    loop_holds = [w for name, w in holds if name not in handlers and name != panic]
    if loop_holds:
        print(f"GP16-GP18 driven low by {len(loop_holds)} store(s) outside the handlers, "
              f"e.g. {loop_holds[0]}")
    else:
        failures.append(
            "no store driving GP16-GP18 low (bits 16-18 to SIO GPIO_OUT_CLR) found outside "
            "the handlers: the check cannot follow this code"
        )
    print(f"{unknown} stores through pointers derived from no SIO or IO_BANK0 address "
          "(the stack, buffers), not checked")
    if seeded:
        print(f"{seeded} functions with code no followed path reaches (after a jump through "
              "a register), checked from nothing known")
    for name, label in sorted(handlers.items()) + [(panic, "panic")]:
        ok, why = first_store_holds(image, name)
        if ok:
            print(f"{label}: drives GP16-GP18 low with its first store ({why})")
        else:
            failures.append(f"{label}: does not drive GP16-GP18 low first: {why}")
    return failures


def elf_image(path: str) -> Image:
    tool = objdump()
    text = subprocess.run(
        [tool, "-d", "--no-show-raw-insn", "--mcpu=cortex-m33", path],
        capture_output=True, text=True, check=True,
    ).stdout
    syms = subprocess.run([tool, "-t", path], capture_output=True, text=True, check=True).stdout
    names: dict[int, list[str]] = {}
    for line in syms.splitlines():
        m = re.match(r"^([0-9a-f]{8}) .* F \S+\s+[0-9a-f]+ (\S+)$", line)
        if m:
            names.setdefault(int(m.group(1), 16) & ~1, []).append(m.group(2))
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF" or data[4] != 1 or data[5] != 1:
        sys.exit(f"check_pins.py: {path} is not a 32-bit little-endian ELF")
    (shoff,) = struct.unpack_from("<I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x2E)
    headers = [struct.unpack_from("<IIIIII", data, shoff + k * shentsize) for k in range(shnum)]
    strtab = headers[shstrndx][4]
    sections, loaded = {}, []
    for name_off, kind, flags, addr, offset, size in headers:
        name = data[strtab + name_off:data.index(b"\0", strtab + name_off)].decode()
        sections[name] = (addr, size)
        if kind == 1 and flags & 2:  # SHT_PROGBITS, SHF_ALLOC
            loaded.append((addr, offset, size))

    def read(addr: int, n: int) -> bytes | None:
        for a, off, size in loaded:
            if a <= addr and addr + n <= a + size:
                return data[off + addr - a:off + addr - a + n]
        return None

    if ".vector_table" not in sections:
        sys.exit("check_pins.py: no .vector_table section")
    addr, size = sections[".vector_table"]
    words = struct.unpack(f"<{size // 4}I", read(addr, size))
    # Word 0 is the initial stack pointer and word 1 the reset handler; 0 is a
    # reserved slot.
    vectors = sorted({w for w in words[2:] if w})
    return Image(parse(text), vectors, names, read)


def self_test() -> None:
    """The check against small made-up images, so that it is seen to fail."""
    hold = "movs r0, #0x20\nmov.w r1, #0x70000\nmovt r0, #0xd000\nstr r1, [r0]\n"

    def image(body: str, handlers: str | None = None, extra: dict | None = None,
              data: dict | None = None, vectors: tuple = ()) -> Image:
        """main is the hold and then `body`, from 0x1000; each line 4 bytes, or
        `@addr` to move on to addr. `@name@` in a line is that function's address."""
        handlers = handlers if handlers is not None else hold + "b 0x0\n"
        funcs = [("main", hold + body), ("OnHardFault", handlers),
                 ("core::panicking::panic_fmt", handlers)] + list((extra or {}).items())
        starts, addr = {}, 0x1000
        for name, code in funcs:
            starts[name] = addr
            addr += 0x100 * (1 + code.count("\n") // 64)
        text = ""
        for name, code in funcs:
            addr = starts[name]
            text += f"{addr:08x} <{name}>:\n"
            for line in code.strip().splitlines():
                if line.startswith("@") and not line.endswith("@"):
                    addr = int(line[1:], 16)
                    continue
                for other, at in starts.items():
                    line = line.replace(f"@{other}@", f"{at:#x} <{other}>")
                op, _, args = line.partition(" ")
                text += f"{addr:08x}:      \t{op}\t{args}\n"
                addr += 4
        mem = data or {}

        def read(a, n):
            return bytes(mem[a + k] for k in range(n)) if all(a + k in mem for k in range(n)) else None

        names = {at: [name] for name, at in starts.items()}
        names[starts["OnHardFault"]] = ["DefaultHandler", "OnHardFault"]
        vec = [starts["OnHardFault"] | 1] + [starts[v] | 1 for v in vectors]
        return Image(parse(text), vec, names, read)

    def failures(img: Image) -> list[str]:
        with contextlib.redirect_stdout(io.StringIO()):
            return run(img)

    def fails(img: Image, why: str) -> None:
        got = failures(img)
        assert any(why in g for g in got), f"expected a failure with {why!r}, got {got}"

    def passes(img: Image) -> None:
        got = failures(img)
        assert got == [], got

    passes(image("movw r4, #0x80cc\nmovt r4, #0x4002\nmovs r2, #0x5\nstr r2, [r4, #-72]\n"))
    # A pointer from memory is counted, not failed.
    passes(image("ldr r0, [r2]\nstr r1, [r0]\n"))
    not_known = "into the SIO at an address not known"
    cases = {
        "SIO GPIO_OUT_SET (": "movs r0, #0x18\nmovt r0, #0xd000\nmovs r1, #1\nstr r1, [r0]\n",
        "SIO GPIO_OUT_SET (0x": "movs r0, #0x0\nmovt r0, #0xd000\nstr r1, [r0, #0x18]\n",
        "SIO GPIO_OUT_XOR": "mov.w r0, #0xd0000000\nadds r0, #0x28\nstr r1, [r0]\n",
        "SIO GPIO_OUT (": "movs r0, #0x10\nmovt r0, #0xd000\nstr r1, [r0]\n",
        "Non-secure mirror": "movs r0, #0x18\nmovt r0, #0xd002\nstr r1, [r0]\n",
        "GPIO18_CTRL set to 0x4": "movw r0, #0x8094\nmovt r0, #0x4002\nmovs r1, #4\nstr r1, [r0]\n",
        "GPIO16_CTRL set to 0x3005": "movw r0, #0x8084\nmovt r0, #0x4002\nmovw r1, #0x3005\nstr r1, [r0]\n",
        "GPIO17_CTRL of a value not known": "movw r0, #0x808c\nmovt r0, #0x4002\nldr r1, [r2]\nstr r1, [r0]\n",
        "GPIO16_CTRL's atomic set alias": "movw r0, #0xa084\nmovt r0, #0x4002\nmovs r1, #0x5\nstr r1, [r0]\n",
        "GPIO16_CTRL of a value not known":
            "movw r0, #0x8084\nmovt r0, #0x4002\nmovs r1, #5\nit ne\nmovne r1, #4\nstr r1, [r0]\n",
        # A pin's write(level): GPIO_OUT_SET or GPIO_OUT_CLR, chosen at run time.
        not_known: "movs r0, #0x20\nit ne\nmovne r0, #0x18\nmovt r0, #0xd000\nstr r1, [r0]\n",
        # A pin number known only at run time.
        "into IO_BANK0's 64 kB at an address not known":
            "movw r0, #0x8004\nmovt r0, #0x4002\nldr r2, [r3]\nstr r1, [r0, r2, lsl #3]\n",
        "an instruction llvm-objdump cannot decode": "<unknown>\n",
        "a load from a literal pool": "ldr r0, [pc, #0x10]\nstr r1, [r0]\n",
    }
    for why, body in cases.items():
        fails(image(body), why)
    # An address only the branch back brings: the loop's second time round.
    fails(image("movs r4, #0\nstr r1, [r4]\nldr r2, [r3]\nmovs r4, #0\nmovt r4, #0xd000\n"
                "add r4, r2\nb 0x1014\n"), not_known)
    # Through calls: an address in an argument, in a return value, or passed on by
    # a tail call.
    helper = {"helper": "str r1, [r0]\nbx lr\n"}
    out_set = "a store to SIO GPIO_OUT_SET"
    fails(image("movs r0, #0x18\nmovt r0, #0xd000\nbl @helper@\n", extra=helper), out_set)
    fails(image("movs r0, #0x18\nmovt r0, #0xd000\nb @helper@\n", extra=helper), out_set)
    fails(image("bl @helper@\nstr r1, [r0]\n",
                extra={"helper": "movs r0, #0x18\nmovt r0, #0xd000\nbx lr\n"}), out_set)
    # A helper called with GPIO_OUT_CLR from one place and an unknown pointer from
    # another passes: each address it can get is checked, or counted.
    passes(image("movs r0, #0x20\nmovt r0, #0xd000\nbl @helper@\nldr r0, [r2]\nbl @helper@\n",
                 extra=helper))
    # Code reached only through a jump table: tbb at 0x1020, its table at 0x1024
    # (entries 2 and 4: targets 0x1028 and 0x102c), then the two cases.
    body = ("ldr r2, [r3]\nmovs r0, #0\nmovt r0, #0xd000\nadd r0, r2\ntbb [pc, r5]\n"
            "@1028\nb 0x2000\nstr r1, [r0]\n")
    fails(image(body, data={0x1024: 2, 0x1025: 4}), not_known)
    # Handlers that store elsewhere first, branch first, or drive the wrong bits.
    for handlers, why in [("movs r0, #0\nstr r0, [r1]\nb 0x0\n", "a store before"),
                          ("b 0x0\n", "leaves the straight line"),
                          ("movs r0, #0x20\nmov.w r1, #0x30000\nmovt r0, #0xd000\nstr r1, [r0]\nb 0x0\n",
                           "a store before")]:
        fails(image("", handlers), why)
    # Every handler in the vector table is checked, whatever its name.
    fails(image("", extra={"SysTick": "movs r0, #0\nstr r0, [r1]\nbx lr\n"}, vectors=("SysTick",)),
          "SysTick: does not drive")
    # The GPIO coprocessor drives GPIO_OUT with no store (rule 6); the FPU's
    # instructions pass.
    for insn in ("mcr p0, #0x1, r0, c0, c0", "mcr2 p0, #0x0, r0, c0, c0",
                 "mcrr p0, #0x0, r0, r1, c0", "mrc p0, #0x0, r0, c0, c0",
                 "cdp p4, #0x0, c0, c0, c0, #0x0", "ldc p0, c0, [r1]", "stc p0, c0, [r1]",
                 "it ne\nmcrne p0, #0x1, r0, c0, c0"):
        fails(image(insn + "\n"), "a coprocessor instruction")
    passes(image("vmov s0, r1\nvadd.f32 s0, s0, s1\nmcr p10, #0x7, r0, c1, c0, #0x0\n"))
    # CPACR (rule 7): rustos's read-modify-write and a known value without CP0 pass.
    cpacr = "movw r0, #0xed88\nmovt r0, #0xe000\n"
    passes(image(cpacr + "ldr r1, [r0]\norr r1, r1, #0xf00000\nstr r1, [r0]\n"))
    passes(image(cpacr + "mov.w r1, #0xf00000\nstr r1, [r0]\n"))
    fails(image(cpacr + "movs r1, #0x3\nstr r1, [r0]\n"), "CPACR set to 0x3")
    fails(image("movw r0, #0xed88\nmovt r0, #0xe002\nmovs r1, #0x1\nstr r1, [r0]\n"),
          "CPACR's Non-secure mirror set to 0x1")
    not_known_cpacr = "a store to CPACR of a value not known"
    for body in (
        # The orr sets CP0.
        "ldr r1, [r0]\norr r1, r1, #0xf00003\nstr r1, [r0]\n",
        # The value is loaded from somewhere else.
        "ldr r1, [r2]\norr r1, r1, #0xf00000\nstr r1, [r0]\n",
        # Something else changes it between the orr and the store.
        "ldr r1, [r0]\norr r1, r1, #0xf00000\nadds r1, #0x1\nstr r1, [r0]\n",
        # A branch lands on the store with another value.
        "ldr r1, [r0]\norr r1, r1, #0xf00000\nstr r1, [r0]\nldr r1, [r2]\nb 0x1020\n",
        # Under a condition.
        "ldr r1, [r0]\nit ne\norrne r1, r1, #0xf00000\nstr r1, [r0]\n",
    ):
        fails(image(cpacr + body), not_known_cpacr)
    fails(image("movw r0, #0xed00\nmovt r0, #0xe000\nldr r2, [r3]\nstr r1, [r0, r2]\n"),
          "into the Private Peripheral Bus at an address not known")
    # No hold outside the handlers fails: the panic handler's does not count.
    img = image("")
    img.funcs["main"] = []
    fails(img, "no store driving GP16-GP18 low")
    print("self-test passed")


def main() -> None:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    if len(sys.argv) != 2:
        sys.exit("usage: check_pins.py ELF | --self-test")
    failures = run(elf_image(sys.argv[1]))
    for f in failures:
        print(f"FAIL: {f}")
    if failures:
        sys.exit(1)
    print("GP16-GP18: never driven high, and driven low first by every handler")


if __name__ == "__main__":
    main()
