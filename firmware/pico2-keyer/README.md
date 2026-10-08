# Keyer box firmware (Raspberry Pi Pico 2)

The firmware for the keyer box: a Pico 2 that keys any radio's key jack, or an FM
handheld's PTT and microphone, for hfnode (`station.rig = "keyer"`). How to wire,
flash and bring it up is in [docs/keyer.md](../../docs/keyer.md); the lines it
takes over USB are in [docs/keyer-protocol.md](../../docs/keyer-protocol.md).

- `src/main.rs`: the box. Its rules and limits are not here but in
  [crates/keyer-core](../../crates/keyer-core), which hfnode's tests and
  `hfnode selftest` run as a mock box; this file moves USB bytes, the key, PTT and
  tone pins, and the PTT line.
- It runs on [rustos](https://github.com/robinonsay/rustos), a bare-metal Rust
  runtime for the Pico 2 with no outside crates: its clocks, timer, SysTick,
  watchdog, GPIO, PWM and USB serial drivers, and the fault handlers, which call
  this firmware's `safe_state` (the key and the PTT open first) before they stop.
  It is pinned in `Cargo.toml` at a commit on a rustos branch until its PRs
  merge, then at their merge commits.
- `build.sh`: builds a commit's UF2 the way CI does (below, "Building it yourself").
- `uf2.py`: turns the built ELF file into `pico2-keyer.uf2`, the file you copy onto
  the Pico 2, after checking that the RP2350 would boot it.
- `check_faults.py`: reads the built ELF's HardFault, default exception and panic
  handlers and checks that the first store each makes opens the key (GP16). No
  computer can run those handlers, so CI checks the machine code instead. It needs
  `llvm-objdump`, which `rust-toolchain.toml` installs (`llvm-tools`):

  ```sh
  python3 check_faults.py pico2-keyer.elf
  ```

## Getting it

GitHub builds it on every push: the CI workflow's `pico2-keyer-firmware` artifact
holds `pico2-keyer.uf2`. Which run to take it from, and what to check it against,
is in [docs/keyer.md](../../docs/keyer.md), "Flashing the firmware".

## Building it yourself

From a checkout of the repository, with `git`, `python3` and `rustup` installed
(rustup fetches the compiler `rust-toolchain.toml` pins), on Linux:

```sh
sh firmware/pico2-keyer/build.sh            # HEAD, or name a commit: build.sh 759f01b6
```

This is CI's build. It builds the commit as committed (`git archive`, so nothing
uncommitted gets in), at one fixed path, `/tmp/pico2-keyer-build`, with the
commit's first eight characters as `KEYER_BUILD_ID`, and writes `pico2-keyer.uf2`,
its `pico2-keyer.uf2.sha256` and `pico2-keyer.elf` into `firmware/pico2-keyer`. The
UF2 comes out byte for byte the same as CI's for that commit, so its SHA-256 is
the one CI and the safety audit publish. CI builds each commit this way twice, from
checkouts in two places, and fails if the two files differ. That is all on Linux:
on a Mac, `build.sh` has never been compared with CI's build. A match there shows
the download is what the commit builds to; a difference shows nothing about the
download. Either way, the file to flash is CI's, checked against the hash the
audit publishes ([docs/keyer.md](../../docs/keyer.md), "Flashing the firmware").

A plain `cargo build` does not: keyer-core is outside this crate's workspace, so
cargo puts its absolute path into the hash in every symbol name, and the code's
layout follows those names. The same commit built in place in two checkouts can
give two different UF2s. That is fine for development: `cargo build --release`
here builds the working tree, and without `KEYER_BUILD_ID` the box reports `-` as
its build.

It builds only for the Pico 2, so it is its own Cargo workspace, outside the
hfnode one (`.cargo/config.toml` sets the target).

To build against a local rustos checkout:

```sh
cargo build --release \
  --config 'patch."https://github.com/robinonsay/rustos".api.path="../../../rustos/api"' \
  --config 'patch."https://github.com/robinonsay/rustos".pico2.path="../../../rustos/firmware/pico2"'
```
