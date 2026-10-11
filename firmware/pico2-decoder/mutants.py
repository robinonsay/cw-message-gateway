#!/usr/bin/env python3
"""Show that check_pins.py fails real builds of this firmware that drive the keyer
box's pins: build the committed firmware seven times, each changed in one way that
could key a radio, and check that check_pins.py rejects every one.

1. GP16 set high at boot (rustos's `write(true)`: a store to GPIO_OUT_SET).
2. GP16 following the decoder's tone (`write` with a level known only at run
   time: GPIO_OUT_SET or GPIO_OUT_CLR, chosen as it runs).
3. GP18 given its PWM function.
4. The fault and panic handlers driving the LED before GP16-GP18.
5. A pin's function set to PWM through a pin number known only at run time.
6. GP16 toggled through the GPIO coprocessor (`mcr p0`), with no store at all.
7. The GPIO coprocessor turned on at boot (CP0's bits set in CPACR).

It builds what is committed (`git archive` of COMMIT, default HEAD, unpacked in a
temporary directory), never the working tree, and leaves the checkout as it was.
It takes a few minutes; CI does not run it.

Usage: python3 mutants.py [COMMIT]
"""

import os
import subprocess
import sys
import tempfile

KEY = "let _key = gpio.output_from_handle(board.pins.gpio16).unwrap();"
MUT_KEY = "let mut _key = gpio.output_from_handle(board.pins.gpio16).unwrap();"
LOOP = "        control.pass(&mut hw);\n"
SAFE = "fn safe_state() {\n    outputs_low();\n}"
MUTANTS = {
    "GP16 high at boot": [(KEY, MUT_KEY + "\n    let _ = api::common::Write::write(&mut _key, true);")],
    "GP16 follows the tone": [
        (KEY, MUT_KEY),
        (LOOP, LOOP + "        let _ = api::common::Write::write(&mut _key, control.decoder().key_down());\n"),
    ],
    "GP18 to PWM": [(KEY, KEY + "\n    unsafe { (0x4002_8094 as *mut u32).write_volatile(4) };")],
    "handlers drive the LED first": [
        (SAFE, "fn safe_state() {\n    Overridden::<LED_GPIO>::drive(true);\n    outputs_low();\n}")
    ],
    "a pin number known at run time, to PWM": [
        (LOOP, LOOP + "        let pin = (control.samples() as usize) % 3 + 16;\n"
               "        unsafe { ((0x4002_8004 + 8 * pin) as *mut u32).write_volatile(4) };\n")
    ],
    "GP16 toggled through the GPIO coprocessor": [
        (LOOP, LOOP + "        unsafe { core::arch::asm!(\"mcr p0, #1, {0}, c0, c0\", "
               "in(reg) ((control.samples() as u32) & 1) << 16) };\n")
    ],
    "the GPIO coprocessor turned on": [
        (KEY, KEY + "\n    unsafe { (0xe000_ed88 as *mut u32).write_volatile(0x00f0_0003) };")
    ],
}


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    repo = subprocess.run(["git", "-C", here, "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True, check=True).stdout.strip()
    commit = sys.argv[1] if len(sys.argv) > 1 else "HEAD"
    survived = []
    with tempfile.TemporaryDirectory(prefix="pico2-decoder-mutants-") as tmp:
        archive = subprocess.run(["git", "-C", repo, "archive", commit], capture_output=True, check=True)
        subprocess.run(["tar", "-x", "-C", tmp], input=archive.stdout, check=True)
        fw = os.path.join(tmp, "firmware", "pico2-decoder")
        src = os.path.join(fw, "src", "main.rs")
        with open(src) as f:
            original = f.read()
        elf = os.path.join(fw, "target", "thumbv8m.main-none-eabihf", "release", "pico2-decoder")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("RUSTFLAGS", "CARGO_"))}
        for name, edits in MUTANTS.items():
            text = original
            for old, new in edits:
                if text.count(old) != 1:
                    sys.exit(f"mutants.py: {name}: the code it changes is not there")
                text = text.replace(old, new)
            with open(src, "w") as f:
                f.write(text)
            build = subprocess.run(["cargo", "build", "--release", "--locked"], cwd=fw, env=env,
                                   capture_output=True, text=True)
            if build.returncode:
                sys.exit(f"mutants.py: {name}: does not build:\n{build.stderr[-2000:]}")
            check = subprocess.run([sys.executable, os.path.join(here, "check_pins.py"), elf],
                                   capture_output=True, text=True)
            fails = [line for line in check.stdout.splitlines() if line.startswith("FAIL")]
            if check.returncode and fails:
                print(f"{name}: rejected: {fails[0]}")
            else:
                print(f"{name}: NOT rejected")
                survived.append(name)
    if survived:
        sys.exit(f"check_pins.py passed {len(survived)} of the changed builds")
    print(f"check_pins.py rejected all {len(MUTANTS)} changed builds")


if __name__ == "__main__":
    main()
