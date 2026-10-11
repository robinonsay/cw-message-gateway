#!/usr/bin/env python3
"""Show that check_core1.py fails real builds of this firmware whose core 1 could
reach a pin: build the committed firmware six times, each with core 1's code
changed in one way, and check that check_core1.py rejects every one.

1. `TEST CORE1 FAULT` stores to GPIO_OUT_SET instead of GPIO_OUT_CLR: the key down.
2. Core 1's board writes GP16's pad on every pass.
3. Core 1 stores to the SIO at an offset from a byte the host sent.
4. Core 1 writes the PWM's tone slice.
5. Core 1 leaves the GPIO coprocessor on (CPACR's CP0 bits set).
6. Core 1 sets the key through the GPIO coprocessor, with no store.

It builds what is committed (`git archive` of COMMIT, default HEAD, unpacked in a
temporary directory), never the working tree, and leaves the checkout as it was.
It takes a few minutes; CI does not run it.

Usage: python3 mutants_core1.py [COMMIT]
"""

import os
import subprocess
import sys
import tempfile

FAULT = "const SIO_GPIO_OUT_CLR: *mut u32 = 0xd000_0020 as *mut u32;"
HOLD = "    fn hold_outputs_low(&mut self) {}"
CPACR = "    const CP10_CP11_FULL: u32 = (0b11 << 20) | (0b11 << 22);"
READ = "    fn usb_read(&mut self, buf: &mut [u8]) -> usize {\n        LINK.0.to_core1.read(buf)\n    }"
MUTANTS = {
    "TEST CORE1 FAULT sets the key": [(FAULT, FAULT.replace("0xd000_0020", "0xd000_0018"))],
    "GP16's pad on every pass": [
        (HOLD, "    fn hold_outputs_low(&mut self) {\n"
               "        unsafe { (0x4003_8044 as *mut u32).write_volatile(0x56) };\n    }")
    ],
    "the SIO at an offset the host sent": [
        (READ, "    fn usb_read(&mut self, buf: &mut [u8]) -> usize {\n"
               "        let n = LINK.0.to_core1.read(buf);\n"
               "        if n > 0 {\n"
               "            let at = 0xd000_0000usize + 4 * buf[0] as usize;\n"
               "            unsafe { (at as *mut u32).write_volatile(1 << 16) };\n"
               "        }\n"
               "        n\n    }")
    ],
    "the PWM's tone slice": [
        (HOLD, "    fn hold_outputs_low(&mut self) {\n"
               "        unsafe { (0x400a_8020 as *mut u32).write_volatile(0x8000_8000) };\n    }")
    ],
    "the GPIO coprocessor left on": [(CPACR, CPACR.replace(";", " | 0b11;"))],
    "the key through the GPIO coprocessor": [
        (HOLD, "    fn hold_outputs_low(&mut self) {\n"
               "        unsafe { core::arch::asm!(\"mcr p0, #4, {0}, c0, c0\", in(reg) 1u32 << 16) };\n    }")
    ],
}


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    repo = subprocess.run(["git", "-C", here, "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True, check=True).stdout.strip()
    commit = sys.argv[1] if len(sys.argv) > 1 else "HEAD"
    survived = []
    with tempfile.TemporaryDirectory(prefix="pico2-keyer-mutants-") as tmp:
        archive = subprocess.run(["git", "-C", repo, "archive", commit], capture_output=True, check=True)
        subprocess.run(["tar", "-x", "-C", tmp], input=archive.stdout, check=True)
        fw = os.path.join(tmp, "firmware", "pico2-keyer")
        src = os.path.join(fw, "src", "core1.rs")
        with open(src) as f:
            original = f.read()
        elf = os.path.join(fw, "target", "thumbv8m.main-none-eabihf", "release", "pico2-keyer")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("RUSTFLAGS", "CARGO_"))}
        for name, edits in MUTANTS.items():
            text = original
            for old, new in edits:
                if text.count(old) != 1:
                    sys.exit(f"mutants_core1.py: {name}: the code it changes is not there")
                text = text.replace(old, new)
            with open(src, "w") as f:
                f.write(text)
            build = subprocess.run(["cargo", "build", "--release", "--locked"], cwd=fw, env=env,
                                   capture_output=True, text=True)
            if build.returncode:
                sys.exit(f"mutants_core1.py: {name}: does not build:\n{build.stderr[-2000:]}")
            check = subprocess.run([sys.executable, os.path.join(fw, "check_core1.py"), elf],
                                   capture_output=True, text=True)
            fails = [line for line in check.stdout.splitlines() if line.startswith("FAIL")]
            if check.returncode and fails:
                print(f"{name}: rejected: {fails[0]}")
            else:
                print(f"{name}: NOT rejected")
                survived.append(name)
    if survived:
        sys.exit(f"check_core1.py passed {len(survived)} of the changed builds")
    print(f"check_core1.py rejected all {len(MUTANTS)} changed builds")


if __name__ == "__main__":
    main()
