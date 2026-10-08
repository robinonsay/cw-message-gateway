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
never `CW`, `MCW` or `TEST`, which could act twice.

The box has three outputs and one input. The **key** (GP16) closes an optocoupler
across a radio's key jack. The **PTT** (GP17) closes a second one across an FM
handheld's PTT contact, and the **tone** (GP18) is a 700 Hz square wave, filtered
into the handheld's microphone. The **PTT line** (GP19) reads the handheld's PTT
contact back: high while it is open, low while anything holds it. `CW` uses the
key; `MCW` uses the PTT, the tone and the line ([keyer.md](keyer.md), "A handheld
through its headset jack").

## Commands

| Command | Reply |
|---|---|
| `HELLO` | `OK HELLO <version> <run limit s> <link timeout ms> <key-down limit ms> <rest ms> <duty budget s> <ptt limit s> <uptime ms> <boot> <build> <name>` |
| `STATUS` | `OK STATUS <key> <run> <ended> <trip> <rest left ms> <budget ms> <ptt> <line>` |
| `CW <wpm> <text>` | `OK CW`, keying from that moment |
| `MCW <wpm> <text>` | `OK MCW`, the PTT closed from that moment |
| `STOP` | `OK STOP`, every output open and any run ended |
| `TEST ARM` | `OK TEST ARM`, for the next 2 s (bring-up only) |
| `TEST HANG` | `OK TEST HANG` (bring-up only, armed) |
| `TEST STUCK` | `OK TEST STUCK` (bring-up only, armed) |
| `TEST HOLD` | `OK TEST HOLD` (host tests only, armed, in an `MCW` run) |

- `HELLO`: protocol version 4; the box's limits (below), the PTT limit after the
  duty budget; time since it started;
  why it started (`POWER`, `WATCHDOG` or `OTHER`); `<build>`, the eight characters
  of the commit CI built the firmware from, or `-` for a build from a working tree
  (`[keyer] firmware_build` is checked against it); its name, `PICO2-KEYER`. hfnode
  refuses a box with another version, another name, or any limit looser than these.
- `STATUS`: `<key>` 1 while the key is closed; `<run>` 1 while a run is under way;
  how the last run ended (`NONE` if none yet, `DONE`, `STOP`, `LINK` for the link
  timeout, `LIMIT` for the run limit, `DOWN` for the key-down limit, `USB` when USB
  went away, `PTT` for the PTT limit, `LINE` when the PTT line did not read low
  after the box closed the PTT); `<trip>` `NONE`, `DOWN` (the key-down limit, on
  the key or a tone element), `PIN` (the loop's own watch on its pins), `SLOW` (a
  pass of the loop too late with the key or the tone on), `CLOCK` (the box's clock
  stopped or slowed against the processor's own count), `WATCHDOG` (the box
  restarted because its watchdog fired), `PTT` (the PTT down past the PTT limit)
  or `LINE` (the PTT line still low after the box let the PTT go); `<rest left
  ms>` of the rest after the last run still to go; `<budget ms>` of keyed time
  (key or PTT down) the duty budget would allow now; `<ptt>` 1 while the PTT is
  closed; `<line>` 1 while the PTT line reads high.
- `CW`: 1 to 30 characters at 5 to 50 wpm: A-Z, 0-9, `. , ? ' / ( ) : = + - " @`
  and spaces, upper case only. The box keys them with its own timing: a dot of
  `1200 / wpm` ms rounded down, a dash of 3 dots, 1 dot between the elements of a
  character, 3 between characters, 7 between words. hfnode computes the same
  timeline, so it knows when each element is keyed.
- `MCW`: the same text rules and timing, for an FM handheld. Refused (`LINE`)
  while the PTT line reads low: the PTT already held, or the radio off if its
  contact then reads low. A cable out reads high on the line's pull-up, so it is
  not refused here: the check 100 ms after the PTT closes ends that run (below).
  A run closes the PTT, waits 500 ms (the radio coming up on transmit, the far
  squelch opening), keys the Morse on the tone, waits 200 ms and opens the PTT.
  Its PTT time (500 ms, the Morse, 200 ms) must be under the PTT limit and the run
  limit (`LIMIT`) and within the duty budget (`DUTY`): the whole PTT time counts,
  because an FM transmitter's carrier is on for all of it. 100 ms after the PTT
  closes the line must read low, or the run ends (`LINE`: the radio was not keyed);
  100 ms after it opens the line must read high, or the box trips (`LINE`:
  something else holds the PTT, and the radio may still be transmitting).
- `TEST ARM`: arms the box for one test for the next 2 s. A `TEST HANG`,
  `TEST STUCK` or `TEST HOLD` that does not follow one is refused, so that a stray
  or replayed line cannot hold the key or the PTT down.
- `TEST HANG`: during a run, armed. At its next key-down the box's control loop
  stops, so that its watchdog resets it (`hfnode keyer hangtest`), and it comes
  back tripped (`WATCHDOG`). The reply goes out before the loop stops. If the
  watchdog does not reset the chip within 1000 ms of the hang, the loop opens the
  key itself and holds it open, still without feeding the watchdog.
- `TEST STUCK`: during a run, armed. Its next key-down (or tone element) is held,
  so that the key-down limit trips it (`hfnode keyer stucktest`).
- `TEST HOLD`: during an `MCW` run, armed. The PTT stays closed after the text, so
  that the PTT limit trips the box. For hfnode's own tests against the mock box;
  no bring-up step sends it.

Errors are `ERR <command> <code>`:

| Reply | Meaning |
|---|---|
| `ERR CW TRIP` | the box has tripped (also after its watchdog fired); unplug it and plug it in again |
| `ERR CW RUN` | a run is already under way |
| `ERR CW WPM` | speed outside 5-50 wpm |
| `ERR CW LEN` | not 1-30 characters |
| `ERR CW CHAR` | a character the box cannot key |
| `ERR CW LIMIT` | its Morse would take longer than the run limit |
| `ERR CW REST` | the rest after the last run is not over |
| `ERR CW DUTY` | its key-down time is more than the duty budget allows |
| `ERR MCW <code>` | as for `CW`, with `LIMIT` for a PTT time not under the PTT limit, `DUTY` for one the budget does not hold, and: |
| `ERR MCW LINE` | the PTT line reads low: the PTT held, or the radio off |
| `ERR TEST RUN` | a test outside a run (`TEST HOLD`: outside an `MCW` run) |
| `ERR TEST ARM` | a test that no `TEST ARM` armed, or armed over 2 s ago |
| `ERR <word> UNKNOWN` | a command the box does not know |

## Limits the box enforces on its own

| Limit | Value | What happens |
|---|---|---|
| Key-down | 1000 ms | Past it (by at most one pass of the loop, under 11 ms) the key opens and the box trips: every `CW` is refused until it is power-cycled. No element is longer than a dash at 5 wpm (720 ms). |
| Run | 60 s | `CW` text longer than this is refused; a run is ended at it. |
| Link timeout | 2000 ms | A run ends when no valid line has arrived for this long, and at once when USB goes away: the cable comes out, or the computer resets or suspends the box's USB. |
| Watchdog | 500 ms | The RP2350's hardware watchdog, fed once at the end of each pass of the control loop (a pass takes microseconds; nothing in it waits). If the loop stalls, the chip resets and the key opens, and the box comes back tripped (`WATCHDOG`). |
| Rest | 1000 ms | `CW` is refused until the key has been up this long after the last run, so that runs sent back to back cannot hold the key down past its limit. |
| Duty budget | 60 s of keying; each second off the air earns one back, up to 60 s | `CW` whose key-down time is more than the budget left is refused: the key is down at most half of any stretch of time plus 30 s, so at most 55% of any 10 minutes, and half in the long run. The PTT's time counts too, all of it: the transmitter is keyed while the key or the PTT is down. A box that did not start from power-up begins with the budget empty, or owing what it owed before the restart (see "Across a restart"). |
| Key pin | 1000 ms | The loop times the key pin by its own clock readings, apart from the Morse timeline: high for the key-down limit (bridging gaps under 24 ms, a dot at 50 wpm) trips the box, as does a pass more than 10 ms after the last with the key down. The tone pin is timed with it, the same way. |
| PTT | 60 s | No `MCW` whose PTT time is not under it. The PTT down past it opens the PTT and trips the box (`PTT`), whatever holds the run open; the loop's own watch on the PTT pin trips it too (`PIN`). Never more than the run limit. |
| PTT line | 100 ms | After the PTT closes, the line must read low within it, or the run ends; after it opens, the line must read high within it, or the box trips (`LINE`). |
| Clock | every 50 ms | The loop checks its clock (TIMER0, on the crystal's microsecond tick) against the processor's own cycle count (SysTick on `clk_sys`). If the clock advanced less than half as far as the processor's count, or the processor's count less than a quarter as far as the clock (the check itself is not running), the key opens, the box trips (`CLOCK`) and it stops feeding its watchdog, which resets it. No debugger can pause the clock: the firmware clears TIMER0's `DBGPAUSE`. |

### Across a restart

Every pass of the loop saves the box's trip, whether it is keying the transmitter
(its key or its PTT down) and its duty budget in two of the RP2350's watchdog
scratch registers, with a check byte. A watchdog reset or any other restart that
keeps the chip powered keeps them; unplugging the box clears them. At start the box reads them back:

- A trip it saved is kept: a restart never clears one. A box whose watchdog fired
  comes up tripped (`WATCHDOG`) whatever it saved.
- It starts with its duty budget empty, as any start other than from power-up
  does, or owing what it owed when it saved, if it owed key-down time; a further
  1000 ms is owed if it saved its key or its PTT down (the most either could have
  stayed down after that pass: the watchdog's 500 ms, with margin). A host that makes the
  box restart over and over gets no more key-down time from it than from one that
  never restarts.
- It rests 1000 ms from its start before its first run.

Saved words that do not check are ignored, and the box starts as from its boot
reason alone.

The key and the PTT are open, and the tone off, at power-up, at every reset and
while USB connects, and whenever no run is under way. A panic or a processor fault
opens the key and the PTT in its first register write, then turns the tone off.
The box never reads the serial port's DTR and RTS lines (hfnode holds both down):
opening or closing the port does nothing by itself.

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
  `STATUS`, which must show the key open and no run. A box that is tripped, or
  whose watchdog fired, outside `hangtest`, is opened all the same, but hfnode keys
  nothing on it and its first receive check latches the transmit inhibit and emails
  the owner; an `OTHER` boot refuses keying until the box is unplugged and plugged
  in again (the first boot after flashing is `OTHER`).
- Each piece of a transmission (at most 30 characters) is one `CW` run. Before it:
  `STATUS` (no run, no trip), and the radio's audio arriving at band level with no
  steady tone at the sidetone pitch (a carrier, or the key held at the radio). A
  trip latches hfnode's transmit inhibit.
- With `[keyer] output = "ptt"` each piece is one `MCW` run instead. Before it:
  the PTT line high, the radio's receive noise at band level and not gone quiet
  over the last second (a station on the channel: hfnode waits for it to clear),
  and the box's rest and duty budget for the whole PTT time. After it, the receive
  noise must have dropped while the PTT was closed (the radio transmitted) and come
  back after (it is on receive again); a PTT line still low with the PTT open
  counts as transmitting.
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
