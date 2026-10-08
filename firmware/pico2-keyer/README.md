# Keyer box firmware (Raspberry Pi Pico 2)

The firmware for the keyer box: a Pico 2 that keys any radio's key jack for
hfnode (`station.rig = "keyer"`). How to wire, flash and bring it up is in
[docs/keyer.md](../../docs/keyer.md); the lines it takes over USB are in
[docs/keyer-protocol.md](../../docs/keyer-protocol.md).

- `src/main.rs`: the box. Its rules and limits are not here but in
  [crates/keyer-core](../../crates/keyer-core), which hfnode's tests and
  `hfnode selftest` run as a mock box; this file moves USB bytes and the key pin.
- It runs on [rustos](https://github.com/robinonsay/rustos), a bare-metal Rust
  runtime for the Pico 2 with no outside crates: its clocks, timer, SysTick,
  watchdog, GPIO, PWM and USB serial drivers, and the fault handlers, which call
  this firmware's `safe_state` (the key open first) before they stop.
- `uf2.py`: turns the built ELF file into `pico2-keyer.uf2`, the file you copy onto
  the Pico 2, after checking that the RP2350 would boot it.
- `check_faults.py`: reads the built ELF's HardFault, default exception and panic
  handlers and checks that the first store each makes opens the key (GP16). No
  computer can run those handlers, so CI checks the machine code instead. It needs
  `llvm-objdump`, which `rust-toolchain.toml` installs (`llvm-tools`):

  ```sh
  python3 check_faults.py target/thumbv8m.main-none-eabihf/release/pico2-keyer
  ```

## Getting it

GitHub builds it on every push: the CI workflow's `pico2-keyer-firmware` artifact
holds `pico2-keyer.uf2`. Which run to take it from, and what to check it against,
is in [docs/keyer.md](../../docs/keyer.md), "Flashing the firmware".

## Building it yourself

From a checkout of the commit you want, with `rustup` installed (it fetches the
compiler `rust-toolchain.toml` pins):

```sh
cd firmware/pico2-keyer
KEYER_BUILD_ID="$(git rev-parse --short=8 HEAD)" cargo build --release --locked
python3 uf2.py target/thumbv8m.main-none-eabihf/release/pico2-keyer pico2-keyer.uf2
```

This is CI's build, and the UF2 comes out byte for byte the same as CI's for that
commit, so its SHA-256 is the one CI and the safety audit publish. Without
`KEYER_BUILD_ID` the box reports `-` as its build, and the file has another checksum.

It builds only for the Pico 2, so it is its own Cargo workspace, outside the
hfnode one (`.cargo/config.toml` sets the target).

To build against a local rustos checkout:

```sh
cargo build --release \
  --config 'patch."https://github.com/robinonsay/rustos".api.path="../../../rustos/api"' \
  --config 'patch."https://github.com/robinonsay/rustos".pico2.path="../../../rustos/firmware/pico2"'
```
