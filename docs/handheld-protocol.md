# Handheld CW firmware: serial command set, version 1

What a handheld's firmware must do for `hfnode` to key it (`station.rig =
"handheld"`, [handheld.md](handheld.md)). The node's side is
`crates/hfnode/src/handheld/proto.rs` and `handheld.rs`; `handheld/mock.rs` is a
simulated firmware that keeps every rule here, and the tests run against it.

The firmware keys the radio's carrier itself, with its own keyer, from text the node
sends: the same split as the IC-7300, whose internal keyer sends the text the node
gives it over CI-V. The node never holds the transmitter on by itself.

## Link

- Serial, 8 data bits, no parity, 1 stop bit, no flow control, at 38400 baud unless
  the firmware says otherwise (`[handheld] baud`: 9600, 19200, 38400, 57600 or
  115200).
- The node holds DTR and RTS down for as long as the port is open. A cable that also
  carries the radio's audio, such as the AIOC, keys the radio's PTT with those lines;
  the firmware must not use them for anything.

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
up to twice when its reply does not come, except `CW`.

## Commands

| Command | Reply | What it does |
|---|---|---|
| `HELLO` | `OK HELLO <version> <tx limit s> <link timeout ms> <name>` | Who the firmware is. `version` is `1`. `tx limit`: the longest any one `CW` run may last, 1 to 60. `link timeout`: 1000 to 3000. `name`: the firmware's name, may contain spaces. |
| `STATUS` | `OK STATUS <tx> <quiet ms>` | `tx` is `1` from a `CW` being accepted until its text has gone out, a `STOP`, or a limit ends it, and while the transmitter is on for any other reason (its PTT pressed); else `0`. It is the transmitter's real state, not a copy of what the node asked for. `quiet`: milliseconds since the squelch was last open (someone else on the frequency), `0` while it is open, at most `60000`. |
| `FREQ` | `OK FREQ <rx Hz> <tx Hz>` | The frequencies the radio receives and would transmit on. |
| `FREQ <Hz>` | `OK FREQ <rx Hz> <tx Hz>` | Receive and transmit on that frequency, simplex: no offset, no split. Refused with `ERR FREQ RANGE` outside 144-148, 222-225 and 420-450 MHz (the firmware may refuse more), and with `ERR FREQ TX` while transmitting. The reply reads back what is now set. |
| `MODE CW` | `OK MODE CW` | Receive CW (a beat note at the firmware's CW pitch, for the node's decoder) and send CW as a keyed, unmodulated carrier. |
| `POWER LOW`, `MID` or `HIGH` | `OK POWER <level>` | The radio's own power levels. |
| `CW <wpm> <text>` | `OK CW` | Key `text` at `wpm` (5 to 50), with standard Morse timing, then return to receive. Sent once keying has begun: `STATUS` reads `tx` 1 from then until the text is done. `text`: 1 to 30 characters from `A-Z 0-9 . , ? ' / ( ) : = + - " @` and space (no lower case). |
| `STOP` | `OK STOP` | Stop keying at once, mid-element if need be. Replied only once the transmitter is off. Accepted at any time, keying or not. |

Errors are `ERR <command> <code>`:

| Code | Meaning |
|---|---|
| `TX` | `CW` or `FREQ <Hz>` while transmitting. |
| `LEN` | `CW` text empty or over 30 characters. |
| `CHAR` | A character outside the set above. |
| `WPM` | Speed outside 5-50. |
| `MODE` | `CW` before `MODE CW`, or another mode asked for. |
| `RANGE` | Frequency refused. |
| `UNKNOWN` | Not a command of this version. |

## What the firmware must do

These are what make the handheld safe to leave to the node; the node checks the
first two in `HELLO` and refuses firmware that reports anything looser.

1. **A transmit limit of its own.** No `CW` run lasts longer than the `tx limit` it
   reports (60 s at most), whatever the text and speed: when it is reached, the
   transmitter goes off.
2. **A link timeout.** While a `CW` run lasts, if no valid line has arrived for the
   `link timeout` it reports (1 to 3 s), the transmitter goes off. This is what
   ends a transmission when the computer crashes, `hfnode` is killed, or the cable
   is pulled.
3. **Keying only for `CW`.** The firmware never transmits except while sending the
   text of an accepted `CW`, or when the radio's own PTT is pressed.
4. **`STOP` always works**, at any moment, and its reply means the transmitter is off.
5. **`STATUS` tells the truth** about the transmitter, read from the hardware state
   rather than from a flag the firmware set.
6. **Damaged lines do nothing** (above).
7. **Receive at power-on**, and after any reset.

The radio's own transmit time-out timer (its menu) stays on at its shortest setting
as the last backstop.

## What the node does

So that the firmware's author knows the timing it will see:

- At start: `HELLO`, `STOP`, `STATUS` (must read `tx` 0); then `STOP` again,
  `MODE CW`, `FREQ <Hz>` and `POWER`. Before every transmission, and every few
  minutes while it listens: `STATUS`, the same settings again, and `FREQ` to read
  them back.
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
  every 250 ms until it reads 0; those lines keep the link alive, so a firmware that
  ignores `STOP` is ended by its transmit limit. If the link is cut, or the node
  dies, nothing arrives, and the link timeout ends the run.

## Example

```
node: 01 HELLO*63
fw:   01 OK HELLO 1 60 2000 UVK1-CW*32
node: 02 STOP*3A
fw:   02 OK STOP*1E
node: 03 STATUS*37
fw:   03 OK STATUS 0 60000*15
node: 04 MODE CW*13
fw:   04 OK MODE CW*37
node: 05 FREQ 144060000*32
fw:   05 OK FREQ 144060000 144060000*01
node: 06 POWER LOW*0D
fw:   06 OK POWER LOW*29
node: 07 CW 20 CQ DE N0CALL*5E
fw:   07 OK CW*17
node: 08 STATUS*3C
fw:   08 OK STATUS 1 4210*2E
node: 09 CW 20 CQ DE N0CALL*50
fw:   09 ERR CW TX*74
node: 0A STATUS*45
fw:   0A OK STATUS 0 5120*57
```
