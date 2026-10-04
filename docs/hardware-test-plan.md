# Hardware test plan

A staged bench plan for bringing the node up on a real IC-7300, safest steps first.
Do the steps in order. Do not move to the next step until the current one passes.
Write down the date, the result and any readings for every step (a results table is
at the end).

The rule for the whole plan: **nothing may damage the radio.** The three ways
automation hurts a transceiver are a stuck transmitter, transmitting into a high
SWR, and too much duty cycle at high power. Every transmit step below is designed
to show that one of those protections works before the node is trusted with it.

## Stop immediately if

Stop the test, force the radio to receive, and do not continue until you know why, if:

- The radio is transmitting (TX indicator lit, power output on the meter) when
  nothing should be keying it, or keeps transmitting after an `hfnode` command has
  exited.
- A command listed as not transmitting (`status`, `rx`, `setup`, `listen`, `run`
  outside a window) makes the radio transmit.
- SWR on the radio's own meter is above 2:1, or the SWR the node logs differs from
  the radio's meter by more than about 0.3.
- Output power on the Po meter (or an external wattmeter) is higher than the
  configured `power_watts`.
- After `setup`, the frequency, mode or power on the radio's display does not match
  the config.
- Any CI-V command fails with `radio rejected the command (NG)` or
  `unexpected reply`, in a step that is expected to pass.
- The tuner does not finish (the node reports `no reply from radio` after 15 s of
  tuning).
- The USB serial port or audio device drops out, the Pi resets, or audio is
  distorted while transmitting. These are signs of RF getting into the USB cable
  or the Pi.
- The software watchdog or the hardware PTT timer does not end a test transmission
  when it should.
- Anything smells hot, the radio's fan runs hard at low power, or the dummy load gets
  hotter than its rating allows.
- On the air: anyone reports interference, or the frequency was not clear.

**How to stop.** Fastest first:

1. Turn the radio off with its POWER switch, or remove its DC supply.
2. `Ctrl-C` an `hfnode` command, then run `hfnode radio --config $C rx`. Ctrl-C does
   **not** send the stop command: the radio's keyer finishes the text it was already
   given (up to 30 characters) before returning to receive. At 6 wpm that can be
   over a minute, so prefer option 1 if the radio misbehaves.
3. For the service: `sudo systemctl stop hfnode`. The unit then runs
   `hfnode radio ... rx` (see `deploy/hfnode.service`).

## Before you start

**Bench config.** Make a copy of the config for testing, so you can change values
without touching the production file:

```sh
cp hfnode.example.toml ~/bench.toml
C=~/bench.toml
```

In `~/bench.toml` set:

- `station.node_call`, `station.field_calls`, `station.serial_port`, `audio.device`
  (see [raspberry-pi-setup.md](raspberry-pi-setup.md));
- `station.frequency_hz` to a frequency inside your license privileges in the CW
  segment;
- `station.power_watts = 10` (raised only in step 8);
- `state_dir` to a scratch directory, for example `"/home/pi/bench-state"`, and
  `auth.key_file` to a scratch key made with `hfnode keygen --out ~/bench.key`. Do
  not test with the node's real key and state, because test exchanges use up codes.

**Radio settings.** Set and record the IC-7300 menu settings listed in
[raspberry-pi-setup.md, section 6](raspberry-pi-setup.md#6-ic-7300-settings),
including CI-V address 94h and CI-V USB baud 115200.

**Hardware for transmit steps (4 onward).** A 50-ohm dummy load rated well above the
test power, connected directly to the ANT connector. Nothing plugged into the KEY
jack. If you have one, an external wattmeter/SWR meter between the radio and the
load. Keep a hand near the radio's POWER switch during every transmit step.

**The radio's own meter.** During transmit steps, set the radio's meter to Po or
SWR (METER key) so you can compare its readings with the node's.

## Step -1: pre-bench self-test against a mock radio (no radio)

No radio, sound card, antenna or network needed: run it on the Pi (or a laptop)
before the radio is connected, and again after every software update. It proves
the node's logic end to end before any step below can transmit.

```sh
hfnode selftest              # every scenario; prints a PASS/FAIL table
hfnode selftest --list       # what each scenario covers
hfnode selftest --scenario fault- -v   # one group (or one name), with transcripts
hfnode selftest --sweep --csv ~/sweep.csv   # where decoding breaks: speed x SNR x keying
```

Each scenario runs the whole node (`node::run`: decoder, parser, session, station
safety layer and the real `Ic7300` CI-V driver) against `civ::mock`, a byte-level
IC-7300 that answers every command the driver uses exactly as Section 19 of the
manual says, and flags anything else as a protocol violation. A scripted field
operator keys CW audio (with noise and hand-keying jitter) into the node's audio
queue, listens to what the mock radio actually keyed and reacts: it opens, checks
the read-back, then answers `OK`, `NO` or `AGN`, and repeats an open or an `OK` that
got no answer. While the mock radio is on transmit, the node hears nothing of the
operator. Every scenario checks the exact text keyed, the gateway side effects
(messages sent, inbox marked read, weather requests), `last_seq`, zero CI-V
violations, the radio's settings as the node left them, that the node forced
receive after a fault and never otherwise, and safety bounds (longest key-down,
longest transmit, no transmit past the break-in delay plus the stuck margin, duty
cycle, receive when the node stops, the tuner cycles expected).

The scenarios cover the field grammar end to end: TX, RX with one to many messages,
the five-message cap and truncation, WX with 4- and 6-character grids, `FAIL`
replies, NO, AGN and AGN with a chunk letter, codes sent in two groups, a repeated
`OK` after a lost result, an open on fresh lines replacing a pending one, and the
10-minute pending-commit and `AGN` windows. Also lost read-backs, replayed and wrong
codes, garbled callsigns, noise bursts after `K`, sending speeds 10 to 30 wpm, SNR
20, 6, 3 and 0 dB in 2500 Hz, a sloppy hand key, the radio's sidetone in the receive
audio, USB echo off, CI-V Transceive frames from someone at the radio, a load the
tuner matches, listening windows (a high-SWR lockout cleared by the next window's
tune), and radio faults: high SWR, power fold-back, stuck transmit or key (also
after the last over), a transmitter that will not unkey, one that only the watchdog
gets off transmit, refused status commands, NG and lost or late CI-V replies, a
readout the radio refuses (left unread), and a tuner that never finishes.

It runs 100 times faster than real time by default (about 30 s for all of them on a
laptop). On a slow or busy Pi lower the speed with `--scale 20`; the result must not
depend on it (any scale from 1 to 200). `--scale 1` runs everything at real speed, including the CI-V reply
timeout, the watchdog and the forced-receive retries, which stay in real time in a
time-scaled run (about 17 minutes with `--jobs 64`).

**Sweep: where it breaks.** `hfnode selftest --sweep` runs a complete TX exchange
(open, read-back, `OK`, `SENT`; `--rx` adds an RX readout) for every combination of
field operator speed (5, 8, 10, 13, 15, 18, 20, 25, 30, 35 wpm), SNR in 2500 Hz
(clean, 20, 10, 6, 3, 0, -3, -6 dB) and keying (machine-keyed, and hand-keyed with
12% jitter, stretched gaps and 25 Hz off pitch), 3 trials each (`--trials`; the
grid with `--wpm`, `--snr`, `--keying`). The operator repeats unanswered
transmissions up to 3 times and answers a wrong read-back `NO`, then starts over
once on fresh lines. A run succeeds only if exactly the intended message reached
the gateway, once, and `SENT` was keyed; a wrong message delivered is counted
separately (`W!`) and, like any safety violation (`S!`), is a hard failure at any
SNR. It prints a successes/trials matrix per keying, the share of transmissions the
node decoded exactly, and the edges, and `--csv` writes every run. It exits
non-zero on a hard failure or a failed trial in the should-pass region
(machine-keyed 10-30 wpm at 6 dB and above, hand-keyed 10-25 wpm at 10 dB and
above). About 3.5 minutes on a 4-core laptop; on a Pi 4 estimate 4 to 6 minutes at
the default scale, about 17 minutes at `--scale 20`.

Edges measured on 2026-10-04 (default grid, 3 trials per cell):

| | Every trial passes down to | Fails | At 10 dB |
|---|---|---|---|
| Machine-keyed | -3 dB at 8-35 wpm; 5 wpm: clean, 6 to -3 dB (1 of 3 failed at 20 and 10 dB) | -6 dB at every speed (1 of 30 got through) | 8-35 wpm |
| Hand-keyed | -3 dB at 8-30 wpm (20 wpm: 0 dB); 35 wpm: 0 dB; 5 wpm: 3 dB | -6 dB at every speed | 5-35 wpm |

Repeats start at 20 dB at most speeds (0.3 to 0.7 extra transmissions per
exchange); hand-keyed, some exchanges need one even on a clean signal. 15 of 480
read-backs were garbled text that still parsed (hand-keyed even when clean): the
operator's read-back check caught them all, and no wrong message was delivered.
No run broke a safety bound. These are synthetic signals in white noise: they say
where the node's own decoding and protocol give out, not how a real band behaves.

**Pass (sweep):** `verdict: PASS`, with no `W!` or `S!` anywhere in the matrices.
Compare the edges with the table above after a software update; an edge that moves
by more than a step is worth a look even when the verdict passes.

**Test vectors for later steps.** Write the field operator's side of a session as
WAV files, with a manifest of what each should decode to and what the node should
answer:

```sh
hfnode testvectors --out ~/vectors --wpm 12,18,25 --snr clean,10
hfnode decode ~/vectors/01-open-tx-18wpm-clean.wav
```

The codes in them come from a fixed **test-only** key (`test-only.key`, written
alongside), which anyone can compute: never use it as a node's key on the air.
They are for steps 2, 3 and 11 (play them from a second device into the radio's
receive audio or a dummy-load setup with a scratch config, as the manifest says).
The manifest gives what each file decodes to: some noisy ones lose a callsign,
number or code to the noise, and for those it gives no reply to expect.

**Pass:** `hfnode selftest` ends with `0 failed`, and the clean vectors decode to
their manifest text. **Fail:** any scenario fails: do not go on to step 4. Run the
failing scenario with `-v` and keep its transcript.

**What this cannot prove.** The mock implements what the manual says, so it cannot
find a place where the manual and the real radio disagree, or where the code and
the mock share the same misreading of it (step 0 checks that against the manual).
It has no RF, so nothing about real SWR, power output, the tuner's actual timing,
the radio's keyer timing, RF getting into USB or audio, the USB serial link or the
sound card. Its CW is synthetic and its noise is white Gaussian: real band noise,
QSB, QRM and real fists are only tested from step 2 on. Watchdog and forced-receive
margins are exercised at the mock's speed only with `--scale 1`, and only against
the manual's figures; step 9 measures them on the radio. The hardware PTT timer
(step 10) cannot be tested in software at all.

## Step 0: check every CI-V command against ICOM's IC-7300 guide

No radio needed. This is a desk check, and it must be finished before any step that
writes to the radio (step 4 onward).

Each command in `crates/civ/src/ic7300.rs` cites ICOM's *IC-7300 Full Manual*
(English, revision `IC-7300_ENG_FM_12b`), Section 19 "CONTROL COMMAND", which is
ICOM's CI-V reference for the IC-7300: data format p. 19-2, command table pp. 19-3 to
19-8, data content descriptions pp. 19-9 to 19-15. A text copy is in the project
files at `reference/IC-7300_ENG_FM_12b.txt`. (Earlier versions of the code took
these values from the IC-7300**MK2** CI-V guide; they have since been re-cited from
the IC-7300 manual, and this step confirms that independently.)

Open the manual (preferably the PDF from ICOM, not only the text copy) and check
each row. If you also have ICOM's separate IC-7300 CI-V Reference Guide, check it
against that too. Tick a row only if the manual says exactly what the code does.

Frames the node sends look like `FE FE 94 E0 <cmd> <sub> <data> FD` and the radio
answers `FE FE E0 94 ... FD`. All examples use address 94h.

| # | Item | What the code sends or expects | Check in the manual | OK |
|---|---|---|---|---|
| 0.1 | Frame format | Preamble `FE FE`, to-address, from-address, command, optional sub-command, data, end `FD` | p. 19-2, data format | ☐ |
| 0.2 | Addresses | Radio `94`, controller `E0` | p. 19-2 (94h is the IC-7300 default; the MK2's is B6h); CI-V Address item, p. 12-10 | ☐ |
| 0.3 | OK / NG replies | `FB` = OK, `FA` = NG. Every set command must be answered `FB` or it is treated as failed | p. 19-2 | ☐ |
| 0.4 | Frequency data | 5 bytes BCD, lowest digits first. 7,030,000 Hz = `00 00 03 07 00` | p. 19-9, frequency data | ☐ |
| 0.5 | `03` read frequency | Sends `03`; expects `03` + 5 frequency bytes | p. 19-3 | ☐ |
| 0.6 | `05` set frequency | `05 00 00 03 07 00` for 7.030 MHz | p. 19-3, p. 19-9 | ☐ |
| 0.7 | `06` set mode | `06 03 01`: mode `03` = CW, filter `01` = FIL1 | p. 19-3; mode and filter codes p. 19-9 (CW is 03, CW-R is 07) | ☐ |
| 0.8 | Level data | 2 bytes BCD, high digits first: level 102 = `01 02` | the `00 00` to `02 55` ranges on the 14 and 15 rows, p. 19-3 | ☐ |
| 0.9 | `14 0A` RF power | `14 0A` + level. The code maps watts linearly, `round(watts x 255 / 100)`: 10 W = `00 26`, 25 W = `00 64`, 40 W = `01 02`, 50 W = `01 28` | p. 19-3: "Send/read [RF PWR] position (00 00=max. CCW, 02 55=max. CW)". This is a knob position, not watts, so linearity is an assumption: step 8 measures it | ☐ |
| 0.10 | `14 0C` key speed | `14 0C` + level, `00 00` = 6 wpm to `02 55` = 48 wpm, linear: 18 wpm = `00 73` | p. 19-3, 14 0C | ☐ |
| 0.11 | `15 12` SWR meter | Reads `15 12`; reply `15 12` + 2 BCD bytes. Converted with 0000 = 1.0, 0048 = 1.5, 0080 = 2.0, 0120 = 3.0, linear between points, extrapolated above 0120 (`swr_from_meter`) | p. 19-3, 15 12: the same four points | ☐ |
| 0.12 | `16 47` break-in | `16 47 01` = semi break-in ON. The node never sends `02` (full break-in) | p. 19-3, 16 47 | ☐ |
| 0.13 | `17` send CW | `17` + up to 30 characters; expects `FB` | p. 19-4 and p. 19-13: "Up to 30 characters"; allowed characters 0-9, A-Z, a-z, / ? . - , : ' ( ) = + " @ and space, which is exactly what `cw::is_sendable` allows (`crates/cw/src/morse.rs`). Footnote *2 (p. 19-8): sent as CW only in CW mode with TRANSMIT, an external TX switch, or break-in ON | ☐ |
| 0.14 | `17 FF` stop CW | `17 FF` | p. 19-13: "FF" stops sending CW messages | ☐ |
| 0.15 | `1C 00` TX state, read | Reads `1C 00`; `00` = receive, anything else = transmit | p. 19-7 | ☐ |
| 0.16 | `1C 00` TX state, set | Only ever sends `1C 00 00` (receive). The node never sends `1C 00 01` | p. 19-7; and `grep -n set_transmit crates/` shows only `false` outside tests and the `Rig` trait | ☐ |
| 0.17 | `1C 01` tuner, start | `1C 01 02` = tune | p. 19-7: 00 = tuner OFF, 01 = ON, 02 = "Send/read to tuning" | ☐ |
| 0.18 | `1C 01` tuner, read | Treats a reply of `02` as still tuning, anything else as finished; gives up after 15 s and forces receive | p. 19-7. The manual does not say how long `02` is reported; step 5 confirms it on the radio | ☐ |
| 0.19 | USB echo back | Frames not addressed to E0 from 94 are skipped, so an echoed copy of the node's own frame is ignored | CI-V USB Echo Back item, p. 12-11 | ☐ |
| 0.20 | CI-V Transceive | Frames the radio sends unasked when its frequency or mode is changed at the front panel (`FE FE 00 94 00 ...` and `... 01 ...`) are skipped like the echo, also while reading the link quiet after a timeout | CI-V Transceive (default ON) and "The default transceive address is 00h", p. 12-10; commands 00 and 01, p. 19-3 | ☐ |
| 0.21 | Unit tests | `cargo test -p civ` passes, and the bytes in the `frames_on_the_wire` test match the rows above | `crates/civ/src/ic7300.rs` | ☐ |

**Pass:** every row ticked. **Fail:** any difference. Fix the code and its citation,
update the unit test, and repeat step 0. Do not run steps 4 onward against a command
that has not been ticked.

Also check `15 11`, the Po meter, which the node reads to count SWR readings only
while there is output (`po_from_meter`): reply `15 11` + 2 BCD bytes, converted with
"00 00=0%, 01 43=50%, 02 13=100%" (p. 19-3), linear between those points.

Related manual items that are useful during testing but are not used by the node:
`14 09` reads the CW pitch ("01 28=600 Hz", p. 19-3), and `1A 05 00 75` sets CI-V
USB Echo Back ("00=ON, 01=OFF", p. 19-5; OFF by default, p. 12-11).

## Step 1: serial link, read only

Radio on, connected to its normal antenna or a dummy load, KEY jack empty.

```sh
hfnode radio --config $C status
```

**Look for:** `frequency N Hz` matching the radio's display, and
`transmitting: false`. Change the frequency on the radio's dial and run it again.

**Pass:** both lines printed, frequency matches the display to the hertz, radio
did not transmit.

**Fail:**

- `opening radio on /dev/...`: wrong `serial_port`, or your user is not in `dialout`.
- `no reply from radio`: baud rate or CI-V address mismatch between the radio menu and
  the config, or CI-V USB Port linked to REMOTE at a different speed.
- `radio rejected the command (NG)` or `unexpected reply`: stop; recheck step 0.4/0.5.

Optional: run it once with CI-V USB Echo Back ON and once with it OFF. Both should work.

## Step 2: live audio and decoding, receive only

Tune the radio by hand to a known, steady CW signal: for example W1AW code practice
or bulletins (published schedule), a beacon, or a friend calling on a clear
frequency. Put the radio in CW mode, CW pitch 600 Hz, and tune so the signal sits on
the pitch (use the radio's tuning indicator or AUTOTUNE).

First check the audio level, with a strong signal tuned in:

```sh
arecord -D plughw:CARD=CODEC,DEV=0 -f S16_LE -r 8000 -c 1 -V mono /dev/null
```

The VU meter should peak well below 100% on the strongest signals. Adjust USB AF
Output Level on the radio if not. Ctrl-C to stop. Then:

```sh
hfnode listen --config $C
```

**Look for:** the decoded text printed as it arrives, with `*` for characters it
could not decode.

**Pass:** a strong, clean, machine-sent signal decodes as readable text with only
occasional errors. Weaker or hand-sent signals decode less well; note how well.

**Fail:** nothing printed (check `audio.device`, the radio's audio settings, and
that `audio.pitch_hz` matches the radio's CW pitch), or garbage on a strong clean
signal (signal not on the pitch, or audio clipping).

## Step 3: decode a recording, receive only

With the same kind of signal tuned in, record a minute of audio and decode it:

```sh
arecord -D plughw:CARD=CODEC,DEV=0 -f S16_LE -r 8000 -c 1 -d 60 ~/rec-$(date +%Y%m%d).wav
hfnode decode ~/rec-$(date +%Y%m%d).wav --pitch 600
```

For comparison, decode a synthetic signal at a similar speed and noise level:

```sh
hfnode synth "N0CALL/P 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K" --out ~/syn.wav --snr 6 --jitter 0.1
hfnode decode ~/syn.wav
```

**Pass:** the recording decodes about as well as `listen` did, and the speed
estimate is close to the actual speed. Keep the recordings: they are useful test
data for decoder changes.

**Fail:** the recording decodes much worse than live, or not at all: check the
sample format and the device.

## Step 4: put the radio in the node's state (no transmit)

From here on: **dummy load on ANT**, KEY jack empty, step 0 complete.

```sh
hfnode radio --config $C setup
```

This sends, in order: force receive (`1C 00 00`), frequency (`05`), CW mode FIL1
(`06 03 01`), RF power (`14 0A`), key speed (`14 0C`), semi break-in (`16 47 01`).

**Look for:** `configured: <freq> Hz, CW, 10 W, 18 wpm`. On the radio: the
frequency, CW mode, FIL1, BK-IN shown on the display, RF power about 10%, keyer
speed 18 wpm.

**Pass:** the command succeeds, the display matches, the radio did not transmit.

**Fail:** any error, or any setting on the display differs from the config. A
setting that is accepted (`FB`) but shows the wrong value means a byte value is
wrong: go back to step 0.

## Step 5: tuner into the dummy load (transmits briefly)

```sh
hfnode radio --config $C tune
hfnode radio --config $C status
```

**Look for:** the radio transmits briefly and shows the tuner working, then
`tuned`. A line `health: tune NNNms` in the log, and a `<time>,tune,NNNms` line in
`<state_dir>/health.csv`. `status` reports `transmitting: false`.

**Pass:** the tune finishes in a few seconds, the radio is back on receive, and
`health.csv` has the tune line.

**Fail:** `no reply from radio` after about 15 s (the node then forces receive; check
step 0.18), the radio stays on transmit, or the tuner cannot match a dummy load.

## Step 6: short CW and the SWR reading (transmits)

Set the radio's meter to SWR. Monitor with the radio's sidetone or a nearby receiver.

```sh
hfnode radio --config $C cw "VVV DE N0CALL"
hfnode radio --config $C status
```

The node reads SWR (`15 12`) repeatedly during the first second of the first piece
it keys, counting only readings taken while the Po meter (`15 11`) shows output.

**Look for:** the CW sent correctly at 18 wpm, `health: swr 1.0x` in the log, a
`<time>,swr,1.0x` line in `health.csv`, `sent; see health.csv for the SWR reading`,
then `transmitting: false`.

**Pass:** text sent correctly; logged SWR is 1.3 or less and within about 0.2 of the
radio's own SWR meter; the radio returns to receive when the text ends.

**Fail:** wrong characters sent, SWR reading far from the radio's meter (check step
0.11), or the radio does not return to receive.

## Step 7: high-SWR lockout (transmits briefly into a mismatch)

This needs a deliberately mismatched but safe load: a non-inductive 100-ohm load
(about 2:1) or 150-ohm load (about 3:1) rated for the test power. **Never** test with
an open or shorted connector. If you do not have such a load, skip this step for now
but complete it before unattended operation.

Switch the internal tuner **off** on the front panel (otherwise it may match the
load). Keep power at 10 W.

```sh
hfnode radio --config $C cw "VVV"
hfnode radio --config $C status
```

**Look for:** an error containing `SWR x.x above limit` (or `no output while
keying`, if the radio cut its own power into the load), a `swr` line in
`health.csv`, and `transmitting: false`.

**Pass:** with the 150-ohm load the node stops with the SWR error after well under a
second of transmission and the radio is on receive. With the 100-ohm load, the
reading is close to 2.0; whether it trips depends on which side of `swr_limit = 2.0`
it lands, so compare it with the radio's meter.

**Fail:** the node keeps keying, or its reading is far from the radio's meter.

Switch the tuner back on and reconnect the 50-ohm dummy load.

## Step 8: power calibration (transmits)

The code assumes the `14 0A` scale is linear from 0 to 100 W. Check it at each power
you might use. For each of 10, 25, 40 and 50 W: set `station.power_watts` in
`~/bench.toml`, then

```sh
hfnode radio --config $C setup
hfnode radio --config $C cw "TEST TEST TEST"
```

and read the Po meter (and the external wattmeter) while it keys. Let the radio rest
a minute between runs.

**Pass:** measured power within about 10% (or 2 W) of the configured value at each
setting, and never above it.

**Fail:** any reading above the configured power, or consistently off: the
`power_level` mapping in `crates/civ/src/ic7300.rs` needs correcting before going
above 10 W.

Set `power_watts` back to 10 for the following steps.

## Step 9: software watchdog (transmits)

This checks that the node forces the radio back to receive when one keying run lasts
too long, and that `17 FF` and `1C 00 00` actually work.

In `~/bench.toml` set `key_speed_wpm = 6` and `max_key_seconds = 5`. The text below is
ten zeros, which the keyer takes about 44 seconds to send at 6 wpm.

```sh
hfnode radio --config $C cw "0000000000"
hfnode radio --config $C status
```

**Look for:** about 5 seconds into the transmission, the log line
`watchdog: keying exceeded 5s, forcing receive`; transmission stops; the command
exits with `transmitter did not return to receive`; `status` shows
`transmitting: false`.

**Pass:** the radio stops transmitting within 6 seconds of starting, and stays on
receive.

**Fail:** the radio keeps sending the zeros. Turn it off (or wait out the 44 seconds
into the dummy load) and investigate `17 FF` and `1C 00 00` (steps 0.14 and 0.16).
Do not continue.

Restore `key_speed_wpm = 18` and `max_key_seconds = 45`.

## Step 10: hardware PTT timer

The design calls for a hardware timer that ends any transmission after about 60 s
regardless of software, as the last line of defence against a stuck transmitter.

**Read this first.** The node does not use a PTT or keying line: it keys CW with CI-V
command `17`, through the radio's internal keyer. A timer wired in series with the
KEY jack or a PTT line will therefore **not** stop a transmission started by the
node. The timer has to detect transmit in a way that works for CI-V keying and act
on something that ends the transmission without any software. One way to sense
transmit is pin 3 (SEND) of the ACC socket, which "goes low when the transceiver
transmits" (IC-7300 Full Manual, ACC socket, p. 18-2). That pin is also an input:
pulling it to ground **makes the radio transmit**, so the timer may only sense it
through a high-impedance input and must never drive it low. What the timer then
interrupts (for example the radio's DC supply) is your design decision; write down
what it senses and what it interrupts.

As an additional backstop inside the radio, set **Time-Out Timer (CI-V)** (MENU >
SET > Function, p. 12-5) to its shortest setting, 3 minutes. The manual says it
applies to transmitting "initiated by a CI-V command or pushing TRANSMIT". It is too
long to replace the hardware timer, and this plan does not test it.

Set the timer to a test value, for example 30 s. In `~/bench.toml` set
`key_speed_wpm = 6` and `max_key_seconds = 120` (the maximum allowed), so the
software watchdog does not act first.

```sh
hfnode radio --config $C cw "0000000000"
```

**Look for:** the transmission (about 44 s long) ends at the timer's setting.

**Pass:** transmission ends within 2 s of the timer setting, by the timer's action
alone. After the radio is back (powered up again, if the timer cuts DC), run
`hfnode radio --config $C status` and confirm `transmitting: false`.

**Fail:** transmission runs the full 44 s. The timer does not protect against CI-V
keying; the node must not run unattended until it does.

Then set the timer to its operating value (about 60 s) and restore
`max_key_seconds = 45`, `key_speed_wpm = 18`. `max_key_seconds` must stay below the
hardware timer. The node keys at most 30 characters per keyer command, which takes
about 20 s at 18 wpm, so 45 s leaves margin.

## Step 11: full exchange into dummy loads (optional, recommended)

A complete transaction with no signal on the air: the field rig transmits into its
own dummy load a few metres from the node, at its lowest power, so the node hears it
by leakage. Use the bench key and state.

In `~/bench.toml` set `schedule.always = true`, configure one `[[contacts]]` entry
with your own email address, and the `[email]` settings. Print a few codes:

```sh
hfnode codes --config $C --count 10
```

Run the node in the foreground, with the secrets loaded:

```sh
set -a; . /etc/hfnode/env; set +a     # or export HFNODE_EMAIL_PASSWORD=... by hand
hfnode run --config $C
```

From the field rig, send the open and commit for a `TX` to your own contact, using
the exact formats in [operating.md](operating.md).

**Pass:** the node logs `heard: ...`, keys the read-back into the dummy load (start-up
tune and SWR logged), keys `SENT n` after the commit, and the email arrives.
`<state_dir>/last_seq` holds the commit's sequence number.

## Step 12: on the air, low power, with a second station

Now with the real antenna. Power 10 W. Arrange a second station (ideally the field
operator, at some distance) and a time. Check the frequency is clear before each
transmission, and send your callsign (the node's transmissions all end
`DE <node_call> K`).

```sh
hfnode radio --config $C tune
hfnode radio --config $C cw "QRL?"          # then listen; continue only if clear
hfnode radio --config $C cw "VVV DE N0CALL"
```

Then ask the second station to send a few lines and run `hfnode listen --config $C`.

**Look for:** a tune line and an SWR reading in `health.csv`; the second station's
signal report; the node's decode of the second station.

**Pass:** SWR after tuning 1.5 or less; the second station copies the node; the node
decodes the second station well enough to read callsigns and numbers.

**Fail:** high SWR (check the antenna and feedline before anything else); RF
getting into the Pi or USB (serial errors, audio dropouts, resets) while
transmitting: add ferrite chokes on the USB cable and check grounding before
continuing.

## Step 13: end-to-end exchange on the air

Use the real configuration now: `/etc/hfnode/hfnode.toml`, the real key, and a
freshly printed table (`sudo -u hfnode hfnode codes --config /etc/hfnode/hfnode.toml`).
Keep `power_watts` low for the first session. Run the node in the foreground the
first time (`sudo systemctl stop hfnode` if it is running), or start the service and
watch `journalctl -u hfnode -f`.

The second station plays the field operator, inside a listening window, using
[operating.md](operating.md). Work through:

1. `TX` to a contact you control. Expect the read-back, then `SENT n` after `OK`,
   and the email or text to arrive.
2. Reply to that message from the contact's address or phone. After `email.poll_secs`
   (and screening), `RX`: expect `R n 1 MSG ?`, then the message after `OK`.
3. `AGN` and `AGN A`: expect the whole transmission, then chunk A, repeated.
4. `WX`: expect a read-back and a forecast.
5. Open a transaction, then `NO`: expect `R NO`, and nothing sent.
6. Resend the open of an already-completed transaction: expect silence.
7. Send a code from the wrong line: expect silence.
8. Optional: reply with a word the filter should remove, and check that `RX` shows
   `REDACTED` in its place.

**Look for:** in the log, `heard:`, `opened transaction`, `committed transaction`,
`sending:`, and `no reply:` with a reason for each silent case. In `state_dir`:
`last_seq` equal to the highest line used (an open burns its line too), `rx.log`
with every decoded transmission, `health.csv` with tune and SWR lines.

**Pass:** every item behaves as described, the radio is on receive between
exchanges, SWR readings stay steady.

**After passing:** raise `power_watts` in steps to the operating value (30-50 W),
repeating step 12's SWR check at each power. Then enable the service
(`sudo systemctl enable --now hfnode`) and keep an eye on `health.csv` for the first
weeks: a slow rise in SWR readings means a connector or the antenna needs attention.

## Results

| Step | Date | Result | Readings / notes |
|---|---|---|---|
| -1 Self-test (mock radio) | | | version: / scale: / passed: |
| 0 CI-V desk check | | | |
| 1 Status | | | |
| 2 Listen | | | |
| 3 WAV decode | | | |
| 4 Setup | | | |
| 5 Tune | | | tune ms: |
| 6 Short CW, SWR | | | node SWR: / radio SWR: |
| 7 SWR lockout | | | load: / node SWR: |
| 8 Power | | | 10 W: / 25 W: / 40 W: / 50 W: |
| 9 Watchdog | | | stopped after: s |
| 10 Hardware timer | | | set: s / stopped after: s |
| 11 Dummy-load exchange | | | |
| 12 On air | | | SWR: / report: |
| 13 End to end | | | |
