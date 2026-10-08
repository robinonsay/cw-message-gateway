# CI-V audit: the node against the IC-7300 manual

An independent review, before the node is first connected to a real IC-7300, of
every byte the driver puts on the CI-V link and every path that can make the radio
transmit. The reference is ICOM's *IC-7300 Full Manual* (`IC-7300_ENG_FM_12b`);
line numbers are those of the text copy in the project files,
`reference/IC-7300_ENG_FM_12b.txt`. Done 2026-10-04 on branch
`claude/civ-protocol-audit-nex3k4` (pull request #1), against base commit
`852e9fc`.

Six reviews looked at the code from different sides: the CI-V wire protocol, the
serial link and reply handling, transmit safety, radio settings the code does not
control, what the mock radio can and cannot prove, and the bench plan. Their
findings were checked against the code and the manual, and the fixes are in this
branch.

## Verdict

- **The command bytes are right.** Every command, sub-command, data format and
  range the driver sends matches Section 19 of the manual, including the
  command-17 character set and its 30-character limit. No byte-level
  contradiction was found.
- **One critical problem, now fixed.** Linux and macOS raise the serial port's DTR
  and RTS lines when it is opened, and the driver left them up. The IC-7300 can be
  set (USB SEND, USB Keying) to transmit on either line. With such a setting, left
  over from WSJT-X or a logging program, the radio would have gone on transmit a
  few seconds after any `hfnode` command opened the port, and nothing in the
  software would have noticed. The driver now lowers both lines at once, and
  nothing is written to the radio until it has read those settings back as OFF,
  except by `hfnode radio rx`, which only sends the stop and receive commands
  (`17 FF`, `1C 00 00`) so that it can take off transmit a radio the check would
  refuse.
- **The software now enforces the bring-up order.** Each stage of
  [hardware-test-plan.md](hardware-test-plan.md) unlocks the next command, power
  stays at 10 W or less until keying has been proven, and every command that
  writes, but for `radio rx`, first runs a read-only check of the radio.
- **What cannot be settled on paper** is listed under
  [Where the manual is silent](#where-the-manual-is-silent): each item is a
  measurement in the bench plan.

## Fixes

| Finding | Severity | Fix |
|---|---|---|
| Opening the port leaves DTR and RTS raised; USB SEND / USB Keying never checked | Critical | Both lines lowered straight after opening (port opened exclusively, DTR off on open); the preflight reads `1A 05 00 78`, `00 79`, `00 80` and refuses unless all are OFF |
| Nothing in the code enforced the bench plan; setup, tune, cw and run worked at up to 100 W on an untested radio | High | `station.commissioned` stage gates; at most 10 W before stage `keying`; read-only preflight before every write but `radio rx`'s `17 FF` and `1C 00 00`; `hfnode radio check` |
| Split, ∂TX or a memory channel could move the transmit frequency unseen | High | Preflight requires `0F` and `21 02` OFF; read-back requires `1C 03` (transmit frequency) to equal the set frequency |
| Settings trusted on a bare OK, never read back | High | Read-back after every setup: `03`, `1C 03`, `04`, `0F`, `21 02`, `16 47`, `14 0A`, `14 0C`, `14 0F` |
| The radio was set up once at start-up; each window's tune used whatever the front panel left | High | Every window re-reads `1C 00`, re-sends the settings and checks split, ∂TX and `1C 03` before tuning; if the radio is on transmit, a setting is refused or the transmit frequency is not the configured one, receive is forced and the window stays silent |
| A tuner that fails to match bypasses itself and reads `00`, which was logged as a good tune | High | `1C 01` parsed strictly; `00` after a tune locks the window out; any tune error forces receive |
| SWR checked once per window (once per boot with `schedule.always`) | High | SWR checked on every transmission |
| Every receive check assumes `1C 00` reads transmit while the keyer sends; the manual does not say so | High | Each SWR sample also reads `1C 00`; output on the Po meter while it reads receive stops all transmitting |
| The transmit inhibit lived in memory; a systemd restart cleared it | High | Written to `tx-inhibited` in the state directory; nothing transmits while it exists; the service gives up after 3 starts an hour |
| A tune that had not yet started could read as finished; 15 s timeout equal to the manual's maximum tune | Medium | Waits briefly for `1C 01` to read `02`; timeout 20 s (maximum 15 s, line 8119) |
| Frequency set before mode: SSB/CW Synchronous Tuning shifts the frequency by the pitch | Medium | Mode (`06`) before frequency (`05`) |
| A frame that lost its end swallowed the next reply | Low | The parser restarts at a preamble found inside a frame |
| Replies of any length accepted; `1C 00` anything but `00` read as transmit | Low | Every reply must have exactly the documented shape, or it is an error |
| `1C 00 01` (force transmit) could be sent; frequencies outside the radio's range could be sent | Low | The driver refuses both before anything goes out |
| CI-V address, baud rate and callsigns not validated | Low | Address 02h-DFh, baud one of the six USB rates, callsigns letters/digits/"/" only |
| CI-V USB port linked to REMOTE would let another controller's replies pass as the radio's | Low | The preflight refuses to go on if `1A 05 00 74` reads Link (it warned until the audit's K11) |
| Docs: "can toggle" understated DTR/RTS; Inhibit Timer and Time-Out Timer presented as protection they are not; USB Keying (RTTY) missing; hardware-timer advice could itself key the radio | Medium | `raspberry-pi-setup.md` section 6 and `hardware-test-plan.md` corrected |
| No trace of what went over the wire for bench evidence | Medium | `RUST_LOG=civ=trace` logs every frame in and out with its reply time |

## Byte-by-byte conformance

Frames are `FE FE 94 E0 <body> FD` to the radio and `FE FE E0 94 <body> FD` back
(line 8551). OK is `FB`, NG is `FA` (lines 8551-8552). A read is the command with no
data. "Bench" means the manual gives the format but not the behaviour, and the
named test-plan step measures it.

**Sent with data (writes and transmit control):**

| Body | Meaning | Manual | Matches |
|---|---|---|---|
| `05 00 00 03 07 00` | Set 7.030 MHz: 5 BCD bytes, lowest digits first, last byte `00` | 8581, 9422-9430 | Yes |
| `06 03 01` | CW, FIL1 | 8582, 9431-9444 | Yes |
| `14 0A 00 26` | RF power level 26 (10 W, linear on 0-255) | 8677 | Format yes; watts mapping bench (step 9) |
| `14 0C 00 73` | Key speed level 73 (18 wpm; 00 00 = 6, 02 55 = 48) | 8685 | Format yes; mapping bench (step 4) |
| `14 0F 01 85` | Break-in delay level 185 (10.0 dots; 00 00 = 2.0, 02 55 = 13.0) | 8698 | Format yes; mapping bench (step 4) |
| `16 47 01` | Semi break-in ON (never `02`, full) | 8767 | Yes |
| `17 <1-30 ASCII>` | Send CW from the radio's keyer; only the manual's characters | 8790, 9711-9735 | Yes |
| `17 FF` | Stop sending CW | 9733 | Yes; effect on a message in progress bench (step 7) |
| `1C 00 00` | Receive. `1C 00 01` is refused by the driver | 9319-9323 | Yes; effect on the keyer bench (step 7) |
| `1C 01 02` | One tuner cycle | 9327 | Yes |

**Reads (sent without data):**

| Body | Reply checked as | Manual | Matches |
|---|---|---|---|
| `03` | 5 BCD bytes, last `00` | 8579 | Yes |
| `04` | mode, filter; must be `03` (CW) after setup | 8580 | Yes |
| `0F` | `00` = split OFF, required | 8624 | Yes |
| `14 0A`, `14 0C`, `14 0F` | 2 BCD bytes, at most `02 55` | 8677, 8685, 8698 | Yes |
| `15 11` | Po: 0 = 0%, 143 = 50%, 213 = 100%, linear between | 8732 | Yes; calibration bench (step 9) |
| `15 12` | SWR: 0 = 1.0, 48 = 1.5, 80 = 2.0, 120 = 3.0 | 8736 | Yes |
| `16 46` | VOX: `00` OFF required | 8766 | Yes |
| `16 47` | `01` (semi) required after setup | 8767 | Yes |
| `16 66` | TX Inhibit: `00` OFF required by `tune`, `cw` and `run`; `01` warns for `check` and `setup` | 8788-8789 | Yes |
| `19 00` | `94`, or nothing is written | 8793 | Yes |
| `1C 00` | `00` receive, `01` transmit, anything else an error | 9319-9323 | Yes; keyer behaviour bench (step 6) |
| `1C 01` | `00` OFF (bypassed after a tune), `01` ON, `02` tuning | 9327, 5917 | Yes; timing bench (step 5) |
| `1C 03` | 5 BCD bytes; must equal the set frequency | 9332 | Yes |
| `21 02` | `00` = ∂TX OFF, required | 9348 | Yes |
| `1A 05 00 29` | Time-Out Timer (CI-V): `00` OFF to `05` 30 min; `tune`, `cw` and `run` refuse anything but `01` (3 min), `check` and `setup` warn | 8861 | Yes |
| `1A 05 00 35` | PTT tune set (PTT Start): `00` OFF required | 8872 | Yes |
| `1A 05 00 66`, `00 67` | MOD input during DATA OFF, DATA: `03` (USB) or `04` (MIC/USB) warns | 8953-8959 | Yes |
| `1A 05 00 71` | CI-V Transceive, reported | 8966 | Yes |
| `1A 05 00 73` | CI-V Output (for ANT): warns if ON | 8973 | Yes |
| `1A 05 00 74` | CI-V USB port: `01` Unlink required, `00` Link refused | 8975 | Yes |
| `1A 05 00 75` | USB Echo Back, raw value reported (see below) | 8978 | Yes |
| `1A 05 00 78`, `00 79`, `00 80` | USB SEND, USB Keying (CW), (RTTY): `00` OFF required | 8986, 8991, 8995 | Yes |
| `1A 05 00 84` | Meter peak hold: warns if ON | 9006 | Yes |
| `1A 05 01 61` | Keyer dot/dash ratio: refused unless `30` (1:1:3.0) | 9185 | Yes |
| `1A 05 01 97` | Inhibit Timer at USB Connection: warns if OFF | 9269 | Yes |
| `27 11` | Scope wave data output: refused if ON (the waveform stream keeps the link busy) | 9353-9361 | Yes; reply format bench (step 0) |

**The link:**

| Item | Driver | Manual | Matches |
|---|---|---|---|
| Addresses | Radio 94h (config 02h-DFh), controller E0h | 6823-6826, 8551 | Yes |
| Baud | 4800, 9600, 19200, 38400, 57600 or 115200 | 6869-6872 | Yes |
| Character format | 8 data bits, no parity, 1 stop bit, no flow control | Not stated | Bench (step 1) |
| Control lines | DTR and RTS lowered at open | 6895-6927 | Yes |
| Echoed frames | Skipped (they are addressed to 94h, from E0h) | 6879 | Yes |
| Transceive frames | Skipped (addressed to 00h) | 6827-6843 | Yes |
| Payload | BCD and ASCII never contain `FD` or `FE` | 8551 | Yes |

The tests `frames_on_the_wire` and `transmit_control_and_read_frames_on_the_wire`
in `crates/civ/src/ic7300.rs` pin these bytes, and `preflight.rs` has a test that
the preflight sends only reads.

## Where the manual is silent

The mock radio in `crates/civ/src/mock.rs` is written from the same manual as the
driver, so where the manual says nothing, both make the same assumption and a
passing self-test proves nothing. These are measured on the radio instead:

| Question | Why it matters | Step |
|---|---|---|
| Do the `1A 05` item numbers match this firmware? | A different numbering would read other items | 1 (change two items on the front panel, see `check` follow) |
| Which way round is Echo Back? The table says `00=ON`, the menu's default is OFF | Nothing depends on it; it settles the table | 1 |
| Are the key speed and break-in delay scales linear? | Keying times and the stuck-transmitter check | 4 |
| How long does `1C 01` read `02`, and what power does the tuner use? | Tune supervision | 5 |
| Does `1C 00` read `01` while the keyer sends `17` text? | Every receive confirmation; the node now stops if it does not | 6 |
| Do `17 FF` and `1C 00 00` each end a message the keyer is sending? | Every stop the node makes | 7 (with the frame trace) |
| Is the `14 0A` scale linear in watts? | Power never above the setting | 9 |
| Does the Time-Out Timer (CI-V) cover text keyed with `17`? | It is a backstop only | 10 (TRANSMIT check; the hardware timer stays required) |
| Do the CI-V meter reads return the held peak with Meter Peak Hold ON? | SWR samples | Not tested: set it OFF (default is ON) |

## Not changed, and why

- **The node does not change the radio's menu settings over CI-V.** It refuses to
  go on instead, so the radio stays in the state the operator set and photographed.
- **TX Inhibit (`16 66`) is only read** (the preflight refuses a command that can
  transmit while it is ON); the node does not set it. The manual does not say what
  it covers (the keyer, the tuner) or whether it survives a power cycle, and a latch
  inside the radio that the operator does not know about is its own hazard. Worth a
  bench look later.
- **Break-in is left ON between transmissions.** `17` only transmits with break-in
  on (p. 19-8, footnote 2). With nothing on the KEY jack and no `17` sent, break-in
  alone does not transmit.
- **No over-power trip.** The Po meter's calibration against watts is not known yet;
  step 9 measures it.
- **A tune that never finishes does not lock the window.** Receive is forced, and
  the SWR check on the first transmission catches a bad match.

## Open before unattended operation

None of these affects the bench steps; all need deciding before stage `done`:

- **Idle transmit monitor.** The node does not watch for the radio transmitting
  when it is not keying (someone at the KEY jack, VOX, another program). The
  hardware timer (step 10) is the backstop.
- **Duty cycle.** There is no limit on total key-down time per hour. A long RX
  reply (five messages) can be many minutes of keying.
- **Band edges.** The config accepts any frequency in the radio's transmit coverage,
  including exact band edges, regardless of licence class. The radio's own
  "ON (User) & TX Limit" band-edge setting (line 1531) could enforce the licensed
  segment.
- **Tuning carrier.** Listening all the time (the default since 2026-10-04), the
  node tunes at start-up and then only just before a reply once the last tune is
  older than `schedule.retune_minutes` (60): when it has just heard a call it will
  answer, so the frequency is in use, and normally followed at once by the reply,
  which identifies the station. With windows it tunes at the top of every window.
  A tune at start-up or at a window's top that matched is followed at once by
  `DE <node_call>` through the keyer (`17`, line 9711), so the carrier is
  identified. A tune that fails (no match, a lost reply, a timeout) is not, because
  the node then keys nothing; after a lost reply or a timeout it may still answer
  later. Either way the tune is a carrier of 2-3 s and at most 15 s (lines
  8118-8119), sent without first checking the frequency is clear. The bench's
  `hfnode radio tune` does not identify; the operator does (step 12).
- **`hfnode radio rx` failing in the service's stop hook** does not write the
  inhibit file.
- **Radio switched off or USB link lost.** Repeated CI-V timeouts are not treated
  as a lost radio. Off at a tune or an idle check (every `schedule.check_minutes`),
  the node inhibits itself; switched off and on between those, it sets the radio
  up again before its next transmission (every transmission does, since
  2026-10-04). A USB device that re-enumerates leaves the node holding a dead port. The
  stop procedure now says to stop `hfnode` before switching the radio back on; the
  node should latch the inhibit and exit after a few consecutive timeouts.
- **Clock.** Listening windows follow the Pi's clock. Listening all the time (the
  default) does not depend on it: the idle checks and re-tunes only count minutes,
  and a clock set back makes them due at once rather than late. `time-sync.target` is reached
  when timesyncd starts, not when it has synced, unless
  `systemd-time-wait-sync.service` is enabled (now a step in the Pi guide). Without
  network after a power cut the window-start tunes can still come at unscheduled
  times; the node does not check that the clock is synchronised itself.

## Addendum, 2026-10-04: macOS and Windows

Added when `hfnode` was made to run on macOS and Windows as well as Linux (branch
`claude/cross-platform-43shz2`). No CI-V byte, stage gate, preflight read or
read-back changed. What did:

- **Opening the port.** On Windows the port is opened without serialport's
  `exclusive()`, which exists only on Unix; Windows opens a COM port for one handle
  by itself (share mode 0, read in serialport 4.10.1's source). As before, the
  driver then lowers DTR and RTS and fails if it cannot, and nothing is written
  until the preflight has read USB SEND and both USB Keying items as OFF (but for
  `radio rx`, as above).
- **What each system does to DTR and RTS at open**, for the IC-7300's CP210x:

  | System | At open | Basis |
  |---|---|---|
  | Linux | Both raised (tty layer, `cp210x` `dtr_rts`); lowered on close (HUPCL) | Kernel source, read |
  | macOS | Both raised on the first open of a port, after waiting out a 2 s DTR-down delay (`IOSerialBSDClient::initSession`); lowered on close (HUPCL) | Apple's IOSerialFamily source, read. That Apple's and Silicon Labs' current CP210x drivers, which are DriverKit extensions, reach the port through this code is understood, not checked |
  | Windows | serialport applies its line settings with DTR and RTS control disabled before `open` returns; whether the driver raises them for an instant before that is not documented | serialport source, read; Microsoft's sample serial driver brings the lines up as its saved settings say. Silicon Labs' driver: inferred only |

  So on every system the lines may be up for a moment while the port opens. The
  radio's Inhibit Timer at USB Connection is meant for that moment but only delays
  a signal by a few seconds; with USB SEND and USB Keying OFF (which the preflight
  requires) the lines do nothing. None of this
  has been measured on an IC-7300. It can be measured without the radio, with a
  separate CP2102 breakout board and a meter or LED on its DTR and RTS pins.
- **Stop signals.** Ctrl-C, SIGTERM, SIGHUP and, on Windows, a console Ctrl-C or
  Ctrl-Break now reach a handler that, once a command has passed its preflight and
  may write to the radio, runs the same forced receive as the station layer
  (`17 FF`, `1C 00 00`, then `1C 00` until it reads receive) and exits while still
  holding the radio. Before, the process died on the signal and only systemd's stop
  hook forced receive. Step 7 of the bench plan now also checks this stop. Not
  covered: on Windows, closing the console window, logging off or shutting down
  ends the process as soon as the handler is told (the `ctrlc` crate's console
  handler returns at once, and Windows then terminates the process), and
  `Stop-Process` or ending the scheduled task kills it outright; the radio then
  finishes what is in its keyer by itself, and `stop-hfnode.ps1` (or `hfnode radio
  ... rx`) checks receive afterwards. A handler of our own that runs the forced
  receive within the few seconds Windows allows is a possible follow-up, to be
  checked on the radio before it is relied on.
- **Start-up outside systemd.** `deploy/hfnode-supervise.sh` (macOS) and
  `deploy/windows/hfnode-supervise.ps1` keep the unit's limits: restart 30 s after
  a failure, give up after 3 starts an hour, no restart after a clean stop, and
  `hfnode radio ... rx` after every stop or crash. As with the unit, that `rx`
  failing does not write the inhibit file (see the open items above).
- **The inhibit file and a missing state directory.** The bench commands `radio
  setup`, `tune` and `cw` did not create `state_dir`, so in a fresh bench setup an
  inhibit could not be written to `tx-inhibited` and held only for that one
  process. Those commands now create the directory before opening the port, and
  the inhibit creates it too if it is missing.

## Addendum, 2026-10-04: listening all the time

The node now listens all the time by default (`schedule.always = true`); listening
windows remain an option. Before this change `always = true` already existed, but
the window-start set-up and checks then ran only once, at start-up, and a lockout
after a high SWR, no output or a tuner that could not match lasted until a
restart. No CI-V command was added or changed: the new checks send the same
commands as a window start (`1C 00` read; `06`, `05`, `14 0A`, `14 0C`, `14 0F`,
`16 47`; then `0F`, `21 02` and `1C 03` reads). What changed:

- **Before every transmission** the node waits up to 2 s for `1C 00` to read
  receive, sends the settings again, and checks split and ∂TX are off and `1C 03`
  reads the configured frequency. If any of that fails nothing is keyed and receive
  is forced; the next transmission checks again. Before this, split or ∂TX switched
  on at the front panel after a window started would have been keyed through: the
  self-test scenarios `front-panel-split` and `front-panel-delta-tx` fail without
  the check.
- **While listening and hearing nothing**, every `schedule.check_minutes` (10,
  1-1440), the node does the same set-up and checks without tuning, so a dial or
  mode change at the front panel does not leave it deaf (`front-panel-idle`). If a
  check fails, receive is forced, and transmitting is inhibited if receive cannot
  be confirmed, as at a window start.
- **Tuning** happens at start-up, at each window start, and before a reply once
  the last tune is older than `schedule.retune_minutes` (60, 10-1440). A lockout
  lasts until that tune (`fault-high-swr-retune`, `retune`), which sets the radio up
  and checks it first, as a window start does. A tune that stopped before starting
  the tuner (inhibited, or the radio could not be set up) does not count: the node
  tries again before its next reply.
- **Decoder.** Listening for hours, the decoder learned a wrong speed from band
  noise, which garbled the next caller's first words; it now goes back to its
  starting speed after a quiet minute (`retune`, `fault-high-swr-retune` and
  `other-stations` fail without this). Not a CI-V change, noted here because it
  came with listening all the time.
- **Unchanged:** the SWR and `1C 00` cross-check on every transmission, the
  watchdog, the persisted inhibit, the bring-up stages and the 10 W bench cap.

## Addendum, 2026-10-07: fixes from the independent safety audit

The independent hardware-safety audit (2026-10-07, at commit `2e39996`) found
three things to fix before the IC-7300 is connected or keyed on the bench. No CI-V
command was added; `1C 01` and `1C 00` are read at new moments, and the mock radio
answers more reads. One driver change came with them: a reply is now looked for
once more after the 500 ms reply timeout before the command counts as unanswered,
so that a computer that held the node's thread up past the timeout does not turn a
reply that came in time into a fault (the SWR checks now make many more reads while
keying). What changed:

- **SWR on every piece (K1).** The SWR, Po and `1C 00` samples ran only for about
  the first second of the first piece of a transmission. They now run all through
  every piece, from just after the keyer starts until the radio is back on
  receive. The radio's own protection reacts to its power amplifier's temperature,
  not directly to SWR (p. 13-4, lines 7316-7324), so it is no prompt backstop.
- **Failed tunes (K2).** A tune with no reply, an unreadable tuner state or one
  still tuning after 20 s was followed by a forced receive and nothing else, and
  counted as a fresh tune for an hour. Now it locks the node out until it has tuned
  again (before its next reply), and if `1C 01` still reads `02` once receive is
  forced, transmitting is inhibited. The tune loop also reads `1C 00`, logged at
  trace level for bench step 5.
- **Tests for the connect-time protections (K3).** Dropping DTR and RTS at open,
  the stage check before the port opens, and the preflight before any write were
  right but untested. They now have tests that fail if any is taken out (named in
  [hardware-test-plan.md](hardware-test-plan.md#what-the-software-enforces-by-itself)),
  the keyer box's and the handheld's ports drop their lines the same way, the mock
  radio answers all 21 preflight reads, and `hfnode selftest` opens the mock as
  `hfnode run` does, preflight first.
