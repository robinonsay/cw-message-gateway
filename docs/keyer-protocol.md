# Keyer box protocol

How hfnode talks to the keyer box ([keyer.md](keyer.md)) over its USB serial port.
The rules are in one place in code, `crates/keyer-core`: the box's firmware
(`firmware/pico2-keyer`) runs them, and hfnode's mock box runs the same code in the
tests and `hfnode selftest`.

## Lines

USB CDC serial; the baud rate is ignored. Each line is printable ASCII ending in
`\n` (a `\r` before it is ignored), at most 96 characters before it:

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
| `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <rest ms> <duty budget s> <uptime ms> <boot> <build> <name>` |
| `STATUS` | `OK STATUS <key> <run> <ended> <trip> <rest left ms> <budget ms>` |
| `CW <wpm> <text>` | `OK CW`, keying from that moment |
| `STOP` | `OK STOP`, the key open and any run ended |
| `TEST ARM` | `OK TEST ARM`, for the next 2 s (bring-up only) |
| `TEST HANG` | `OK TEST HANG` (bring-up only, armed) |
| `TEST STUCK` | `OK TEST STUCK` (bring-up only, armed) |

- `HELLO`: protocol version 2; the box's limits (below); time since it started;
  why it started (`POWER`, `WATCHDOG` or `OTHER`); `<build>`, the eight characters
  of the commit CI built the firmware from, or `-` for a build from a working tree
  (`[keyer] firmware_build` is checked against it); its name, `PICO2-KEYER`. hfnode
  refuses a box with another version, another name, or any limit looser than these.
- `STATUS`: `<key>` 1 while the key is closed; `<run>` 1 while a run is under way;
  how the last run ended (`NONE` if none yet, `DONE`, `STOP`, `LINK` for the link
  timeout, `LIMIT` for the run limit, `DOWN` for the key-down limit, `USB` when USB
  went away); `<trip>` `NONE`, `DOWN` (the key-down limit), `PIN` (the loop's own
  watch on the key pin) or `SLOW` (a pass of the loop too late with the key down);
  `<rest left ms>` of the rest after the last run still to go; `<budget ms>` of
  key-down time the duty budget would allow now.
- `CW`: 1 to 30 characters at 5 to 50 wpm: A-Z, 0-9, `. , ? ' / ( ) : = + - " @`
  and spaces, upper case only. The box keys them with its own timing: a dot of
  `1200 / wpm` ms rounded down, a dash of 3 dots, 1 dot between the elements of a
  character, 3 between characters, 7 between words. hfnode computes the same
  timeline, so it knows when each element is keyed.
- `TEST ARM`: arms the box for one test for the next 2 s. A `TEST HANG` or
  `TEST STUCK` that does not follow one is refused, so that a stray or replayed
  line cannot hold the key down.
- `TEST HANG`: during a run, armed. At its next key-down the box's control loop
  stops, so that its watchdog resets it (`hfnode keyer hangtest`). The reply goes
  out before the loop stops. If the watchdog does not reset the chip within
  1000 ms of the hang, the loop opens the key itself and holds it open, still
  without feeding the watchdog.
- `TEST STUCK`: during a run, armed. Its next key-down is held, so that the
  key-down limit trips it (`hfnode keyer stucktest`).

Errors are `ERR <command> <code>`:

| Reply | Meaning |
|---|---|
| `ERR CW TRIP` | the box has tripped; unplug it and plug it in again |
| `ERR CW RUN` | a run is already under way |
| `ERR CW WPM` | speed outside 5-50 wpm |
| `ERR CW LEN` | not 1-30 characters |
| `ERR CW CHAR` | a character the box cannot key |
| `ERR CW LIMIT` | its Morse would take longer than the run limit |
| `ERR CW REST` | the rest after the last run is not over |
| `ERR CW DUTY` | its key-down time is more than the duty budget allows |
| `ERR TEST RUN` | a test outside a run |
| `ERR TEST ARM` | a test that no `TEST ARM` armed, or armed over 2 s ago |
| `ERR <word> UNKNOWN` | a command the box does not know |

## Limits the box enforces on its own

| Limit | Value | What happens |
|---|---|---|
| Key-down | 1000 ms | Past it the key opens and the box trips: every `CW` is refused until it is power-cycled. No element is longer than a dash at 5 wpm (720 ms). |
| Run | 60 s | `CW` text longer than this is refused; a run is ended at it. |
| Link timeout | 2000 ms | A run ends when no valid line has arrived for this long, and at once when USB goes away: the cable comes out, or the computer resets or suspends the box's USB. |
| Watchdog | 500 ms | The RP2350's hardware watchdog, fed once at the end of each pass of the control loop (a pass takes microseconds; nothing in it waits). If the loop stalls, the chip resets and the key opens. |
| Rest | 1000 ms | `CW` is refused until the key has been up this long after the last run, so that runs sent back to back cannot hold the key down past its limit. |
| Duty budget | 60 s of key-down, refilled over 10 min | `CW` whose key-down time is more than the budget left is refused: at most half the time keying over any 10 minutes. A box that did not start from power-up begins with the budget empty. |
| Key pin | 1000 ms | The loop times the key pin by its own clock readings, apart from the Morse timeline: high for the key-down limit (bridging gaps under 24 ms, a dot at 50 wpm) trips the box, as does a pass more than 10 ms after the last with the key down. |

The key is open at power-up, at every reset and while USB connects, and whenever
no run is under way. The box never reads the serial port's DTR and RTS lines
(hfnode holds both down): opening or closing the port does nothing by itself.

Closing the port is not itself a stop: the box hears nothing about it. What ends a
run is the link timeout (no valid line for 2 s) or USB going away, which the host
reports as a bus reset, a suspend or the device being deconfigured. A host that
keeps the device configured while the program is gone leaves the link timeout to do
it. `hfnode keyer linktest` is the check that it does.

## How hfnode uses it

- Before opening the port: the port's USB product name must be `PICO2-KEYER`, so
  that hfnode cannot open a radio's own serial port by a stale device name (opening
  a port pulses DTR on Linux).
- At start: `HELLO` (checked as above, and the boot reason logged), `STOP`, then
  `STATUS`, which must show the key open, no run and no trip. A `WATCHDOG` boot
  outside `hangtest` latches hfnode's transmit inhibit; an `OTHER` boot refuses
  keying until the box is unplugged and plugged in again (the first boot after
  flashing is `OTHER`).
- Each piece of a transmission (at most 30 characters) is one `CW` run. Before it:
  `STATUS` (no run, no trip), and the radio's audio arriving at band level with no
  steady tone at the sidetone pitch (a carrier, or the key held at the radio). A
  trip latches hfnode's transmit inhibit.
- Before each run, hfnode waits out the box's rest and its duty budget, and its own
  `[keyer] max_duty_percent` of the last `duty_window_secs`.
- While a run lasts, `STATUS` every 0.25 s keeps it alive; if hfnode dies, the link
  timeout ends the run. A run still going 1 s past its Morse length is stopped once
  with `STOP`, and then left to the link timeout: the transmission fails.
- After each run, the sidetone heard on the radio's audio must follow the box's
  timeline and then stop; see [keyer.md](keyer.md), "What stops a stuck key".
- To force receive: `STOP`, then `STATUS` and the audio must show the key open.
- While not keying, hfnode looks at the audio every 0.25 s for a key held at the
  radio.
