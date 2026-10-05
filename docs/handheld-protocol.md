# Handheld CW firmware: serial command set, version 1

How `hfnode` keys a handheld (`station.rig = "handheld"`, [handheld.md](handheld.md)):
the commands added to the NR7Y CW firmware for the Quansheng UV-K1 and UV-K5 v3 in
[firmware/uv-k1](../firmware/uv-k1/README.md) (`app/hfnode.c`). The node's side is
`crates/hfnode/src/handheld/proto.rs` and `handheld.rs`; `handheld/mock.rs` is a
simulated firmware that keeps the rules here, except that it answers `CW` at once
and keeps no key-down budget, and the node's tests run against it.
The firmware's own host test (`firmware/uv-k1/test`) checks the firmware against the
same rules.

The firmware keys the radio's carrier itself, with its own keyer, from text the node
sends: the same split as the IC-7300, whose internal keyer sends the text the node
gives it over CI-V. The node never holds the transmitter on by itself, and the
firmware sets nothing on the radio: frequency, mode, power and break-in are set by
hand at the radio and only read here.

## Link

- The radio's USB-C port, which is a virtual serial port (USB CDC): its speed and
  framing settings mean nothing. `[handheld] baud` is ignored there.
- The node holds DTR and RTS down for as long as the port is open; the firmware does
  not use them.
- The headset jack's serial line (the firmware's programming port) does not take
  these commands: it shares a wire with the PTT (the jack's wiring is from memory).

## Lines

Every line, either way, is printable ASCII (0x20 to 0x7E) ending in `\n`; a `\r`
before it is ignored. At most 80 characters before the `\n`.

```
<id> <body>*<cs>
```

- `<id>`: two uppercase hex digits, `01` to `FF`. The reply repeats the command's id,
  so that a late reply to an earlier command is never taken for this one's.
- `<body>`: the command, or the reply.
- `<cs>`: two uppercase hex digits, the XOR of every byte from the start of the line
  up to, not including, the `*`. `01 STATUS*35` and `01 HELLO*63` are correct lines.

A line whose checksum does not match, or that is malformed in any way, is ignored
completely: no reply, nothing done, and it does not count as a line received for the
link timeout below. The node waits 500 ms for each reply, and sends a command again
up to twice when its reply does not come, except `CW` and `TEST HANG`.

## Commands

| Command | Reply | What it does |
|---|---|---|
| `HELLO` | `OK HELLO <version> <tx limit s> <link timeout ms> <uptime ms> <name>` | Who the firmware is: `OK HELLO 1 60 2000 5230 NR7Y-CW HFNODE`. `tx limit`: the longest any one `CW` run may last (the node accepts 1 to 60). `link timeout`: the node accepts 1000 to 3000. `uptime`: how long since the firmware started, by its own clock (the radio's start-up before that not counted); the hang test uses it. `name` may contain spaces. |
| `STATUS` | `OK STATUS <tx> <quiet ms>` | `tx` is `1` from a `CW` being accepted until its text has gone out and the transmitter reads off, or a stop has turned it off; and while the transmitter is on for any other reason (its PTT pressed). Read from the radio chip (the BK4819's transmit bit) as well as the firmware's state. `quiet`: milliseconds since the squelch was last open (someone else on the frequency), or since the firmware started, `0` while it is open, at most `60000`; the squelch tail just after the radio's own transmission is not counted. |
| `FREQ` | `OK FREQ <rx Hz> <tx Hz>` | The frequencies the radio receives and would transmit on. |
| `MODE` | `OK MODE <tx> <rx>` | The transmit and receive modes: `CW`, `FM`, `AM`, `USB`, ... or `OTHER`. |
| `POWER` | `OK POWER <level>` | The transmit power level: `LOW1` to `LOW5`, `MID`, `HIGH`, `USER` or `OTHER`. |
| `BREAKIN` | `OK BREAKIN <0 or 1>` | Whether break-in is on. Without it the keyer only sounds the sidetone. |
| `CW <wpm> <text>` | `OK CW` | Key `text` at `wpm` (5 to 50), with standard Morse timing, then return to receive. Answered once keying has begun, within 300 ms; `ERR CW REFUSED` if it has not by then. `text`: 1 to 30 characters from `A-Z 0-9 . , ? ' / ( ) : = + - " @` and space (no lower case). |
| `STOP` | `OK STOP` | Stop keying at once, mid-element if need be. Accepted at any time, keying or not. A `CW` not yet answered is answered `ERR CW STOP` first. `STATUS` reads `tx` 1 until the transmitter reads off; the firmware then watches it for a second (below). |
| `TEST HANG` | `OK TEST HANG` | Bring-up only (`hfnode handheld hangtest`): while a `CW` is keying, stop the firmware's main loop for good, as a hang would. Only the watchdog can end the transmission then. |

Errors are `ERR <command> <code>`:

| Code | Meaning |
|---|---|
| `LEN` | `CW` text empty or over 30 characters. |
| `WPM` | Speed outside 5-50. |
| `CHAR` | A character outside the set above. |
| `MODE` | `CW` with the radio not in CW, or the radio switched out of CW before keying began. |
| `BKIN` | `CW` with break-in off. |
| `CHECK` | `CW` after a run whose text went out without the radio chip ever reading transmitting: the firmware could not see a stuck transmitter, so it takes no more `CW` until the radio is switched off and on. |
| `DUTY` | `CW` with the key-down budget used up (below). |
| `WAIT` | `CW` within a second of a stop, while the firmware watches the transmitter. The node tries again. |
| `TX` | `CW` while transmitting, recording or playing a CW memory; or a limit ended it before keying began. |
| `REFUSED` | `CW` accepted, but the radio did not key within 300 ms (read from the firmware's code): its `TxLock` setting for the channel, its busy-channel lock with someone on the frequency, a frequency it does not transmit on, a low battery, or the paddle or key in use. |
| `STOP` | `CW` stopped by `STOP` before keying began. |
| `RUN` | `TEST HANG` with no `CW` keying. |
| `UNKNOWN` | Not a command of this version (any setting command, too: nothing is set over the link). |

## What the firmware does

These are what make the handheld safe to leave to the node. The node checks the
first two in `HELLO` and refuses firmware that reports anything looser. The radio's
own transmit time-out timer does not work in CW (the firmware clears it on every
key-down), so these are the only limits on the radio. They are timed by the
firmware's own clock, counted from its 10 ms tick, not by the timer its keyer uses,
so that a fault in one does not stop both the keyer and its limits.

1. **A transmit limit.** No `CW` run lasts longer than the `tx limit` it reports
   (60 s), whatever the text and speed.
2. **A link timeout.** While a `CW` run lasts, if no valid line has arrived for the
   `link timeout` it reports (2 s), it stops the run. This is what ends a
   transmission when the computer crashes, `hfnode` is killed, or the cable is
   pulled.
3. **Every stop is checked.** After a stop by `STOP` or by a limit, the transmitter
   must read off within 0.5 s and stay off for the rest of a second; a held or stuck
   paddle keys it again, and is stopped again each time. If it reads on 0.5 s or
   more after the stop, the firmware stops feeding its watchdog, which resets the
   radio about 2 s later, and does not feed it again even if the transmitter then
   goes off. Further `STOP`s do not put this off. A `STOP` that finds the transmitter
   on in CW outside a run (keyed by hand, or by a paddle after a run) is checked the
   same way; one in another mode is the operator's, and left alone. `CW` is answered
   `ERR CW WAIT` during that second.
4. **A watchdog.** While a `CW` run lasts, the firmware stops feeding the hardware
   watchdog once the main loop has stopped for a second, and the watchdog resets the
   radio about 2 s later: about 3 s in all. Outside a run it is fed from the 10 ms
   tick; a hard fault stops that tick, and so resets the radio too. A reset should
   turn the transmitter off: the radio chip keeps transmitting through the
   processor's reset until the start-up code resets it, which it does after the
   display's start-up (about 0.2 s, from the code), and how long the bootloader
   takes before that is not known. `hfnode handheld hangtest` checks it on the
   radio.
5. **A key-down budget.** Time in `CW` runs adds to it, time out of them takes from
   it, and `CW` is answered `ERR CW DUTY` while it is over 165 s: back to back runs
   for that long, then about half the time. The node's duty cycle stays within it.
6. **The transmitter's state is checked.** A run whose text went out with the CW
   engine transmitting, but without the radio chip ever reading transmitting, means
   the firmware cannot see the transmitter; it then answers `CW` with `ERR CW CHECK`
   until the radio is restarted.
7. **Keying only for `CW`.** The firmware never transmits for the node except while
   sending the text of an accepted `CW`.
8. **`STATUS` tells the truth** about the transmitter, read from the radio chip as
   well as the firmware's state.
9. **Damaged lines do nothing** (above).
10. **Nothing is set or written**: no frequency, mode or power change, and nothing
    written to the radio's memory.

## What the node does

So that the firmware's timing is known:

- At start: `HELLO`, `STOP`, `STATUS` (must read `tx` 0). Then `STOP` again, and
  `MODE`, `FREQ`, `POWER` and `BREAKIN`, which must read CW, the configured frequency
  simplex, the configured power and `1`; if any does not, the node says what to
  change at the radio and does not transmit. Before every transmission, and every
  few minutes while it listens: `STATUS`, the same reads again, and `FREQ` again.
- Before keying: `STATUS`, and it waits for `quiet` to reach `busy_quiet_ms`.
- A `CW` answered `ERR CW WAIT` is sent again every 100 ms for up to 1.5 s. The
  node leaves out spaces at either end of a `CW`'s text.
- A transmission goes out as `CW` runs of at most 30 characters, one at a time. After
  each, once its text should have gone out, the node reads `STATUS` every 100 ms
  until `tx` reads 0, and only then sends the next. A run read back as ended well
  before its text could have gone out (cut short by a limit) fails the transmission.
- Until a run reads ended, or the node stops it (below), a keep-alive `STATUS` goes
  out every quarter of the link timeout or every 250 ms, whichever is more often,
  besides the reads above.
- The node stops a run that goes on too long: 2 s past the length of its text at its
  speed, or `max_key_seconds` plus 5 s, whichever is sooner. It sends `STOP`, stops
  the keep-alives for that run, and fails the transmission. If `STATUS` still reads
  `tx` 1, it latches its transmit inhibit, and sends `STOP` again once
  `max_key_seconds` have passed since the run began and at each periodic check;
  meanwhile the firmware's check of its own stop (above) resets the radio. If the
  link is cut, or the node dies, nothing arrives, and the link timeout ends the
  run.

## Example

Start-up, one `CW` run, and (`0A`, for illustration) a `CW` sent while the first is
still keying, which is refused; the node itself never does that.

```
node: 01 HELLO*63
fw:   01 OK HELLO 1 60 2000 5230 NR7Y-CW HFNODE*33
node: 02 STOP*3A
fw:   02 OK STOP*1E
node: 03 STATUS*37
fw:   03 OK STATUS 0 60000*15
node: 04 MODE*27
fw:   04 OK MODE CW CW*03
node: 05 FREQ*25
fw:   05 OK FREQ 144060000 144060000*01
node: 06 POWER*79
fw:   06 OK POWER LOW1*18
node: 07 BREAKIN*7F
fw:   07 OK BREAKIN 1*4A
node: 08 CW 20 CQ DE N0CALL*51
fw:   08 OK CW*18
node: 09 STATUS*3D
fw:   09 OK STATUS 1 4210*2F
node: 0A CW 20 CQ DE N0CALL*28
fw:   0A ERR CW TX*0C
node: 0B STATUS*46
fw:   0B OK STATUS 0 5120*54
```
