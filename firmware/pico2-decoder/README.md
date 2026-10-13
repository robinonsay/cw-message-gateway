# Decoder box firmware (Raspberry Pi Pico 2)

The firmware for the decoder box: a Pico 2 that listens to a radio's headphone audio
on GP26 and sends the Morse it hears as letters over USB serial. How to wire and use
it is in [docs/decoder.md](../../docs/decoder.md); the lines it sends and takes are
in [docs/decoder-protocol.md](../../docs/decoder-protocol.md).

- `src/main.rs`: the board. Everything the box does is in
  [crates/decoder-core](../../crates/decoder-core), which its tests run against a
  board in memory; this file connects it to rustos's USB, ADC, DMA, timer and
  watchdog drivers, and holds the keyer box's key, PTT and tone pins (GP16-GP18)
  low.
- It runs on [rustos](https://github.com/robinonsay/rustos), as the keyer box's
  firmware does, pinned in `Cargo.toml` at a commit on a rustos branch until its
  ADC and DMA drivers merge, then at their merge commit.
- `build.sh`: builds a commit's UF2 the way CI does: the commit as committed, at
  one fixed path, `/tmp/pico2-decoder-build`, with the commit's first eight
  characters as `DECODER_BUILD_ID`. It writes `pico2-decoder.uf2`, its `.sha256`
  and `pico2-decoder.elf` here. CI builds each commit twice, from checkouts in two
  places, and fails if the two UF2s differ (on Linux; not yet compared on a Mac).
  Why a plain `cargo build` differs: [the keyer's README](../pico2-keyer/README.md),
  "Building it yourself".
- `check_pins.py`: reads the built ELF's machine code and checks that no store
  can drive GP16-GP18 high (no SIO `GPIO_OUT`, `GPIO_OUT_SET` or `GPIO_OUT_XOR`,
  only the SIO or the null function on their pins, no store into the SIO or the
  GPIO registers at an address worked out at run time), that the loop drives them
  low, and that every exception handler in the vector table and the panic handler
  drive them low first. Its docstring says how and what it does not cover. It
  needs `llvm-objdump`, which `rust-toolchain.toml` installs (`llvm-tools`):

  ```sh
  python3 check_pins.py --self-test
  python3 check_pins.py pico2-decoder.elf
  ```

- `mutants.py`: shows `check_pins.py` failing real builds that drive the pins. It
  builds the committed firmware five times, each changed one way (GP16 set high
  at boot, GP16 following the decoder's tone, GP18 given its PWM function, the
  fault handlers driving the LED first, a pin's function set through a pin number
  known only at run time), and checks that `check_pins.py` rejects every one. It
  takes a few minutes, so CI does not run it.

It builds only for the Pico 2, so it is its own Cargo workspace, outside the hfnode
one (`.cargo/config.toml` sets the target).
