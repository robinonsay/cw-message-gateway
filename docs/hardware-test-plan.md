# Hardware test plan

A staged bench plan for bringing the node up on a real IC-7300, safest steps first.
Do the steps in order. Do not move to the next step until the current one passes.
Write down the date, the result and any readings for every step (a results table is
at the end).

The rule for the whole plan: **nothing may damage the radio.** The three ways
automation hurts a transceiver are a stuck transmitter, transmitting into a high
SWR, and too much duty cycle at high power. Every transmit step below is designed
to show that one of those protections works before the node is trusted with it.

Manual references are to ICOM's *IC-7300 Full Manual* (`IC-7300_ENG_FM_12b`): page,
and line of the text copy in the project files (`reference/IC-7300_ENG_FM_12b.txt`).
The independent byte-by-byte audit of the driver against that manual is in
[civ-audit.md](civ-audit.md).

## Bring-up stages

`station.commissioned` in the config records how far this plan has got on this
radio. **The software refuses any command whose stage has not been reached**, so a
step cannot be skipped by running the wrong command, and the service cannot start
early:

| `station.commissioned` | Set after | Allows, in addition | Transmits? |
|---|---|---|---|
| `none` (default) | | `radio check`, `radio status`, `radio rx`, `listen` | Never |
| `link` | step 1 | `radio setup` (writes settings) | Never |
| `setup` | step 4 | `radio tune` | A 2-3 s tuner carrier |
| `tune` | step 5 | `radio cw` | CW, at most 10 W |
| `keying` | steps 6, 7 and 8 | `power_watts` above 10 | CW at the configured power |
| `done` | steps 9 and 10 | `run` (the node, and the service) | Unattended |

Raise it one stage at a time, only after the step that sets it has passed. Set it
back to `none` and start again from step 1 if the radio's firmware, the cable or the
computer changes.

### What the software enforces by itself

These hold whatever the stage or config, and are covered by unit tests:

- **Serial control lines are dropped.** With USB SEND or USB Keying (CW) or (RTTY)
  set to DTR or RTS, a raised line transmits or holds the key down (p. 12-11, lines
  6895-6927). Linux and macOS raise both lines when a serial port is opened; the
  driver lowers them straight after opening and will not use the port if it
  cannot. The radio's Inhibit Timer at USB Connection only delays such a signal by
  "a few seconds" (line 6945), so these items must also be OFF, and the preflight
  checks that they are.
- **Read-only preflight before any write.** `setup`, `tune`, `cw` and `run` first
  read, and refuse unless: the radio answers `19 00` as an IC-7300 (94h); it is on
  receive (`1C 00`); USB SEND, USB Keying (CW) and USB Keying (RTTY) are all OFF
  (`1A 05 00 78`, `00 79`, `00 80`); SPLIT is off (`0F`); ∂TX is off (`21 02`). `run`
  also requires the radio's Time-Out Timer (CI-V) to be set (`1A 05 00 29`).
  `radio check` runs the same reads and prints them.
- **Read-back after setup.** After setting the radio up, the node reads back the
  frequency (`03`, and the transmit frequency `1C 03`), mode (must be CW, not CW-R),
  break-in (must be semi), split, ∂TX, RF power (never above what was sent), key
  speed and break-in delay, and stops if any differs. An OK only means the radio
  accepted a command.
- **Only two frames can transmit:** `17` (CW text, at most 30 characters, which the
  radio's keyer sends and then stops) and `1C 01 02` (one tuner cycle). The driver
  refuses to send `1C 00 01` (force transmit), refuses frequencies outside the
  radio's 30 kHz-74.8 MHz, and treats any reply that is not exactly the documented
  shape as an error, never as a guess.
- **Every listening window starts from a known state.** Before its tune the node
  checks the radio reads receive, sends the settings again (someone may have used
  the front panel since), and checks that split and ∂TX are still off and `1C 03`
  reads the configured frequency. If any of that fails it forces receive and stays
  silent until the next window.
- **Transmit checks.** A tuner that cannot match bypasses itself (p. 11-2, line
  5917); the node then stays silent for that listening window. Any tuner error
  forces receive. SWR is measured at the start of every transmission. Every SWR
  sample also reads `1C 00`: output on the Po meter while the radio reads receive
  means none of the node's receive confirmations can be trusted.
- **Transmit inhibit.** If the radio cannot be confirmed back on receive, or its
  status reads receive while there is output, the node stops transmitting and
  writes `tx-inhibited` in its state directory, with the time and the reason.
  While that file exists nothing transmits, also after a restart (systemd restarts
  the service after a crash). Remove it only once you know what happened.
- **Config limits.** Baud must be one of the radio's CI-V USB rates and the CI-V
  address within 02h-DFh; frequency inside the transmit coverage table; power
  1-100 W, and at most 10 W until stage `keying`.

## Stop immediately if

Stop the test, force the radio to receive, and do not continue until you know why, if:

- The radio is transmitting (TX indicator lit, power output on the meter) when
  nothing should be keying it, or keeps transmitting after an `hfnode` command has
  exited.
- A command listed as not transmitting (`check`, `status`, `rx`, `setup`, `listen`,
  `run` outside a window) makes the radio transmit.
- `radio check`, or the preflight in front of any command, prints a FAIL you did not
  expect, or a value that differs from the radio's own screen.
- SWR on the radio's own meter is above 2:1, or the SWR the node logs differs from
  the radio's meter by more than about 0.3.
- Output power on the Po meter (or an external wattmeter) is higher than the
  configured `power_watts`.
- After `setup`, the frequency, mode or power on the radio's display does not match
  the config, or the node reports `the radio's settings do not read back as set`.
- Any CI-V command fails with `radio rejected the command (NG)` or
  `unexpected reply`, in a step that is expected to pass.
- The node reports `radio not confirmed on receive`, `health.csv` gets a
  `tx-status` line, or `tx-inhibited` appears in the state directory.
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

1. Turn the radio off: hold POWER for 2 seconds until "POWER OFF..." shows (line
   1271), or remove its DC supply. (If the radio shows its overheat protection, it
   has already stopped transmitting: leave it on so the fan can cool it, line
   7333.)
2. `Ctrl-C` an `hfnode` command, then run `hfnode radio --config $C rx`. Ctrl-C does
   **not** send the stop command: the radio's keyer finishes the text it was already
   given (up to 30 characters) before returning to receive. At 6 wpm that can be
   over a minute, so prefer option 1 if the radio misbehaves. A command holds the
   serial port exclusively while it runs, so `rx` from a second terminal cannot
   open it until the first command has exited.
3. For the service: `sudo systemctl stop hfnode`. The unit then runs
   `hfnode radio ... rx` (see `deploy/hfnode.service`).

After option 1, also stop `hfnode` (option 2 or 3) **before** switching the radio
back on. A running node does not take the radio being off as a reason to stop: if
it was off at the start of a listening window the node inhibits transmitting, but
if it comes back within the same window the node answers the next call it hears.

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
- `station.power_watts = 10` (raised only in step 9; the software refuses more
  until stage `keying`);
- `station.commissioned = "none"` (see [Bring-up stages](#bring-up-stages));
- `state_dir` to a scratch directory, for example `"/home/pi/bench-state"`, and
  `auth.key_file` to a scratch key made with `hfnode keygen --out ~/bench.key`. Do
  not test with the node's real key and state, because test exchanges use up codes.

**Radio settings.** Set the IC-7300 menu settings listed in
[raspberry-pi-setup.md, section 6](raspberry-pi-setup.md#6-ic-7300-settings),
including CI-V address 94h, CI-V USB baud 115200, USB SEND and both USB Keying items
OFF, and Time-Out Timer (CI-V) 3 min. Photograph each screen. Also:

- Record the firmware version (MENU > SET > Others > Information > Version, line
  7876). The CI-V `1A 05` item numbers the driver uses are those of manual revision
  12b; step 1 confirms they match this firmware.
- On the main screen: SPLIT off, XIT (∂TX) off, RIT off, CW mode, and **RF POWER at
  0%**, so that anything unexpected before step 4 happens at minimum power.

**Hardware.** A 50-ohm dummy load rated well above the test power, connected
directly to the ANT connector, for step 1 (the first time the USB cable goes in)
and every step from 4 on. Steps 2 and 3 only receive and need the antenna. Nothing
plugged into the KEY jack, no microphone (its PTT transmits) and VOX off, nothing
on the ACC socket or the REMOTE jack, and no external amplifier,
antenna switch or relay in the line (the manual warns that slower external
equipment can reflect power back into the IC-7300: TX Delay item, p. 12-5, line
6272). If you have one, an external wattmeter/SWR meter between the radio and the
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
tuner matches and one beyond its range (the window stays silent), listening windows
(a high-SWR lockout cleared by the next window's tune), and radio faults: SWR rising
after the tune, power fold-back, stuck transmit or key (also
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

Edges measured on 2026-10-04 (default grid, 3 trials per cell, 4 at once; the
same success counts in five sweeps but for one trial, machine-keyed 5 wpm at 10 dB):

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
A trial's seed (`audio_seed` in the CSV) fixes its audio, not its outcome: the node
and the mock radio run on the wall clock, so a re-run, especially under a different
load or `--jobs`, can turn a trial at an edge either way and changes the extra
transmissions a little. Compare a Pi's matrices with these give or take one trial
per cell at the edges.

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
the manual's figures; step 7 measures them on the radio. The hardware PTT timer
(step 10) cannot be tested in software at all.

## Step 0: check every CI-V command against ICOM's IC-7300 guide

No radio needed. This is a desk check, and it must be finished before any step that
writes to the radio (step 4 onward). It has been done once independently, with
every finding and fix, in [civ-audit.md](civ-audit.md); this step is your own
check of the same commands.

Each command in `crates/civ/src/ic7300.rs` cites ICOM's *IC-7300 Full Manual*
(English, revision `IC-7300_ENG_FM_12b`), Section 19 "CONTROL COMMAND", which is
ICOM's CI-V reference for the IC-7300: data format p. 19-2, command table pp. 19-3 to
19-8, data content descriptions pp. 19-9 to 19-15. A text copy is in the project
files at `reference/IC-7300_ENG_FM_12b.txt`; the line numbers below are in that
copy. (Earlier versions of the code took these values from the IC-7300**MK2** CI-V
guide; they have since been re-cited from the IC-7300 manual, and this step confirms
that independently.)

Open the manual (preferably the PDF from ICOM, not only the text copy) and check
each row. If you also have ICOM's separate IC-7300 CI-V Reference Guide, check it
against that too. Tick a row only if the manual says exactly what the code does.

Frames the node sends look like `FE FE 94 E0 <cmd> <sub> <data> FD` and the radio
answers `FE FE E0 94 ... FD`. All examples use address 94h. A read is the command
sent with no data; the reply repeats the command and adds the data.

**Commands that write or transmit:**

| # | Item | What the code sends or expects | Check in the manual | OK |
|---|---|---|---|---|
| 0.1 | Frame format | Preamble `FE FE`, to-address, from-address, command, optional sub-command, data, end `FD` | p. 19-2, data format (line 8551) | ☐ |
| 0.2 | Addresses | Radio `94`, controller `E0`. The config accepts CI-V addresses 02h to DFh only | p. 19-2 (94h is the IC-7300 default; the MK2's is B6h); CI-V Address item, p. 12-10: "Range: 02h ~ 94h ~ DFh" (line 6825) | ☐ |
| 0.3 | OK / NG replies | `FB` = OK, `FA` = NG. Every set command must be answered `FB` or it is treated as failed | p. 19-2 (lines 8551-8552) | ☐ |
| 0.4 | Frequency data | 5 bytes BCD, lowest digits first: 7,030,000 Hz = `00 00 03 07 00`. In a reply the last byte (1000 MHz and 100 MHz digits) must be `00`. Frequencies outside 30 kHz to 74.8 MHz are refused before anything is sent | p. 19-9, frequency data; receiver coverage p. 16-2 (line 8014) | ☐ |
| 0.5 | `03` read frequency | Sends `03`; expects `03` + exactly 5 frequency bytes | p. 19-3 (line 8579) | ☐ |
| 0.6 | `05` set frequency | `05 00 00 03 07 00` for 7.030 MHz | p. 19-3 (line 8581), p. 19-9 | ☐ |
| 0.7 | `06` set mode | `06 03 01`: mode `03` = CW, filter `01` = FIL1 | p. 19-3 (line 8582); mode and filter codes p. 19-9 (CW is 03, CW-R is 07) | ☐ |
| 0.8 | Level data | 2 bytes BCD, high digits first: level 102 = `01 02`. A reply must be exactly 2 bytes, at most `02 55` | the `00 00` to `02 55` ranges on the 14 and 15 rows, p. 19-3 | ☐ |
| 0.9 | `14 0A` RF power | `14 0A` + level. The code maps watts linearly, `round(watts x 255 / 100)`: 10 W = `00 26`, 25 W = `00 64`, 40 W = `01 02`, 50 W = `01 28`. The read-back must not be above what was sent | p. 19-3: "Send/read [RF PWR] position (00 00=max. CCW, 02 55=max. CW)" (line 8677). This is a knob position, not watts, so linearity is an assumption: step 9 measures it | ☐ |
| 0.10 | `14 0C` key speed | `14 0C` + level, `00 00` = 6 wpm to `02 55` = 48 wpm, linear: 18 wpm = `00 73` | p. 19-3, 14 0C (line 8685) | ☐ |
| 0.11 | `14 0F` break-in delay | `14 0F` + level, `00 00` = 2.0 dots to `02 55` = 13.0 dots, linear: 10.0 dots = `01 85` | p. 19-3, 14 0F (line 8698). Only the end points are given: step 4 compares the display | ☐ |
| 0.12 | `16 47` break-in | `16 47 01` = semi break-in ON; must read back `01`. The node never sends `02` (full break-in) | p. 19-3, 16 47 (line 8767) | ☐ |
| 0.13 | `17` send CW | `17` + up to 30 characters; expects `FB` | p. 19-4 (line 8790) and p. 19-13: "Up to 30 characters" (line 9711); allowed characters 0-9, A-Z, a-z, / ? . - , : ' ( ) = + " @ and space, which is exactly what `cw::is_sendable` allows (`crates/cw/src/morse.rs`). Footnote *2 (p. 19-8): sent as CW only in CW mode with TRANSMIT, an external TX switch, or break-in ON | ☐ |
| 0.14 | `17 FF` stop CW | `17 FF` | p. 19-13: "FF" stops sending CW messages (line 9733) | ☐ |
| 0.15 | `1C 00` TX state, read | Reads `1C 00`; `00` = receive, `01` = transmit, anything else is an error | p. 19-7 (lines 9319-9323) | ☐ |
| 0.16 | `1C 00` TX state, set | Only ever sends `1C 00 00` (receive). The driver refuses `1C 00 01` without sending anything (`set_transmit(true)` returns an error; test `never_forces_transmit_on`) | p. 19-7 | ☐ |
| 0.17 | `1C 01` tuner, start | `1C 01 02` = tune | p. 19-7: 00 = tuner OFF, 01 = ON, 02 = "Send/read to tuning" (line 9327) | ☐ |
| 0.18 | `1C 01` tuner, read | `02` = still tuning; `01` = tuner ON (matched); `00` = tuner OFF, which after a tune means it could not match and bypassed itself, so the window is locked out. Anything else is an error. Gives up after 15 s; any tuner error forces receive | p. 19-7 (line 9327); p. 11-2: "If the tuner cannot tune, "TUNE" disappears and the tuning circuit is automatically bypassed" (line 5917). The manual does not say how long `02` is reported; step 5 confirms it on the radio | ☐ |

**Reads only:**

| # | Item | What the code sends or expects | Check in the manual | OK |
|---|---|---|---|---|
| 0.19 | `15 12` SWR meter | Reads `15 12`; reply `15 12` + 2 BCD bytes. Converted with 0000 = 1.0, 0048 = 1.5, 0080 = 2.0, 0120 = 3.0, linear between points, extrapolated above 0120 (`swr_from_meter`) | p. 19-3, 15 12: the same four points (line 8736) | ☐ |
| 0.20 | `15 11` Po meter | Reads `15 11` + 2 BCD bytes; 0000 = 0%, 0143 = 50%, 0213 = 100%, linear between (`po_from_meter`). SWR readings count only while it shows output | p. 19-3, 15 11 (line 8732) | ☐ |
| 0.21 | `19 00` transceiver ID | Must answer `94` before anything is written | p. 19-4 (line 8793) | ☐ |
| 0.22 | `04` read mode | Mode then filter, coded as for `06`; must read `03` (CW) after setup | p. 19-3 (line 8580) | ☐ |
| 0.23 | `0F` split | `00` = OFF, required | p. 19-3 (line 8624) | ☐ |
| 0.24 | `21 02` ∂TX | `00` = OFF, required | p. 19-7 (line 9348) | ☐ |
| 0.25 | `1C 03` transmit frequency | 5 frequency bytes as in 0.4; must equal the set frequency | p. 19-7 (line 9332) | ☐ |
| 0.26 | `1A 05 00 78`, `00 79`, `00 80` | USB SEND, USB Keying (CW), USB Keying (RTTY): `00` = OFF (required), `01` = DTR, `02` = RTS | p. 19-5 (lines 8986, 8991, 8995) | ☐ |
| 0.27 | `1A 05 00 29` | Time-Out Timer (CI-V): `00` = OFF, `01` = 3 min to `05` = 30 min. `run` refuses OFF | p. 19-4 (line 8861) | ☐ |
| 0.28 | `1A 05 01 97`, `1A 05 00 74` | Inhibit Timer at USB Connection: `00` = OFF (warning), `01` = ON. CI-V USB Port: `00` = Link to [REMOTE] (warning), `01` = Unlink | p. 19-7 (line 9269); p. 19-5 (line 8975) | ☐ |
| 0.29 | `1A 05 00 71`, `00 75`, `00 84`, `01 61` | Reported only: CI-V Transceive, USB Echo Back (raw value), meter peak hold (warning if ON), keyer dot/dash ratio (warning unless `30`, 1:1:3) | pp. 19-5 and 19-6 (lines 8966, 8978, 9006, 9185) | ☐ |
| 0.30 | `27 11` scope data output | Reads `27 11`; `00` = OFF, `01` = ON (warning: the radio streams `27 00` waveform frames to the port, which slow the stop commands after a timeout) | p. 19-14: "Send/read the Scope wave data output (00=OFF, 01=ON)" (lines 9353-9361) | ☐ |
| 0.31 | USB echo back | Frames not addressed to E0 from 94 are skipped, so an echoed copy of the node's own frame is ignored | CI-V USB Echo Back item, p. 12-11 | ☐ |
| 0.32 | CI-V Transceive | Frames the radio sends unasked when its frequency or mode is changed at the front panel (`FE FE 00 94 00 ...` and `... 01 ...`) are skipped like the echo, also while reading the link quiet after a timeout | CI-V Transceive (default ON) and "The default transceive address is 00h", p. 12-10 (line 6843); commands 00 and 01, p. 19-3 | ☐ |
| 0.33 | Serial link | DTR and RTS low straight after opening; port opened exclusively; 8 data bits, no parity, 1 stop bit, no flow control; baud one of 4800, 9600, 19200, 38400, 57600, 115200 | USB SEND and USB Keying items, p. 12-11 (lines 6895-6927); baud options (lines 6869-6872). The manual does not give the character format: 8N1 is what CI-V software uses, and step 1 shows it works | ☐ |
| 0.34 | Unit tests | `cargo test -p civ` passes, and the bytes in the `frames_on_the_wire` and `transmit_control_and_read_frames_on_the_wire` tests match the rows above | `crates/civ/src/ic7300.rs` | ☐ |

**Pass:** every row ticked. **Fail:** any difference. Fix the code and its citation,
update the unit test, and repeat step 0. Do not run steps 4 onward against a command
that has not been ticked.

The `1A 05` item numbers (0.26 to 0.29) are the ones most likely to differ between
firmware versions; step 1 checks two of them against the radio's screen.

A related manual item that is useful during testing but not used by the node:
`14 09` reads the CW pitch ("01 28=600 Hz", p. 19-3).

## Step 1: serial link, read only (sets stage `link`)

Radio on, **dummy load on ANT**, KEY jack empty, RF POWER at 0%, and
`commissioned = "none"`. Connect the USB cable, then:

```sh
hfnode radio --config $C check
hfnode radio --config $C status
```

`check` only reads: `19 00`, `1C 00`, `1A 05 00 78`, `00 79`, `00 80`, `0F`, `21 02`,
`1A 05 00 29`, `1A 05 00 74`, `1A 05 01 97`, `1A 05 01 61`, `1A 05 00 84`, `27 11`,
`03`, `1C 03`, `04`, `14 0A`, `16 47`, `1C 01`, `1A 05 00 71`, `1A 05 00 75`, each sent
with no data, which reads the item. To see every frame on the wire, with the time
each reply took, put `RUST_LOG=civ=trace` in front of the command.

**Look for:** one line per item, each PASS, WARN or info, then `preflight passed;
nothing was written to the radio`. `status` prints `frequency N Hz` and
`transmitting: false`.

**Pass:**

1. No FAIL line, and `preflight passed` printed. A WARN is acceptable only if you
   understand it (for example the Time-Out Timer, which only `run` requires).
2. Every value matches the radio's own screen: frequency to the hertz, mode, RF
   power about 0%, break-in, tuner, Time-Out Timer, the USB items.
3. **The item numbers match this firmware.** On the front panel change Time-Out
   Timer (CI-V) from 3 to 5 min and CI-V USB Echo Back from OFF to ON; run `check`
   again and see both values change; put both back. Turn the VFO dial and see the
   frequency follow in `status`. If a value does not follow, the radio's firmware
   numbers the `1A 05` items differently from the manual: stop and report it.
4. Write down the raw Echo Back value read with Echo Back OFF. The command table
   says `00=ON, 01=OFF` (line 8978) but the menu's default is OFF (line 6879); the
   driver works either way, and this settles which way round the table is.
5. The radio did not transmit at any point.

**Fail:**

- `opening radio on /dev/...`: wrong `serial_port`, or your user is not in `dialout`.
- `no reply from radio`: baud rate or CI-V address mismatch between the radio menu and
  the config, or CI-V USB Port linked to REMOTE at a different speed.
- `transceiver ID` FAIL: the radio on the port is not an IC-7300 at 94h.
- Any other FAIL line: change that item on the radio's front panel
  (raspberry-pi-setup.md, section 6) and run `check` again.
- `radio rejected the command (NG)` or `unexpected reply`: stop; recheck step 0.

**Then** set `commissioned = "link"`. Steps 2 and 3 only use the radio's audio, so
they can be done now with the antenna connected; put the dummy load back before
step 4.

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

## Step 4: put the radio in the node's state (no transmit, sets stage `setup`)

From here on: **dummy load on ANT**, KEY jack empty, step 0 complete.

```sh
hfnode radio --config $C setup
```

This first runs the read-only checks of step 1 and stops if any fails. It then
sends, in order: force receive (`1C 00 00`), frequency (`05`), CW mode FIL1
(`06 03 01`), RF power (`14 0A 00 26` for 10 W), key speed (`14 0C`), break-in delay
(`14 0F 01 85` for 10.0 dots), semi break-in (`16 47 01`), and reads every one of
them back.

**Look for:** `configured and read back: <freq> Hz, CW, 10 W, 18 wpm`. On the radio:
the frequency, CW mode, FIL1, BK-IN shown on the display, RF power about 10%, keyer
speed 18 wpm, break-in delay 10.0d.

**Pass:** the command succeeds, the display matches, the radio did not transmit.
Write down the key speed and break-in delay the radio displays: the manual gives
only the end points of those two scales, so these readings confirm the linear
mapping the driver uses. **Then** set `commissioned = "setup"`.

**Fail:** any error, or any setting on the display differs from the config.
`the radio's settings do not read back as set (...)` names a setting the radio
accepted (`FB`) but holds at a different value: a byte value is wrong, so go back to
step 0.

## Step 5: tuner into the dummy load (transmits briefly, sets stage `tune`)

A tuner cycle is 2 to 3 seconds of carrier (p. 11-2, line 5911). Watch the Po meter.

```sh
hfnode radio --config $C tune
hfnode radio --config $C status
```

**Look for:** the checks and setup of step 4, then the radio transmits briefly and
shows the tuner working, then `tuned`. A line `health: tune NNNms` in the log, and
a `<time>,tune,NNNms` line in `<state_dir>/health.csv`. `status` reports
`transmitting: false`. Note the Po reading while it tunes (whether the tuner uses
the set power or its own) and how long TUNE blinks. Run it once with
`RUST_LOG=civ=trace` and note what `1C 01` reads during and after the tune (`02`
while tuning, then `01`, is what the node expects; a warning that the tuner never
read `02` means it does not report tuning that way).

**Pass:** the tune finishes in a few seconds, Po stays at or below about 10%, the
radio is back on receive, and `health.csv` has the tune line. **Then** set
`commissioned = "tune"`.

**Fail:** `no reply from radio` after about 15 s (the node then forces receive; check
step 0.18), `tuner could not match the load` (a 50-ohm dummy load should always
match: check the load and its cable), or the radio stays on transmit.

## Step 6: short CW and the SWR reading (transmits)

Steps 6, 7 and 8 together make up stage `keying`, all at 10 W into loads. Set the
radio's meter to SWR. Monitor with the radio's sidetone or a nearby receiver.

```sh
hfnode radio --config $C cw "VVV DE N0CALL"
hfnode radio --config $C status
```

The node reads SWR (`15 12`) repeatedly during the first second of the first piece
it keys, counting only readings taken while the Po meter (`15 11`) shows output.
Each reading also reads the transmit status (`1C 00`), which must say transmit
while there is output. The manual does not say whether text keyed with `17` shows
as transmit there; this step is where that is first seen.

**Look for:** the CW sent correctly at 18 wpm, `health: swr 1.0x` in the log, a
`<time>,swr,1.0x` line in `health.csv`, `sent; see health.csv for the SWR reading`,
then `transmitting: false`.

**Pass:** text sent correctly; logged SWR is 1.3 or less and within about 0.2 of the
radio's own SWR meter; the radio returns to receive when the text ends.

**Fail:** wrong characters sent, SWR reading far from the radio's meter (check step
0.19), or the radio does not return to receive. If the node stops with
`radio not confirmed on receive: transmit inhibited ...` and `health.csv` has a
`tx-status` line, the radio reported receive while its Po meter showed output:
stop and report it, because the node's checks that the radio is back on receive
depend on that status. (Remove `tx-inhibited` from the state directory before
transmitting again.)

## Step 7: software watchdog (transmits)

This checks that the node forces the radio back to receive when one keying run lasts
too long, and that `17 FF` and `1C 00 00` actually work. It comes before the
high-SWR test because every stop the node makes relies on those two commands, and
the manual does not say that either one ends a message the keyer is already
sending.

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

Run it once more with `RUST_LOG=civ=trace` in front of the command, and check that
the radio answered `FB` (OK) to both `17 FF` and `1C 00 00`. The node sends the two
together, so the trace is what shows whether each one works on its own: an `FA`
(NG) to either means only the other stopped the keyer. Write down which.

Restore `key_speed_wpm = 18` and `max_key_seconds = 45`.

## Step 8: high-SWR lockout (transmits briefly into a mismatch)

This needs a deliberately mismatched but safe load: a non-inductive 100-ohm load
(about 2:1) or 150-ohm load (about 3:1) rated for the test power. **Never** test with
an open or shorted connector. If you do not have such a load, get one before going
on: stage `keying`, and every step after it, needs this one to have passed.

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

Switch the tuner back on and reconnect the 50-ohm dummy load. **Then**, with steps
6, 7 and 8 passed, set `commissioned = "keying"`.

## Step 9: power calibration (transmits)

The code assumes the `14 0A` scale is linear from 0 to 100 W. Check it at each power
you might use. Power above 10 W is refused until stage `keying`. For each of 10, 25,
40 and 50 W: set `station.power_watts` in `~/bench.toml`, then

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

## Step 10: hardware PTT timer

The design calls for a hardware timer that ends any transmission after about 60 s
regardless of software, as the last line of defence against a stuck transmitter.

**Read this first.** The node does not use a PTT or keying line: it keys CW with CI-V
command `17`, through the radio's internal keyer. A timer wired in series with the
KEY jack or a PTT line will therefore **not** stop a transmission started by the
node. The timer has to detect transmit in a way that works for CI-V keying and act
on something that ends the transmission without any software. Write down what it
senses and what it interrupts, and keep to these rules, because a careless timer
can itself key or damage the radio:

- **Sensing.** The rear [SEND] jack "goes low when the transceiver transmits"
  (p. 18-4, line 8509), and the manual describes it only as an output. Pin 3 (SEND)
  of the ACC socket goes low too, but it is also an input: below +0.8 V it **makes
  the radio transmit** (p. 18-2, lines 8265-8276). The manual does not say whether
  the two are the same line inside the radio, so treat both as able to key it.
  Sense through a high-value series resistor (100 kΩ or more) into an input that
  cannot pull the line down when the timer is unpowered or has failed, with the
  pull-up supplied by the timer. Do not power the timer from ACC pin 8, which only
  carries 13.8 V while the radio is on. If the timer drives a relay from SEND, fit
  the diode the manual asks for (line 8360).
- **Acting.** Cut the radio's DC supply. Never switch the antenna or RF path: a
  relay that opens under carrier reflects power into the radio, which the manual
  warns may damage it (TX Delay item, line 6272). After a DC cut the radio needs its
  POWER switch pressed and comes back in its previous state.
- **First check, before any transmit test with the timer connected:** with the
  timer connected but unpowered, and then powered, the radio stays on receive (TX
  indicator off, `hfnode radio --config $C status` shows `transmitting: false`). If
  you used ACC pin 3, measure it: it must stay above 2.0 V on receive.

As an additional backstop inside the radio, set **Time-Out Timer (CI-V)** (MENU >
SET > Function, p. 12-5) to its shortest setting, 3 minutes; `run` refuses to start
while it is OFF. The manual says it applies to transmitting "initiated by a CI-V
command or pushing TRANSMIT" (line 6283). It is too long to replace the hardware
timer. Check that it works: in CW mode with nothing on the KEY jack, push TRANSMIT
(with the key up the radio sends no carrier; the Po meter should stay at zero) and
confirm the radio returns to receive by itself after 3 minutes. Whether it also
covers text keyed with `17` cannot be tested without a three-minute keyer message,
which the node never sends, so the hardware timer remains the backstop.

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

**Then**, with steps 9 and 10 passed, set `commissioned = "done"`. This allows
`hfnode run`, and with it the service.

## Step 11: full exchange into dummy loads (optional, recommended)

A complete transaction with no signal on the air: the field rig transmits into its
own dummy load a few metres from the node, at its lowest power, so the node hears it
by leakage. Use the bench key and state.

In `~/bench.toml` set `schedule.always = true`, configure one `[[contacts]]` entry
with your own email address, and the `[email]` settings. Print a few codes:

```sh
hfnode codes --config $C --count 10
```

This is the first `run` (it needs stage `done`, and runs the preflight first,
including the Time-Out Timer check). Run the node in the foreground, with the
secrets loaded:

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
hfnode listen --config $C                    # first listen; Ctrl-C when sure it is clear
hfnode radio --config $C cw "QRL? DE N0CALL" # then listen again; continue only if clear
hfnode radio --config $C tune
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
| 1 Check and status (`link`) | | | firmware: / echo back raw with OFF: / TOT and echo followed: |
| 2 Listen | | | |
| 3 WAV decode | | | |
| 4 Setup (`setup`) | | | key speed shown: / BKIN delay shown: |
| 5 Tune (`tune`) | | | tune ms: / Po while tuning: |
| 6 Short CW, SWR | | | node SWR: / radio SWR: |
| 7 Watchdog | | | stopped after: s |
| 8 SWR lockout (`keying`) | | | load: / node SWR: |
| 9 Power | | | 10 W: / 25 W: / 40 W: / 50 W: |
| 10 Hardware timer (`done`) | | | set: s / stopped after: s / TOT returned after: |
| 11 Dummy-load exchange | | | |
| 12 On air | | | SWR: / report: |
| 13 End to end | | | |
