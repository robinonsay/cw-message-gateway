# Keyer box protocol

How hfnode talks to the keyer box ([keyer.md](keyer.md)) over its USB serial port.
The rules are in one place in code, `crates/keyer-core`: the box's firmware
(`firmware/pico2-keyer`) runs them, and hfnode's mock box runs the same code in the
tests and `hfnode selftest`.

## Lines

USB CDC serial; the baud rate is ignored. Each line is printable ASCII ending in
`\n` (a `\r` before it is ignored), at most 80 characters before it:

```text
<id> <body>*<cs>
```

- `<id>`: two uppercase hex digits, chosen by hfnode, repeated in the reply. A late
  reply to an earlier command is never taken for the answer to this one. hfnode
  never uses `00` and never repeats an id back to back.
- `<cs>`: two uppercase hex digits, the XOR of every byte before the `*`.

A line that does not decode (bad checksum, too long, not ASCII) is ignored: no
reply, nothing done, and it does not count as hearing from hfnode. hfnode waits
0.3 s for each reply and sends `HELLO`, `STATUS` and `STOP` up to three times;
never `CW` or `TEST`, which could act twice.

## Commands

| Command | Reply |
|---|---|
| `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <uptime ms> <boot> <name>` |
| `STATUS` | `OK STATUS <key> <run> <ended> <trip>` |
| `CW <wpm> <text>` | `OK CW`, keying from that moment |
| `STOP` | `OK STOP`, the key open and any run ended |
| `TEST HANG` | `OK TEST HANG` (bring-up only) |
| `TEST STUCK` | `OK TEST STUCK` (bring-up only) |

- `HELLO`: protocol version 1; the box's limits (below); time since it started;
  why it started (`POWER`, `WATCHDOG` or `OTHER`); its name, `PICO2-KEYER`. hfnode
  refuses a box with another version or with any limit looser than these.
- `STATUS`: `<key>` 1 while the key is closed; `<run>` 1 while a run is under way;
  how the last run ended (`NONE` if none yet, `DONE`, `STOP`, `LINK` for the link
  timeout, `LIMIT` for the run limit, `DOWN` for the key-down limit, `USB` when
  the port closed); `<trip>` `NONE` or `DOWN`.
- `CW`: 1 to 30 characters at 5 to 50 wpm: A-Z, 0-9, `. , ? ' / ( ) : = + - " @`
  and spaces, upper case only. The box keys them with its own timing: a dot of
  `1200 / wpm` ms rounded down, a dash of 3 dots, 1 dot between the elements of a
  character, 3 between characters, 7 between words. hfnode computes the same
  timeline, so it knows when each element is keyed.
- `TEST HANG`: during a run only. At its next key-down the box's control loop
  stops, so that its watchdog resets it (`hfnode keyer hangtest`).
- `TEST STUCK`: during a run only. Its next key-down is held, so that the key-down
  limit trips it (`hfnode keyer stucktest`).

Errors are `ERR <command> <code>`:

| Reply | Meaning |
|---|---|
| `ERR CW TRIP` | the box has tripped; unplug it and plug it in again |
| `ERR CW RUN` | a run is already under way |
| `ERR CW WPM` | speed outside 5-50 wpm |
| `ERR CW LEN` | not 1-30 characters |
| `ERR CW CHAR` | a character the box cannot key |
| `ERR CW LIMIT` | its Morse would take longer than the run limit |
| `ERR TEST RUN` | a test outside a run |
| `ERR <word> UNKNOWN` | a command the box does not know |

## Limits the box enforces on its own

| Limit | Value | What happens |
|---|---|---|
| Key-down | 1000 ms | Past it the key opens and the box trips: every `CW` is refused until it is power-cycled. No element is longer than a dash at 5 wpm (720 ms). |
| Run | 60 s | `CW` text longer than this is refused; a run is ended at it. |
| Link timeout | 2000 ms | A run ends when no valid line has arrived for this long, and at once when USB goes away: the cable comes out, or the computer resets or suspends the box's USB. |
| Watchdog | 500 ms | The RP2350's hardware watchdog, fed only by the 1 ms control loop that times the key. If the loop stalls, the chip resets and the key opens. |

The key is open at power-up, at every reset and while USB connects, and whenever
no run is under way. The box never reads the serial port's DTR and RTS lines
(hfnode holds both down): opening or closing the port does nothing by itself. When
hfnode closes the port or stops, its check-ins stop and the link timeout ends the
run.

## How hfnode uses it

- At start: `HELLO` (checked as above, and the boot reason logged), `STOP`, then
  `STATUS`, which must show the key open, no run and no trip.
- Each piece of a transmission (at most 30 characters) is one `CW` run. Before it:
  `STATUS` (no run, no trip), and the radio's audio arriving at band level with no
  steady tone at the sidetone pitch (a carrier, or the key held at the radio). A
  trip latches hfnode's transmit inhibit.
- While a run lasts, `STATUS` every 0.25 s keeps it alive; if hfnode dies, the link
  timeout ends the run. A run still going 1 s past its Morse length is stopped.
- After each run, the sidetone heard on the radio's audio must follow the box's
  timeline and then stop; see [keyer.md](keyer.md), "What stops a stuck key".
- To force receive: `STOP`, then `STATUS` and the audio must show the key open.
- While not keying, hfnode looks at the audio every 0.25 s for a key held at the
  radio.
