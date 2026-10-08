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

These hold whatever the stage or config. Where a bullet names tests (in
`cargo test --all`, or scenarios of `hfnode selftest`), those tests fail if that
protection is taken out of the code; they run against the mock radio, so they show
what the code does, not what the radio does.

- **Serial control lines are dropped.** With USB SEND or USB Keying (CW) or (RTTY)
  set to DTR or RTS, a raised line transmits or holds the key down (p. 12-11, lines
  6895-6927). Linux and macOS raise both lines when a serial port is opened, and
  on Windows it is not documented whether the CP210x driver does for an instant;
  the driver asks for DTR off when opening and lowers DTR, then RTS, straight
  after opening (every system), and will not use the port if either fails. The
  keyer box's and the handheld's ports are opened the same way. The radio's
  Inhibit Timer at USB Connection only delays such a signal by "a few seconds"
  (line 6945), so these items must also be OFF, and the preflight checks that they
  are. How long a line is up inside the system's open, before the driver can lower
  it, has not been measured: see [DTR and RTS at port open](#dtr-and-rts-at-port-open-h1-optional-no-radio).
  Tests: `civ::serial` (`dtr_then_rts_go_down`,
  `a_line_that_will_not_go_down_is_an_error`) and `civ::ic7300`
  (`open_lowers_dtr_then_rts_before_the_driver_is_returned`,
  `open_fails_if_either_line_cannot_be_lowered`).
- **Read-only preflight before any write.** `setup`, `tune`, `cw` and `run` check
  the stage and power before they open the port, then first read, and refuse with
  nothing written unless: the radio answers `19 00` as an IC-7300 (94h); it is on
  receive (`1C 00`); USB SEND, USB Keying (CW) and USB Keying (RTTY) are all OFF
  (`1A 05 00 78`, `00 79`, `00 80`); SPLIT is off (`0F`); ∂TX is off (`21 02`). `run`
  also requires the radio's Time-Out Timer (CI-V) to be set (`1A 05 00 29`).
  `radio check` runs the same reads and prints them. `radio rx` is the one command
  that writes without a preflight: it sends only the stop and receive commands
  (`17 FF`, `1C 00 00`), so that it can take off transmit a radio the preflight
  would refuse. Tests: `commissioning` (`a_refused_stage_or_power_opens_nothing_and_sends_nothing`,
  `nothing_is_written_after_a_failed_preflight`,
  `run_needs_the_radios_time_out_timer_and_the_bench_commands_do_not`,
  `the_first_write_after_the_preflight_puts_the_radio_on_receive`), and the
  selftest scenarios `preflight-tot-off` and `preflight-usb-send-dtr`.
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
- **Every tune and every transmission starts from a known state.** Before each
  tune, and again before each transmission, the node checks the radio reads
  receive, sends the settings again (someone may have used the front panel since),
  and checks that split and ∂TX are still off and `1C 03` reads the configured
  frequency. If any of that fails before a tune it forces receive and stays silent
  until the next tune; before a transmission it forces receive and keys nothing,
  and the next transmission checks again. While it listens and hears nothing it
  does the same every `schedule.check_minutes` (10), without transmitting.
- **When it tunes.** At start-up, at the top of each listening window if it uses
  them, and just before a reply once the last tune is older than
  `schedule.retune_minutes` (60). A window's tune happens when the schedule opens
  it, also if the node is still listening on from the last one (or just after, if
  it was transmitting as the window opened). A tune at start-up or at a window's
  top that matches is followed by `DE <node_call>`; a tune before a reply is
  identified by the reply. Listening all the time (the default), an idle node
  transmits nothing after its start-up tune and ID.
- **Transmit checks.** A tuner that cannot match bypasses itself (p. 11-2, line
  5917); the node then stays silent until its next tune. Any other tuner error (no
  reply to the tune command, a tuner state that cannot be read, or still tuning
  after 20 s) forces receive and keeps the node silent until it has tuned again,
  which it does before its next reply; if the tuner still reads tuning (`1C 01`
  reads `02`) after that, the node inhibits transmitting. Every piece the keyer
  sends, the first of a transmission and every later one, is watched from just
  after it starts until the radio is back on receive: each sample reads the Po
  meter (`15 11`), the transmit status (`1C 00`) and SWR (`15 12`). SWR above
  `swr_limit` stops the transmission at once; so does no output on the Po meter
  for 40 samples in a row, and at least a second, while the keyer is sending (the
  radio's protection reduces its output once its power amplifier is hot, p. 13-4,
  lines 7316-7324). Output on the Po meter while the radio reads receive means none
  of the node's receive confirmations can be trusted: transmitting is inhibited.
  Tests: `station` (`swr_is_watched_on_every_piece_not_just_the_first`,
  `swr_is_watched_for_the_whole_of_a_piece`,
  `output_lost_after_the_first_piece_stops_keying`,
  `a_tuner_still_tuning_after_its_time_limit_inhibits`,
  `a_lost_tune_reply_locks_out_until_the_next_tune`), and the selftest scenarios
  `fault-high-swr-mid-over`, `fault-tune-hang` and `fault-tune-lost-reply`.
- **Station identification** (47 CFR 97.119(a)). In `run`, a tune that matched is
  followed by `DE <node_call>`, keyed with `17` and every transmit check above.
  Inside a transmission `DE <node_call>` is keyed on its own between chunks, or
  before the first chunk after a long over from the field, so that no more than 8
  minutes pass from one ID of the node's to its next (the rule allows 10). An ID 10
  minutes old no longer counts: then the time runs from the start of the next
  transmission. A tune that fails, cannot match or times out is not identified,
  because the node keys nothing into a fault. `radio tune` and `radio cw` add no
  ID: there the operator identifies (step 12).
- **Transmit inhibit.** If the radio cannot be confirmed back on receive, or its
  status reads receive while there is output, the node stops transmitting and
  writes `tx-inhibited` in its state directory, with the time and the reason.
  While that file exists nothing transmits, also after a restart (systemd or the
  start-up scripts in `deploy/` restart the node after a crash). The node reads it
  only when it starts: remove it with the node stopped, and only once you know what
  happened. `hfnode run` emails `[email] alert_to`, if set, when it latches and at
  each start while the file is there. (Each `radio` command is a start of its own,
  and does not email.)
- **Config limits.** Baud must be one of the radio's CI-V USB rates and the CI-V
  address within 02h-DFh; frequency inside the transmit coverage table; power
  1-100 W, and at most 10 W until stage `keying`.

## Stop immediately if

Stop the test, force the radio to receive, and do not continue until you know why, if:

- The radio is transmitting (TX indicator lit, power output on the meter) when
  nothing should be keying it, or keeps transmitting after an `hfnode` command has
  exited.
- A command listed as not transmitting (`check`, `status`, `rx`, `setup`, `listen`)
  makes the radio transmit, or `run` transmits anything but its tune (at start-up,
  at a window start, or just before a reply), the `DE <call>` after a tune, and its
  replies (with windows, also past a window's end while the node still listens on,
  logged as `window over: still listening ...`: then it answers whatever it would
  in a window, a new open included).
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
- The tuner does not finish (the node reports `no reply from radio` after 20 s of
  tuning; the manual's longest tune is 15 s), or `tx-inhibited` names `1C 01`: the
  tuner still read tuning after the node forced receive.
- In `run`, a tuning carrier with no `DE <node_call>` after it: the tune failed or
  the ID found a fault, and the node may stay silent this window. Find out which
  from the log before going on.
- The USB serial port or audio device drops out, the computer resets, or audio is
  distorted while transmitting. These are signs of RF getting into the USB cable
  or the computer.
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
2. `Ctrl-C` an `hfnode` command. Once the command has passed its preflight (so it
   may write to the radio), Ctrl-C sends the stop command (`17 FF`, then `1C 00 00`)
   and exits only once the radio reads receive (`radio confirmed on receive;
   exiting`), or with an error if it does not. Until step 7 has shown that the stop
   command ends a message the keyer is already sending, assume the keyer may
   finish the text it was given (up to 30 characters: 30 zeros take 44 s at 18 wpm
   and 131 s at 6 wpm), so prefer option 1 if the radio misbehaves. Run
   `hfnode radio --config $C rx` afterwards if in doubt. A command holds the serial port exclusively while it
   runs, so `rx` from a second terminal cannot open it until the first command has
   exited.
3. For the node started at boot or log-in: on Linux `sudo systemctl stop hfnode`;
   on a Mac Ctrl-C in its Terminal window (or `launchctl bootout` for the launchd
   agent); on Windows Ctrl-C in its window, or, from the repository folder,
   `powershell -NoProfile -ExecutionPolicy Bypass -File deploy\windows\stop-hfnode.ps1`.
   Each then runs `hfnode radio ... rx` (see `deploy/`). On Windows, closing the
   window does not stop the keyer or check receive; run the stop script after it.

After option 1, also stop `hfnode` (option 2 or 3) **before** switching the radio
back on. A running node does not take the radio being off as a reason to stop: if
it finds the radio off (at start-up, a window start, a tune or its idle check every
`schedule.check_minutes`) it inhibits transmitting (and emails `[email] alert_to`),
but if the radio comes back on before then, the node answers the next call it hears.

## Before you start

**Bench config.** Make a copy of the config for testing, so you can change values
without touching the production file:

```sh
cp hfnode.example.toml ~/bench.toml
C=~/bench.toml
```

On Windows, in PowerShell: `Copy-Item hfnode.example.toml ~\bench.toml` and
`$C = "$HOME\bench.toml"`. The `hfnode` commands below then work with `$C`, with
two changes: where a command starts with `RUST_LOG=info,civ=trace`, run
`$env:RUST_LOG = "info,civ=trace"` first and the command without it (and
`Remove-Item Env:RUST_LOG` afterwards); and on the command line write `$HOME\...`
where a path starts with `~/` (`~` works only inside the config file).

In `~/bench.toml` set:

- `station.node_call`, `station.field_calls`, `station.serial_port`, and
  `audio.device` if the radio's codec is not found under the default name
  (`hfnode devices` lists the ports and audio inputs, and marks the radio's; see
  the setup guide for your computer: [Raspberry Pi or Linux](raspberry-pi-setup.md),
  [Mac](macos-setup.md) or [Windows](windows-setup.md));
- `station.frequency_hz` to a frequency inside your license privileges in the CW
  segment;
- `station.power_watts = 10` (raised only in step 9; the software refuses more
  until stage `keying`);
- `station.commissioned = "none"` (see [Bring-up stages](#bring-up-stages));
- `state_dir` to the state directory the node itself will use, not a scratch one:
  `"/var/lib/hfnode"` on a Pi (as in the example), `"~/Library/Application
  Support/hfnode/state"` on a Mac, `'~\AppData\Local\hfnode\state'` on Windows (in
  single quotes), as in the setup guide. `tx-inhibited` is kept there, so the bench
  and the node share it: an inhibit latched during a test must also stop the node
  when it starts later, and no test may key while the node's inhibit is there.
  (`hfnode` refuses a relative `state_dir`, and its commands that key refuse one
  they cannot write.) The tests also leave `last_seq`, the inbox and the health
  log there, and the node carries on from them: its code sheet, printed in step
  13, starts after the last line the tests used.
- `auth.key_file` to a scratch key made with `hfnode keygen --out ~/bench.key` (on
  Windows `--out $HOME\bench.key`). Do not test with the node's real key: the code
  sheets printed for the tests would then work on the node.

On a Pi, `/var/lib/hfnode` belongs to the `hfnode` user, and the bench commands run
as you. Make it yours until step 13, which gives it back:

```sh
sudo chown -R "$USER" /var/lib/hfnode
```

**Radio settings.** Set the IC-7300 menu settings listed in
[raspberry-pi-setup.md, section 6](raspberry-pi-setup.md#6-ic-7300-settings),
including CI-V address 94h, CI-V USB baud 115200, USB SEND and both USB Keying items
OFF, Time-Out Timer (CI-V) 3 min, and the tuner's PTT Start OFF (the preflight does
not read it yet). Photograph each screen. Also:

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

### DTR and RTS at port open (H1, optional, no radio)

The driver lowers DTR and RTS straight after the system has opened the port, but
the system may raise them for a moment inside the open itself (Linux and macOS do;
on Windows it is not documented). That moment has never been measured. It is
harmless only while USB SEND and both USB Keying items are OFF, so this is
optional, but it is the only way to know how long the lines are up.

On the exact computer the node will use, plug in a spare CP2102 breakout board, so
that it binds to the same driver as the radio's port (on a Mac, Apple's
`/dev/cu.usbserial-*` or Silicon Labs' `/dev/cu.SLAB_USBtoUART`). Put a scope or a
logic analyser on its DTR and RTS pins: an LED or a meter can miss a pulse of a few
milliseconds. Note each pin's level while nothing has the port open first (the
lines are probably active-low on the pins). Point `station.serial_port` in
`~/bench.toml` at the breakout and run `hfnode radio --config $C check` a few
times; with no radio on the breakout it fails at the first read (`no reply from
radio`), which is expected: it has still opened the port. Record any pulse on
either line at the open, how long it lasts, and that both lines end at their idle
level. With no radio the command holds the port open only until its first read
times out, so a raise that some systems are said to make later, after the open, is
not covered by this measurement.

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

Each scenario opens the radio as `hfnode run` does (the read-only preflight first)
and runs the whole node (`node::run`: decoder, parser, session, station safety
layer and the real `Ic7300` CI-V driver) against `civ::mock`, a byte-level IC-7300
that answers every command the node sends, the preflight's 21 reads among them,
as Section 19 of the manual says, and flags anything else as a protocol violation
(a write to one of the radio's menu settings, for one). A scripted field
operator keys CW audio (with noise and hand-keying jitter) into the node's audio
queue, listens to what the mock radio actually keyed and reacts: it opens, checks
the read-back, then answers `OK`, `NO` or `AGN` (each on its own line), and repeats
an open or an `OK` that got no answer. While the mock radio is on transmit, the node
hears nothing of the operator. Every scenario checks the exact text keyed, the
gateway side effects (messages sent, inbox marked read, weather requests),
`last_seq`, zero CI-V violations, the radio's settings as the node left them, that
the node forced receive after a fault and never otherwise, the station ID (`DE
N0DE` after every tune that matched, and no stretch of a transmission longer than 8
minutes without one), that the owner is told exactly once of a transmit inhibit
(when it latches, or at start-up) and never otherwise, and safety bounds (longest
key-down, longest transmit, no transmit past the break-in delay plus the stuck
margin, duty cycle, receive when the node stops, the tuner cycles expected).

The scenarios cover the field grammar end to end: TX, RX with one to many messages,
the five-message cap and truncation, WX with 4- and 6-character grids, a grid sent
as two words, known and unknown presets, WX alone reusing the last place, `FAIL`
replies (`WX NO COVERAGE` among them), `NO` and `AGN` on their own lines with their
free repeats (a bare one is ignored), `AGN` with a chunk letter, `AGN <n> <code> K
K` for chunk K and `AGN` after a read-back, `KN` keyed run together as the over, a
message whose last word is `K`, codes sent in two groups, a repeated `OK` after a
lost result (also past the end of a listening window, and silence once that result
can no longer be repeated), an open on fresh lines replacing a pending one, and the
10-minute pending-commit and `AGN` windows. Also a readout longer than the 8-minute
ID interval, identified between two chunks, lost read-backs, replayed and wrong
codes, garbled callsigns, noise bursts after `K`, sending speeds 10 to 30 wpm, SNR
20, 6, 3 and 0 dB in 2500 Hz, a sloppy hand key, the radio's sidetone in the receive
audio, USB echo off, CI-V Transceive frames from someone at the radio, a load the
tuner matches and one beyond its range (the window stays silent), listening windows
(a high-SWR lockout cleared by the next window's tune), listening all the time (a
re-tune before a reply once the last tune is old, a high-SWR lockout cleared by it,
split or ∂TX switched on at the radio, the dial and mode changed while the node is
idle, band noise and other stations calling, and a call after a long quiet spell
answered the first time), and radio faults: SWR rising
after the tune and part-way through an over, power fold-back, stuck transmit or key
(also after the last over), a transmitter that will not unkey, one that only the
watchdog gets off transmit, refused status commands, NG and lost or late CI-V
replies, a readout the radio refuses (left unread), a tuner that never finishes
(transmitting inhibited), a lost reply to the tune command (no reply until the
node has tuned again), a node that starts with transmitting already inhibited (no
tune, nothing keyed), and a radio the preflight refuses (Time-Out Timer OFF, USB
SEND set to DTR: only reads sent, nothing written).

It runs 100 times faster than real time by default (about 35 s for all of them on a
laptop). On a slow or busy Pi lower the speed with `--scale 20`; the result must not
depend on it (any scale from 1 to 200). `--scale 1` runs everything at real speed,
including the CI-V reply timeout, the watchdog and the forced-receive retries, which
stay in real time in a time-scaled run (about 23 minutes with `--jobs 100`, one job
per scenario).

**Sweep: where it breaks.** `hfnode selftest --sweep` runs a complete TX exchange
(open, read-back, `OK`, `SENT`; `--rx` adds an RX readout) for every combination of
field operator speed (5, 8, 10, 13, 15, 18, 20, 25, 30, 35 wpm), SNR in 2500 Hz
(clean, 20, 10, 6, 3, 0, -3, -6 dB) and keying (machine-keyed, and hand-keyed with
12% jitter, stretched gaps and 25 Hz off pitch), 3 trials each (`--trials`; the
grid with `--wpm`, `--snr`, `--keying`). The operator repeats unanswered
transmissions up to 3 times and answers a wrong read-back with `NO` (on the line the
`OK` would have used), then starts over once on fresh lines. A run succeeds only if
exactly the intended message reached the gateway, once, and `SENT` was keyed; a
wrong message delivered is counted separately (`W!`) and, like any safety violation
(`S!`: a failed safety, CI-V, settings, forced-receive, self-decode or station ID
check), is a hard failure at any SNR. It prints a successes/trials matrix per
keying, the share of transmissions the node decoded exactly, and the edges, and
`--csv` writes every run. It exits non-zero on a hard failure or a failed trial in
the should-pass region
(machine-keyed 10-30 wpm at 6 dB and above, hand-keyed 10-25 wpm at 10 dB and
above). About 3.5 minutes on a 4-core laptop; on a Pi 4 estimate 4 to 6 minutes at
the default scale, about 17 minutes at `--scale 20`.

Edges measured on 2026-10-04 (default grid, 3 trials per cell, 4 at once; the
same success counts in five sweeps but for one trial, machine-keyed 5 wpm at 10 dB,
and in a sweep after the station ID, `NO` and `AGN` on lines and listening past the
window were added, but for machine-keyed 5 wpm at 20 dB, 1 of 3 instead of 2 of 3):

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
| 0.18 | `1C 01` tuner, read | `02` = still tuning; `01` = tuner ON (matched); `00` = tuner OFF, which after a tune means it could not match and bypassed itself, so the window is locked out. Anything else is an error. Gives up after 20 s (the manual's longest tune is 15 s, p. 16-3, line 8119); any tuner error forces receive and locks the node out until it has tuned again, and if `1C 01` still reads `02` after that, transmitting is inhibited | p. 19-7 (line 9327); p. 11-2: "If the tuner cannot tune, "TUNE" disappears and the tuning circuit is automatically bypassed" (line 5917). The manual does not say how long `02` is reported; step 5 confirms it on the radio | ☐ |

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
| 0.33 | Serial link | DTR, then RTS, low straight after opening; port opened exclusively; 8 data bits, no parity, 1 stop bit, no flow control; baud one of 4800, 9600, 19200, 38400, 57600, 115200 | USB SEND and USB Keying items, p. 12-11 (lines 6895-6927); baud options (lines 6869-6872). The manual does not give the character format: 8N1 is what CI-V software uses, and step 1 shows it works | ☐ |
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
each reply took, put `RUST_LOG=info,civ=trace` in front of the command.

**Look for:** one line per item, each PASS, WARN or info, then `preflight passed;
nothing was written to the radio`. `status` prints `frequency N Hz` and
`transmitting: false`.

**Pass:**

1. No FAIL line, and `preflight passed` printed. A WARN about the Time-Out Timer
   is a stop: `radio tune` and `radio cw` only warn about it, but the bench steps
   need it set. Set it to 3 min (raspberry-pi-setup.md, section 6) and run `check`
   again. Any other WARN is acceptable only if you understand it.
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

- `opening radio on ...`: wrong `serial_port` (check `hfnode devices`); on Linux,
  your user is not in `dialout`; on Windows, "Access is denied" means another
  program has the COM port open.
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
hfnode record --config $C --out level.wav --seconds 10
```

It records what the node hears and prints the peak level, which should be well
below 100% on the strongest signals (it says so if the audio clipped, or if it got
only silence, which on a Mac or Windows PC usually means the microphone permission
in the setup guide). Adjust USB AF Output Level on the radio if not. Then:

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
hfnode record --config $C --out rec.wav --seconds 60
hfnode decode rec.wav --pitch 600
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
sends, in order: force receive (`1C 00 00`), CW mode FIL1 (`06 03 01`; mode first,
because with SSB/CW Synchronous Tuning on, a change from SSB to CW shifts the
frequency, p. 12-6), frequency (`05`), RF power (`14 0A 00 26` for 10 W), key speed
(`14 0C`), break-in delay (`14 0F 01 85` for 10.0 dots), semi break-in (`16 47 01`),
and reads every one of them back.

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
`RUST_LOG=info,civ=trace,hfnode::station=trace` and note what `1C 01` and `1C 00`
read during and after the tune: the node logs a `tuning, N ms: 1C 01 ..., 1C 00 ...`
line each time it asks (`02` while tuning, then `01`, is what the node expects from
`1C 01`; a warning that the tuner never read `02` means it does not report tuning
that way; `RUST_LOG=civ=trace` alone would hide that warning). The manual does not
say whether `1C 00` reads transmit during a tune: the node does not rely on it, and
this is where it is first seen. `radio tune` does not identify its carrier; into a
dummy load none is needed, and on the air you do it yourself (step 12).

**Pass:** the tune finishes in a few seconds, Po stays at or below about 10%, the
radio is back on receive, and `health.csv` has the tune line. **Then** set
`commissioned = "tune"`.

**Fail:** `no reply from radio` after about 20 s (the node then forces receive, and
inhibits transmitting if the tuner still reads tuning; check step 0.18), `tuner
could not match the load` (a 50-ohm dummy load should always
match: check the load and its cable), or the radio stays on transmit.

## Step 6: short CW and the SWR reading (transmits)

Steps 6, 7 and 8 together make up stage `keying`, all at 10 W into loads. Set the
radio's meter to SWR. Monitor with the radio's sidetone or a nearby receiver.

```sh
hfnode radio --config $C cw "VVV DE N0CALL"
hfnode radio --config $C status
```

The node reads SWR (`15 12`) repeatedly all through every piece it keys, from just
after the keyer starts until the radio is back on receive, counting only readings
taken while the Po meter (`15 11`) shows output, and logs the highest once per
transmission. Each reading also reads the transmit status (`1C 00`), which must say
transmit while there is output. The manual does not say whether text keyed with
`17` shows as transmit there; this step is where that is first seen. Run it once
with `RUST_LOG=info,civ=trace` and check that the `15 11`, `1C 00` and `15 12` reads
go on until the end of the text, not only at its start. Note how far apart one
sample's first `15 11` read is from the next sample's: 40 times that is how long
the node keys a radio that shows no output before it stops (expected 6 to 8 s).

**Look for:** the CW sent correctly at 18 wpm, `health: swr 1.0x` in the log, a
`<time>,swr,1.0x` line in `health.csv`, `sent; see health.csv for the SWR reading`,
then `transmitting: false`.

**Pass:** text sent correctly; logged SWR is 1.3 or less and within about 0.2 of the
radio's own SWR meter; the radio returns to receive when the text ends.

**Fail:** wrong characters sent, SWR reading far from the radio's meter (check step
0.19), or the radio does not return to receive. `no output while keying` into the
dummy load means the Po meter read no output for longer than the node allows while
the keyer sends (40 samples in a row, at least a second): stop and report it, with
the trace. If the node stops with
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
`watchdog: keying exceeded 5s, forcing receive`; the zeros stop; the command
exits with `transmitter did not return to receive`; `status` shows
`transmitting: false`.

**Pass:** the zeros stop about 5 seconds after they start, the radio is back on
receive within about 7.3 seconds of starting (the watchdog's 5 s, the stop
commands, and the break-in delay, which is 10 dots, 2 s at 6 wpm), and it stays on
receive. Judge by the zeros stopping, not by the TX indicator alone.

**Fail:** the radio keeps sending the zeros. Turn it off (or wait out the 44 seconds
into the dummy load) and investigate `17 FF` and `1C 00 00` (steps 0.14 and 0.16).
Do not continue.

Run it once more with `RUST_LOG=info,civ=trace` in front of the command, and check
that the radio answered `FB` (OK) to both `17 FF` and `1C 00 00`. The node sends the
two together, so the trace is what shows whether each one works on its own: an `FA`
(NG) to either means only the other stopped the keyer. Write down which.

**Ctrl-C.** Still at 6 wpm, set `max_key_seconds = 45`, run the same `cw` command
and press Ctrl-C about 3 seconds into the transmission.

**Pass:** the log shows `stop requested: stopping the keyer and forcing receive`,
the keying stops within about a second, the radio is back on receive after its
break-in delay, and the command exits with `radio confirmed on receive; exiting`;
`status` shows `transmitting: false`. This is the stop that Ctrl-C and the start-up
scripts rely on, on every system; on a Mac or Windows PC it is its first real test.

Restore `key_speed_wpm = 18`.

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

**Read this first.** A timer that cuts the radio's DC supply will leave
`tx-inhibited` in the state directory when it acts during an `hfnode` command:
the node cannot confirm receive from a radio with no power, so it latches the
inhibit (and `hfnode run` emails the alert). That is expected here. Check the
radio, run `hfnode radio --config $C status`, and only then remove the file before
the next step. **Take the SD card out of the radio** before any test that cuts its
DC: the manual warns that the card's data may be corrupted or deleted if the power
fails or the power cable is disconnected while the card is being accessed
(p. 8-2, lines 4862-4869).

The node does not use a PTT or keying line: it keys CW with CI-V
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
hardware timer. The node keys at most 30 characters per keyer command. Ordinary
text takes about 20 s at 18 wpm, but 30 zeros, the slowest 30 characters, take
44 s, which only just fits under 45 s; at 6 wpm they would take 131 s. A piece
that outlasts `max_key_seconds` is cut off by the watchdog, which counts as a
fault: it never makes a transmission longer.

**Then**, with steps 9 and 10 passed, set `commissioned = "done"`. This allows
`hfnode run`, and with it the service.

## Step 11: full exchange into dummy loads (optional, recommended)

A complete transaction with no signal on the air: the field rig transmits into its
own dummy load a few metres from the node, at its lowest power, so the node hears it
by leakage. Use the bench key.

In `~/bench.toml` leave `schedule.always = true` (the default) and set
`schedule.check_minutes = 1` for this step, configure one `[[contacts]]` entry
with your own email address, and the `[email]` settings, with `alert_to` set to an
address of yours. Print a few codes:

```sh
hfnode codes --config $C --count 10
```

`run` also needs a `[storm]` section with the station's latitude and longitude (see
`hfnode.example.toml`). Check it reads the forecast (it prints `clear:` or
`storm:`; an error means the node would never transmit):

```sh
hfnode storm --config $C
```

This is the first `run` (it needs stage `done`, and runs the preflight first,
including the Time-Out Timer check). Run the node in the foreground, with the
secrets loaded (`/etc/hfnode/env` is readable by root only):

```sh
set -a; . <(sudo cat /etc/hfnode/env); set +a     # or export HFNODE_EMAIL_PASSWORD=... by hand
```

First check the inhibit alert, with nothing transmitted: make the node start as if
an earlier fault had stopped it. `S` is the `state_dir` in `~/bench.toml`.

```sh
S=/var/lib/hfnode     # on a Mac: S=~/"Library/Application Support/hfnode/state"
echo "$(date +%s) bench alert test" > "$S/tx-inhibited"
hfnode run --config $C
```

On Windows, in PowerShell, write the file this way instead: a plain `>` in Windows
PowerShell writes UTF-16, which the node cannot read (the email would say
`Reason: not recorded`).

```powershell
$S = "$HOME\AppData\Local\hfnode\state"
"$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds()) bench alert test" |
    Set-Content -Encoding ascii "$S\tx-inhibited"
hfnode run --config $C
```

Expect `if transmitting is inhibited, <your address> is emailed`, then `emailed the
transmit-inhibit alert`, and an email `N0CALL: node restarted, still not
transmitting (tx-inhibited)` with the reason `bench alert test` and the steps to
clear it. Nothing tunes or keys. Once that line is logged, Ctrl-C, and:

```sh
hfnode radio --config $C rx
rm "$S/tx-inhibited"
hfnode run --config $C
```

On a Mac load your own `env` file as in [macos-setup.md, section
6](macos-setup.md#6-secrets); on Windows set each one with
`$env:HFNODE_EMAIL_PASSWORD = "..."`.

From the field rig, send the open and commit for a `TX` to your own contact, using
the exact formats in [operating.md](operating.md).

**Pass:** the alert email arrived with nothing keyed. Then the node tunes and keys
`DE N0CALL` into the dummy load as it starts (`health: tune` and `health: swr`
logged), logs `heard: ...`, keys the read-back, keys `SENT n` after the commit, and
the email arrives. `<state_dir>/last_seq` holds the commit's sequence number.

Then, with the node still running and idle, check the idle radio check without
transmitting: at the radio turn the dial off the node's frequency and select USB.
**Pass:** within about a minute (`check_minutes = 1`) the display is back on the
node's frequency in CW, and nothing transmitted. Then switch SPLIT on and send an
open: **Pass:** nothing is keyed and the node logs `radio not ready to transmit`
(or, if a re-tune was due, `could not set the radio up` and no tune). Switch SPLIT
off, repeat the same open, and expect the read-back (after a tune in the second
case). Set `check_minutes` back to 10 afterwards.

## Step 12: on the air, low power, with a second station

Now with the real antenna. Power 10 W. Arrange a second station (ideally the field
operator, at some distance) and a time. Check the frequency is clear before each
transmission, and identify.

Know the antenna's SWR before keying anything into it, the `QRL?` included. If you
have an antenna analyser, measure the antenna at the node's frequency first
(radio off, analyser on the coax): 2:1 or less, or within the tuner's 3:1 range.
Then listen, tune, and send the `QRL?` straight after the tune: the node's own
transmissions all end `DE <node_call> K`, and in `run` it identifies its window
tune itself, but `radio tune` does not, so the `QRL? DE N0CALL` is that tune's
identification.

```sh
hfnode listen --config $C                    # first listen; Ctrl-C when sure it is clear
hfnode radio --config $C tune                # a few seconds of carrier; stop if it cannot match
hfnode radio --config $C cw "QRL? DE N0CALL" # then listen again; continue only if clear
hfnode radio --config $C cw "VVV DE N0CALL"
```

If `radio tune` reports `tuner could not match the load`, or the `cw` command stops
with `SWR x.x above limit`, stop: check the antenna and feedline before anything
else.

Then ask the second station to send a few lines and run `hfnode listen --config $C`.

**Look for:** a tune line and an SWR reading in `health.csv`; the second station's
signal report; the node's decode of the second station.

**Pass:** SWR after tuning 1.5 or less; the second station copies the node; the node
decodes the second station well enough to read callsigns and numbers.

**Fail:** high SWR (check the antenna and feedline before anything else); RF
getting into the computer or USB (serial errors, audio dropouts, resets) while
transmitting: add ferrite chokes on the USB cable and check grounding before
continuing.

## Step 13: end-to-end exchange on the air

On a Pi, first give the state directory back to the node: it was yours for the
bench (see [Before you start](#before-you-start)), with any `tx-inhibited` a test
left in it.

```sh
sudo chown -R hfnode:hfnode /var/lib/hfnode
```

Use the real configuration now (on a Pi `/etc/hfnode/hfnode.toml`; on a Mac or
Windows PC the one in the node's folder), the real key, and a freshly printed table
(on a Pi `sudo -u hfnode hfnode codes --config /etc/hfnode/hfnode.toml`, elsewhere
`hfnode codes --config` with that file). Keep `power_watts` low for the first
session. Run the node in the foreground the first time (on a Pi `sudo systemctl
stop hfnode` if it is running), or start it as in section 7 or 10 of the setup
guide and watch its log (on a Pi `journalctl -u hfnode -f`).

On a Pi, run it in the foreground as the service user:

```sh
sudo systemctl stop hfnode
sudo systemd-run --pty --quiet --uid=hfnode --gid=hfnode \
  -p SupplementaryGroups="dialout audio" -p EnvironmentFile=/etc/hfnode/env \
  /usr/local/bin/hfnode run --config /etc/hfnode/hfnode.toml
```

(As yourself it cannot read the key, config or state; under plain `sudo` it leaves
`rx.log` and `health.csv` owned by root, which the service then cannot write.)

The second station plays the field operator, using [operating.md](operating.md).
Work through:

1. `TX` to a contact you control. Expect the read-back, then `SENT n` after `OK`,
   and the email or text to arrive.
2. Reply to that message from the contact's address or phone. After `email.poll_secs`
   (and screening), `RX`: expect `R n 1 MSG ?`, then the message after `OK`.
3. `AGN <n> <code> K`, then `AGN <n+1> <code> A K`, each on its own line: expect the
   whole transmission, then chunk A. Then send the last one again exactly: expect
   chunk A again, without using a line. A bare `AGN K` gets silence.
4. `WX` with the field station's 6-character grid square, then with a preset: expect
   a read-back naming the grid square (and the preset number), then a forecast that
   starts with it. Then `WX` alone: expect the read-back to name the preset's grid
   square, the last place confirmed. Then `WX IO91` (southern England): expect
   `FAIL n WX NO COVERAGE`, which shows the node tells a place the NWS does not
   cover apart from an outage (that place is not remembered). The node keeps the
   last place for that callsign in `state_dir/wx_last.json`; to start the trip
   from `weather.default_grid` instead, stop the node and delete that file.
5. Open a transaction, then a bare `NO K`: expect silence, and the transaction
   still pending. Then `NO` on the next line: expect `R NO`, and nothing sent.
6. With listening windows (`schedule.always = false` for this item): open a `TX` in
   the last minute of a window and send `OK` after the window has ended. Expect
   `SENT n` and the email once. Repeat the `OK` within 10 minutes: expect `SENT n`
   again and no second email. The log shows `window over: still listening ...` and,
   about 10 minutes after the last `SENT`, `listening window closed`.
7. End one exchange with `KN` instead of `K`, keyed as two letters, and another with
   `KN` run together as one character: expect the same replies as with `K`.
8. Resend the open of an already-completed transaction: expect silence.
9. Send a code from the wrong line: expect silence.
10. When the node starts (and at the start of each window, with windows), expect
    the tuning carrier followed by `DE N0CALL` on the air (ask the second station).
11. Optional: reply with a word the filter should remove, and check that `RX` shows
    `REDACTED` in its place.
12. Leave the node idle for longer than `schedule.retune_minutes` (set it to 10 for
    this session), then open a transaction: expect a few seconds of tuner carrier
    just before the read-back (no `DE N0CALL` between them: the read-back
    identifies it), a new `tune` line in `health.csv`, and no tune before the
    `SENT` that follows. Set `retune_minutes` back to 60 afterwards.

**Look for:** in the log, `heard:`, `opened transaction`, `committed transaction`,
`sending:`, and `no reply:` with a reason for each silent case; `health: swr` right
after the `health: tune` at start-up or a window's start (the station ID's SWR
check); `window over: still
listening ...` past a window's end; `station ID inside a long transmission` during a
long readout. In `state_dir`: `last_seq` equal to the highest line used (an open,
`NO` or `AGN` uses its line too), `rx.log` with every decoded transmission,
`health.csv` with tune and SWR lines.

**Pass:** every item behaves as described, the radio is on receive between
exchanges, SWR readings stay steady.

**After passing:** raise `power_watts` in steps to the operating value (30-50 W),
repeating step 12's SWR check at each power. Then have it start by itself (on a Pi
`sudo systemctl enable --now hfnode`; on a Mac or Windows PC section 7 of the setup
guide) and keep an eye on `health.csv` for the first
weeks: a slow rise in SWR readings means a connector or the antenna needs attention.

## Results

| Step | Date | Result | Readings / notes |
|---|---|---|---|
| -1 Self-test (mock radio) | | | version: / scale: / passed: |
| H1 DTR/RTS at port open (optional) | | | breakout: / driver: / pulse on DTR: / on RTS: / idle after: |
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
| 11 Dummy-load exchange | | | alert email: / ID after tune: |
| 12 On air | | | SWR: / report: |
| 13 End to end | | | ID after tune: |
