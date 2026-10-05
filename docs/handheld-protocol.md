# Handheld CW firmware: serial command set, version 1

How `hfnode` keys a handheld (`station.rig = "handheld"`, [handheld.md](handheld.md)):
the commands added to the NR7Y CW firmware for the Quansheng UV-K1 and UV-K5 v3 in
[firmware/uv-k1](../firmware/uv-k1/README.md) (`app/hfnode.c`). The node's side is
`crates/hfnode/src/handheld/proto.rs` and `handheld.rs`; `handheld/mock.rs` is a
simulated firmware that keeps every rule here, and the node's tests run against it.
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
- The headset jack's serial line is not used: it shares a wire with the PTT.

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
| `HELLO` | `OK HELLO <version> <tx limit s> <link timeout ms> <name>` | Who the firmware is: `OK HELLO 1 60 2000 NR7Y-CW HFNODE`. `tx limit`: the longest any one `CW` run may last (the node accepts 1 to 60). `link timeout`: the node accepts 1000 to 3000. `name` may contain spaces. |
| `STATUS` | `OK STATUS <tx> <quiet ms>` | `tx` is `1` from a `CW` being accepted until its text has gone out and the transmitter reads off, or a stop has turned it off; and while the transmitter is on for any other reason (its PTT pressed). Read from the radio chip (the BK4819's transmit bit) as well as the firmware's state. `quiet`: milliseconds since the squelch was last open (someone else on the frequency), `0` while it is open, at most `60000`; the squelch tail just after the radio's own transmission is not counted. |
| `FREQ` | `OK FREQ <rx Hz> <tx Hz>` | The frequencies the radio receives and would transmit on. |
| `MODE` | `OK MODE <tx> <rx>` | The transmit and receive modes: `CW`, `FM`, `AM`, `USB`, ... or `OTHER`. |
| `POWER` | `OK POWER <level>` | The transmit power level: `LOW1` to `LOW5`, `MID`, `HIGH`, `USER` or `OTHER`. |
| `BREAKIN` | `OK BREAKIN <0 or 1>` | Whether break-in is on. Without it the keyer only sounds the sidetone. |
| `CW <wpm> <text>` | `OK CW` | Key `text` at `wpm` (5 to 50), with standard Morse timing, then return to receive. Answered once keying has begun, within 300 ms; `ERR CW REFUSED` if it has not by then. `text`: 1 to 30 characters from `A-Z 0-9 . , ? ' / ( ) : = + - " @` and space (no lower case). |
| `STOP` | `OK STOP` | Stop keying at once, mid-element if need be. Accepted at any time, keying or not. A `CW` not yet answered is answered `ERR CW STOP` first. `STATUS` reads `tx` 1 until the transmitter reads off (below). |
| `TEST HANG` | `OK TEST HANG` | Bring-up only (`hfnode handheld hangtest`): while a `CW` is keying, stop the firmware's main loop for good, as a hang would. Only the watchdog can end the transmission then. |

Errors are `ERR <command> <code>`:

| Code | Meaning |
|---|---|
| `LEN` | `CW` text empty or over 30 characters. |
| `WPM` | Speed outside 5-50. |
| `CHAR` | A character outside the set above. |
| `MODE` | `CW` with the radio not in CW, or the radio switched out of CW before keying began. |
| `BKIN` | `CW` with break-in off. |
| `TX` | `CW` while transmitting, recording or playing a CW memory, or still stopping; or a limit ended it before keying began. |
| `REFUSED` | `CW` accepted, but the radio did not key within 300 ms: its transmit lock, a frequency it does not transmit on, a low battery, or the paddle or key in use. |
| `STOP` | `CW` stopped by `STOP` before keying began. |
| `RUN` | `TEST HANG` with no `CW` keying. |
| `UNKNOWN` | Not a command of this version (any setting command, too: nothing is set over the link). |

## What the firmware does

These are what make the handheld safe to leave to the node. The node checks the
first two in `HELLO` and refuses firmware that reports anything looser. The radio's
own transmit time-out timer does not work in CW (the firmware clears it on every
key-down), so these are the only limits on the radio.

1. **A transmit limit.** No `CW` run lasts longer than the `tx limit` it reports
   (60 s), whatever the text and speed.
2. **A link timeout.** While a `CW` run lasts, if no valid line has arrived for the
   `link timeout` it reports (2 s), it stops the run. This is what ends a
   transmission when the computer crashes, `hfnode` is killed, or the cable is
   pulled.
3. **Every stop is checked.** After a stop by `STOP` or by a limit, the transmitter
   must read off within 0.5 s; if it does not, the firmware stops feeding its
   watchdog, which resets the radio within about 2 s.
4. **A watchdog.** While a `CW` run lasts, the hardware watchdog resets the radio if
   the main loop stops for a second; outside one it is fed from the 10 ms tick. A
   hard fault stops that tick, and so resets the radio too. A reset turns the
   transmitter off: start-up resets the radio chip within milliseconds.
5. **Keying only for `CW`.** The firmware never transmits for the node except while
   sending the text of an accepted `CW`.
6. **`STATUS` tells the truth** about the transmitter, read from the radio chip as
   well as the firmware's state.
7. **Damaged lines do nothing** (above).
8. **Nothing is set or written**: no frequency, mode or power change, and nothing
   written to the radio's memory.

## What the node does

So that the firmware's timing is known:

- At start: `HELLO`, `STOP`, `STATUS` (must read `tx` 0). Then `STOP` again, and
  `MODE`, `FREQ`, `POWER` and `BREAKIN`, which must read CW, the configured frequency
  simplex, the configured power and `1`; if any does not, the node says what to
  change at the radio and does not transmit. Before every transmission, and every
  few minutes while it listens: `STATUS`, the same reads again, and `FREQ` again.
- Before keying: `STATUS`, and it waits for `quiet` to reach `busy_quiet_ms`.
- A transmission goes out as `CW` runs of at most 30 characters, one at a time. After
  each, once its text should have gone out, the node reads `STATUS` every 100 ms
  until `tx` reads 0, and only then sends the next. A run read back as ended well
  before its text could have gone out (cut short by a limit) fails the transmission.
- While a run lasts, a keep-alive `STATUS` goes out every quarter of the link timeout
  (every 250 ms, with timeouts of 1 s and up), besides the reads above.
- The node stops a run that goes on too long: 2 s past the length of its text at its
  speed, or `max_key_seconds` plus 5 s, whichever is sooner. It sends `STOP`, stops
  the keep-alives for that run, and fails the transmission. If `STATUS` still reads
  `tx` 1, it latches its transmit inhibit and goes on sending `STOP` and `STATUS`
  every 250 ms or so until it reads 0; meanwhile the firmware's check of its own
  stop (above) resets the radio. If the link is cut, or the node dies, nothing
  arrives, and the link timeout ends the run.

## Example

```
node: 01 HELLO*63
fw:   01 OK HELLO 1 60 2000 NR7Y-CW HFNODE*17
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
