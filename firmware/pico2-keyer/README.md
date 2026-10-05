# Keyer box firmware (Raspberry Pi Pico 2)

The firmware for the keyer box: a Pico 2 that keys any radio's key jack for
hfnode (`station.rig = "keyer"`). How to wire, flash and bring it up is in
[docs/keyer.md](../../docs/keyer.md); the lines it takes over USB are in
[docs/keyer-protocol.md](../../docs/keyer-protocol.md).

- `src/main.rs`: the box. Its rules and limits are not here but in
  [crates/keyer-core](../../crates/keyer-core), which hfnode's tests and
  `hfnode selftest` run as a mock box; this file moves USB bytes and the key pin.
- It runs on [rustos](https://github.com/robinonsay/rustos), a bare-metal Rust
  runtime for the Pico 2 with no outside crates: its clocks, timer, watchdog, GPIO
  and USB serial drivers.
- `uf2.py`: turns the built ELF file into `pico2-keyer.uf2`, the file you copy onto
  the Pico 2, after checking that the RP2350 would boot it.

## Getting it

GitHub builds it on every push: the CI workflow's `pico2-keyer-firmware` artifact
holds `pico2-keyer.uf2`.

## Building it yourself

```sh
rustup target add thumbv8m.main-none-eabihf
cd firmware/pico2-keyer
cargo build --release
python3 uf2.py target/thumbv8m.main-none-eabihf/release/pico2-keyer pico2-keyer.uf2
```

It builds only for the Pico 2, so it is its own Cargo workspace, outside the
hfnode one (`.cargo/config.toml` sets the target).

To build against a local rustos checkout:

```sh
cargo build --release \
  --config 'patch."https://github.com/robinonsay/rustos".api.path="../../../rustos/api"' \
  --config 'patch."https://github.com/robinonsay/rustos".pico2.path="../../../rustos/firmware/pico2"'
```
