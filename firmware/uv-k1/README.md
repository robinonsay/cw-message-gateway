# UV-K1 firmware with hfnode control

The NR7Y CW firmware for the Quansheng UV-K1 and UV-K5 v3
([briand/uv-k1-k5v3-firmware-custom](https://github.com/briand/uv-k1-k5v3-firmware-custom),
docs at <https://briand.github.io/cw-firmware-docs/>), with commands added so that
`hfnode` can key it over the radio's USB-C port. The commands and how the node uses
them are in [docs/handheld-protocol.md](../../docs/handheld-protocol.md); setting the
radio up and the bring-up are in [docs/handheld.md](../../docs/handheld.md). Not for
the older UV-K5 (v1 and v2), which has a different processor (from memory).

**Nothing here has run on a radio yet, and this version has not been built.** An
earlier one, before the stop check (3 below) was added, was built once (94,216 bytes,
in the radio's 118 KB of program flash). The command and safety logic is tested on a
computer against a simulated radio (below).

## What is here

| File | |
|---|---|
| `app/hfnode.c`, `app/hfnode.h` | The commands and the limits below. |
| `app/hfnode_line.c`, `app/hfnode_line.h` | The line format: framing and checksums. |
| `nr7y-hfnode.patch` | The small changes to the firmware's own files: start and poll hfnode from the main loop, feed the watchdog from the 10 ms tick, a keyer entry that plays given text at a given speed, and five Morse characters the keyer lacked (`' ) : " @`). |
| `build.sh` | Fetches the firmware at the commit the patch was made for, applies it and builds. |
| `test/` | The host test, with stand-ins for the radio's code. |

Everything is built in only with `ENABLE_HFNODE`, which needs the CW mod and USB;
without it the firmware is the stock NR7Y build. Licensed Apache-2.0, like the
firmware.

## How it keeps the transmitter from sticking

The radio's own transmit time-out timer does not work in CW: the firmware clears it
on every key-down. So these limits are the firmware's own, all on the radio, none
depending on the computer. They run on a clock of their own, counted from the 10 ms
tick, not on the timer the keyer uses, so that a fault in one does not stop both:

1. **A run limit.** No `CW` command keys for more than 60 s, whatever its text and
   speed.
2. **A link timeout.** While a `CW` command is sending, a valid line must arrive from
   `hfnode` at least every 2 s; otherwise the firmware stops it. A crashed computer,
   a killed `hfnode` or a pulled cable ends the transmission within 2 s.
3. **Every stop is checked.** After a stop (by `STOP` or by a limit), the transmitter
   must read off within 0.5 s, from the BK4819 radio chip's own register as well as
   the firmware's state, and stay off for the rest of a second: a held or stuck
   paddle keys the radio again after a stop, and is stopped again each time. If it
   reads on 0.5 s or more after the stop, the firmware stops feeding the watchdog,
   which resets the radio about 2 s later; more `STOP`s from `hfnode` do not put
   that off. A `STOP` that finds the transmitter on in CW outside a run is checked
   the same way.
4. **A hardware watchdog.** While a `CW` command is under way, the firmware stops
   feeding the watchdog once its main loop has stopped for a second (a hang), and
   the watchdog resets the radio about 2 s later: about 3 s in all. Outside one, it
   is fed from the 10 ms tick, which a hard fault stops. A reset should turn the
   transmitter off: the radio chip, which also switches the power amplifier, keeps
   transmitting through the processor's reset until the start-up code resets it.
   That comes early in the start-up code, but how long the bootloader takes first is
   not known. `hfnode handheld hangtest` checks it on your radio.
5. **A key-down budget.** Time in runs adds to it and time out of them takes from
   it; past 165 s, `CW` is refused until it has come down. A computer that keeps
   sending gets about half the time on the air.
6. **The transmitter's state is checked.** If a run's text goes out without the
   BK4819 ever reading transmitting, the stop checks could not see a stuck
   transmitter, so no more `CW` is taken until the radio is restarted.
7. **Keying only for `CW`.** The firmware transmits for `hfnode` only while sending
   the text of a `CW` command it accepted.
8. **Nothing set.** No command sets anything on the radio: frequency, mode, power
   and break-in are read, never written, so `hfnode` cannot change what you set at
   the radio, and nothing is written to its memory.

Not done: the start-up code resets the BK4819 only after the display's start-up
delays (about 0.2 s, from the code). Resetting it straight after the pins are set
up (`BOARD_Init` in `board.c`, after `BOARD_GPIO_Init`) would end a carrier left on
through a reset that much sooner. It is a change to the firmware's own start-up,
not yet made here.

Commands come only over USB-C. The headset jack's serial line (the firmware's
programming port, left as it is) shares a wire with the PTT, so `hfnode`'s commands
are not taken there.

## Building

It needs git, CMake 3.22 or later, Ninja, Python 3 and the Arm GNU toolchain
(`arm-none-eabi-gcc` with its C library). The earlier version was built on Ubuntu
24.04 with arm-none-eabi-gcc 13.2; the Mac steps have not been tried.

On a Mac (Homebrew):

```
brew install cmake ninja python
brew install --cask gcc-arm-embedded
firmware/uv-k1/build.sh
```

On Debian or Ubuntu:

```
sudo apt install git cmake ninja-build python3 gcc-arm-none-eabi libnewlib-arm-none-eabi
firmware/uv-k1/build.sh
```

It fetches the upstream firmware at the commit the patch was made for, and writes
`firmware/uv-k1/build/nr7y.cw.hfnode.bin`. The build directory is build.sh's own (it
resets and cleans the checkout in it on every build) and is ignored by git; delete it
to start again.

## Flashing

From memory, not checked against the NR7Y docs (their flashing page wins where they
differ):

1. Use [UVTools2](https://armel.github.io/uvtools2/) in Chrome or Edge on the
   computer (it uses Web Serial).
2. Turn the radio off. Hold the PTT and turn it on: it starts in flashing mode, with
   the screen blank and the flashlight LED on. Connect the USB-C cable.
3. In UVTools2, choose the flash tool, the radio's port and `nr7y.cw.hfnode.bin`,
   and flash. Then turn the radio off and on.
4. Its start-up screen and menus are the NR7Y firmware's. `hfnode handheld check`
   should find it as `NR7Y-CW HFNODE`.

To go back, flash the plain NR7Y CW release the same way. Flashing only works from
that power-on flashing mode, so a radio running this firmware can always be
reflashed.

Don't use CHIRP, UVTools2's other tools or anything else on the radio's USB-C port
while `hfnode` is running: they share the port.

## The host test

From `firmware/uv-k1`:

```
cc -std=c11 -Wall -Wextra -Werror -I test/stubs -I . \
   test/test_hfnode.c app/hfnode_line.c -o test_hfnode && ./test_hfnode
```

It compiles `app/hfnode.c` against stand-ins for the radio (`test/stubs`), and runs
the line format, every command, and each limit above: the run limit, the link
timeout, a stop that fails (with `STOP` sent again and again), a paddle that keys
again after a stop, a mode change mid-run, the key-down budget, a transmitter that
never reads on, the watchdog with the main loop running and stopped, and the hang
test. CI runs it on every push. The stand-ins
behave as the firmware's code was read to behave (`CW_EndTxNow` ends the
transmission, the BK4819's transmit bit clears on receive); the bring-up checks that
on the radio.
