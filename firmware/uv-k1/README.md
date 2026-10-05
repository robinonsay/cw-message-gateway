# UV-K1 firmware with hfnode control

The NR7Y CW firmware for the Quansheng UV-K1 and UV-K5 v3
([briand/uv-k1-k5v3-firmware-custom](https://github.com/briand/uv-k1-k5v3-firmware-custom),
docs at <https://briand.github.io/cw-firmware-docs/>), with commands added so that
`hfnode` can key it over the radio's USB-C port. The commands and how the node uses
them are in [docs/handheld-protocol.md](../../docs/handheld-protocol.md); setting the
radio up and the bring-up are in [docs/handheld.md](../../docs/handheld.md).

**Nothing here has run on a radio yet.** It has been built once (94,216 bytes, in the
radio's 118 KB of program flash) and its command and safety logic is tested on a
computer against a simulated radio (below).

## What is here

| File | |
|---|---|
| `app/hfnode.c`, `app/hfnode.h` | The commands, the transmit limit, the link timeout and the watchdog. |
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
depending on the computer:

1. **A run limit.** No `CW` command keys for more than 60 s, whatever its text and
   speed.
2. **A link timeout.** While a `CW` command is sending, a valid line must arrive from
   `hfnode` at least every 2 s; otherwise the firmware stops it. A crashed computer,
   a killed `hfnode` or a pulled cable ends the transmission within 2 s.
3. **Every stop is checked.** After a stop (by `STOP` or by a limit), the transmitter
   must read off within 0.5 s, from the BK4819 radio chip's own register, not just
   the firmware's state. If it does not, the firmware stops feeding the watchdog,
   which resets the radio within about 2 s.
4. **A hardware watchdog.** While a `CW` command is under way, the watchdog resets
   the radio if the firmware's main loop stops for a second (a hang or a crash). A
   reset turns the transmitter off: the start-up code resets the radio chip within
   milliseconds of power-up. `hfnode handheld hangtest` proves this on your radio.
5. **Keying only for `CW`.** No command sets anything on the radio: frequency, mode,
   power and break-in are read, never written, so `hfnode` cannot change what you
   set at the radio, and nothing is written to its memory.

Commands come only over USB-C. The headset jack's serial line shares a wire with the
PTT, so the firmware does not listen there.

## Building

On a Mac (Homebrew):

```
brew install cmake ninja python
brew install --cask gcc-arm-embedded
firmware/uv-k1/build.sh
```

On Linux: `git cmake ninja-build python3 gcc-arm-none-eabi` from the package manager,
then `firmware/uv-k1/build.sh`.

It writes `firmware/uv-k1/build/nr7y.cw.hfnode.bin`. The build directory is ignored by
git; delete it to start again.

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
timeout, a stop that fails, a mode change mid-run, the watchdog with the main loop
running and stopped, and the hang test. CI runs it on every push. The stand-ins
behave as the firmware's code was read to behave (`CW_EndTxNow` ends the
transmission, the BK4819's transmit bit clears on receive); the bring-up checks that
on the radio.
