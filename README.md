# ic-7300-hf-server

A home-station node that relays texts and email over QRP Morse code. A computer (a
Raspberry Pi, a Mac or a Windows PC), connected to an ICOM IC-7300 by one USB cable,
listens on a fixed HF frequency,
decodes CW from a field operator, authenticates them with one-time letter codes
from a printed table, and then:

- **TX**: sends a text or email to a named contact,
- **RX**: reads back replies that have arrived (after a compliance filter has screened them),
- **WX**: reads back a short National Weather Service forecast for the field
  operator's grid square or a numbered preset place (US only).

Every request is read back and does nothing until the field operator confirms it
with a second code. Message content travels in the clear. The codes only prove who
is sending.

> **Hardware status.** Every CI-V command in `crates/civ/src/ic7300.rs` cites
> ICOM's IC-7300 Full Manual (IC-7300_ENG_FM_12b), Section 19, and the bytes match it.
> The code has not yet run against a real radio, so follow the
> [hardware test plan](docs/hardware-test-plan.md) in order, starting with the
> receive-only bench steps. The RF power level to watts mapping is an assumption
> to confirm with the Po meter. Before the radio is connected, `hfnode selftest`
> runs the whole node against a byte-level mock IC-7300 (see
> [Self-test against a mock radio](#self-test-against-a-mock-radio)).

## How the code maps to the design

The design spec is Robin's design doc ("HF CW Message Gateway", kept in the
project files as `design/spec.md`, not in this repository). Where each part lives:

| Design item | Where |
|---|---|
| Precomputed HOTP-style codes, 8 letters A-Z | `crates/auth` (`CodeBook`). HMAC-SHA256 over the 8-byte sequence number, encoded as letters. |
| Accept only `seq > last_seq`, `last_seq` on disk | `crates/auth` (`Verifier`, `SeqStore`). `last_seq` is written before the node acts. |
| Two codes per transaction (open, then commit) | `crates/hfnode/src/session.rs` |
| Grammar `CALL seq code TX/RX/WX`, `OK seq code`, `NO seq code`, `AGN seq code [letter]`, over `K` or `KN` | `crates/protocol` (`parse`). Callsigns, `TX`/`RX`/`WX` and contact names are snapped to the nearest legal token. `OK`, `NO` and `AGN`, sequence numbers and codes must decode exactly. |
| Read-backs, `SENT`, chunk letters for `AGN` | `crates/protocol/src/reply.rs` |
| Stop-and-wait ARQ, silence as NACK, idempotent retries (a repeated `OK` recovers a lost result, also after the window has ended) | `crates/hfnode/src/session.rs`; listening on past a window's end in `crates/hfnode/src/node.rs` |
| CW decoder | `crates/cw` (decoder plus a synthesizer used for tests) |
| Inbound compliance filter (redact or drop, never paraphrase) | `crates/hfnode/src/gateway/filter.rs` (Claude API, or a local model through Ollama) |
| Email, text and iMessage connectors | `crates/hfnode/src/gateway/email.rs` (SMTP out, IMAP in), `google_voice.rs` (texts from a Google Voice number, through the same mailbox), `imessage.rs` (Messages on a Mac), `route.rs` (which one TX uses); see [docs/texting.md](docs/texting.md) |
| Weather (`WX`) | `crates/hfnode/src/gateway/weather.rs` (api.weather.gov) |
| Radio control over CI-V | `crates/civ` (`Rig` trait, framing, IC-7300 driver, `SimRig` and the byte-level `mock` for tests) |
| Station safety: reduced power, radio set up and checked before every transmission, tune at start-up (and before a reply once the last tune is old), SWR check, software PTT watchdog, chunked keying, health log | `crates/hfnode/src/station.rs` |
| Station ID (47 CFR 97.119(a)): `DE <call>` after the tune when the node starts listening (at start-up or a window's top) if it matched, and between chunks (or before the first, after a long over from the field) so that no more than 8 minutes pass from one ID of the node's to its next (the rule allows 10; an ID 10 minutes old no longer counts, and the time runs from the start of the next transmission); a tune before a reply is identified by the reply | `crates/hfnode/src/station.rs` (`open_window`, `ID_INTERVAL`) |
| Owner alert when the node stops transmitting (transmit inhibit) | `crates/hfnode/src/alert.rs` (email to `[email] alert_to`; the latch is in `station.rs`) |
| Storm stand-down: no tune or transmit while thunder is forecast or warned at the station, fails closed | `[storm]` in the config, `crates/hfnode/src/storm.rs` |
| Listening all the time (default) or in scheduled windows; radio set up again while idle | `[schedule]` in the config, `crates/hfnode/src/node.rs` |

The hardware PTT timer in the design is external hardware, not part of this
repository. See the [hardware test plan](docs/hardware-test-plan.md#step-10-hardware-ptt-timer)
for what it has to do.

## Workspace layout

```
crates/
  auth/       codes, verifier, last_seq store
  cw/         Morse table, decoder, synthesizer (examples/ has decoder experiments)
  protocol/   grammar, fuzzy snapping, replies, chunking, text sanitizing
  civ/        Rig trait, CI-V framing, IC-7300 driver, SimRig,
              mock: a byte-level IC-7300 that answers as the manual's Section 19 says
  hfnode/     config, inbox, session state machine, station safety layer,
              gateways (SMTP/IMAP, NWS, reply filter), node loop, CLI (src/main.rs)
              selftest: scripted field operator + scenarios against the mock radio
              tests/end_to_end.rs: synthesized CW audio in, keyer text out, no hardware
              tests/mock_radio_e2e.rs: every selftest scenario, as cargo tests
              tests/sweep_e2e.rs: a fast slice of the speed x SNR x keying sweep
hfnode.example.toml   annotated example configuration
deploy/               start-up: hfnode.service (systemd, Linux), hfnode-supervise.sh
                      and macos/ (Terminal or launchd), windows/ (Task Scheduler)
docs/                 setup, testing and operating guides
```

## Build and test

`hfnode` runs on Linux (the Raspberry Pi included), macOS (Apple Silicon and Intel)
and Windows. CI builds it, runs the tests and runs the self-test on all three (the
Intel Mac build is compile-checked only). It has not yet been run with the radio on
any of them.

You need a Rust toolchain (stable) and a C compiler (the TLS library, `ring`,
compiles some C): on Linux `build-essential`, on a Mac Apple's command line tools
(`xcode-select --install`), on Windows Visual Studio Build Tools with "Desktop
development with C++". Audio capture uses ALSA's `arecord` on Linux (package
`alsa-utils`), Core Audio on a Mac and WASAPI on Windows.

```sh
cargo test --workspace          # unit tests plus the end-to-end tests; no hardware needed
cargo build --release -p hfnode # binary at target/release/hfnode
```

The mock-radio scenarios (`tests/mock_radio_e2e.rs`) run 100 times faster than
real time and take about 40 s. On a slow or busy machine (a Pi, or a Mac doing
other work), run them slower: `HFNODE_E2E_SCALE=20 cargo test --test mock_radio_e2e`,
and `hfnode selftest --scale 20`.

**Building on the Pi.** This works on a Pi 4 or Pi 5 with 64-bit Raspberry Pi OS; the
first build takes a while. See [docs/raspberry-pi-setup.md](docs/raspberry-pi-setup.md).

**On a Mac or Windows PC**, `cargo install --locked --path crates/hfnode` builds it
and puts `hfnode` on your `PATH`. See [docs/macos-setup.md](docs/macos-setup.md) and
[docs/windows-setup.md](docs/windows-setup.md).

**Cross-compiling from an x86-64 Linux machine.** The simplest route is
[`cross`](https://github.com/cross-rs/cross), which runs the build in a container
with the right linker and C compiler:

```sh
cargo install cross
cross build --release -p hfnode --target aarch64-unknown-linux-gnu
# binary: target/aarch64-unknown-linux-gnu/release/hfnode
```

Without `cross`, on Debian or Ubuntu:

```sh
sudo apt install gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  cargo build --release -p hfnode --target aarch64-unknown-linux-gnu
```

A binary built this way links against the build machine's glibc. If the Pi
complains about a missing `GLIBC_` version, build with `cross` or on the Pi itself.

## Quick start (no radio needed)

Everything below runs on any machine (the commands are for a Unix shell; in
Windows PowerShell use a folder such as `$HOME\hf` instead of `/tmp/hf`). Start from
the example config, pointing the key and state at a scratch directory:

```sh
cargo build --release -p hfnode
alias hfnode=$PWD/target/release/hfnode
mkdir -p /tmp/hf && cp hfnode.example.toml /tmp/hf/hfnode.toml
# edit /tmp/hf/hfnode.toml: state_dir = "/tmp/hf/state", [auth] key_file = "/tmp/hf/node.key"

hfnode keygen --out /tmp/hf/node.key                   # refuses to overwrite an existing key
hfnode codes --config /tmp/hf/hfnode.toml --count 30   # the paper table
```

`hfnode codes` starts at one after the last sequence number used (or `--from N`)
and prints the codes in three columns, in groups of four letters (`VZLL AIIJ`).

**Try the protocol by typing.** `sim` runs the real session state machine on text
you type, as if it were decoded CW. With `--offline` nothing is emailed and no web
service is called:

```sh
hfnode sim --config /tmp/hf/hfnode.toml --offline
```

```
N0CALL/P 1 VZLLAIIJ TX MOM RUNNING LATE HOME SUN K
  NODE> R 1 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K
OK 2 YAAWUURC K
  [offline] would send to MOM: RUNNING LATE HOME SUN
  NODE> SENT 2 DE N0CALL K
```

Use the codes from your own table. Inside `sim`, `/code N` shows the code for line
N: `NO` and `AGN` take the next line and its code too (`NO 3 <code 3> K`).
`/msg NAME TEXT` adds an inbound message (so you can try `RX` and `AGN`), and
`/quit` exits. `sim` runs the session alone, with no radio and no schedule: it shows
the replies, but not the station ID the node keys after its tune and inside a long
readout, and it has no listening windows to end. Without `--offline`, `sim` really
sends email (and texts or iMessages, if those are set up) and calls the weather
service, so it needs the `[email]` settings and `HFNODE_EMAIL_PASSWORD`.

`sim` writes `last_seq` into `state_dir` like the real node, so codes you use in
`sim` are used up. Use a scratch `state_dir` and a scratch key, not the node's.

**Exercise the decoder.** `synth` writes a WAV file of CW, optionally with noise
and hand-keying jitter, and `decode` reads it back:

```sh
hfnode synth "N0CALL/P 1 VZLLAIIJ RX K" --out /tmp/hf/t.wav --wpm 18 --snr 6 --jitter 0.1
hfnode decode /tmp/hf/t.wav --pitch 600
```

`decode` also works on a recording made from the radio with `hfnode record`, which
is one of the bench steps.

## Self-test against a mock radio

`hfnode selftest` needs no config, radio, sound card or network. It runs the whole
node (decoder, parser, session, station safety layer and the real IC-7300 CI-V
driver) against `civ::mock`, a byte-level IC-7300, with a scripted field operator
on the other end:

```sh
hfnode selftest                          # all scenarios, PASS/FAIL table; exit code 1 on failure
hfnode selftest --list                   # names and what each covers
hfnode selftest --scenario rx-several -v # one scenario with every check and its transcript
hfnode selftest --scenario fault- --scale 20   # a group, at 20x real time (for a slow Pi)
hfnode selftest --sweep --csv sweep.csv  # speed x SNR x keying matrices, where it breaks
```

The operator keys CW audio (`cw::Keyer` plus noise) into the node's audio queue,
listens to what the mock radio actually keyed, and reacts: it waits out the
node's first tune and station ID, opens, checks the read-back, answers `OK`, `NO` or
`AGN` (each on its own line), and repeats an open or an `OK` that got no answer.
While the mock radio is on transmit the node hears nothing of the operator.
Scenarios cover the grammar (TX, RX up to the five-message cap and truncation, WX
with 4- and 6-character grids, a grid sent as two words, known and unknown presets,
WX alone reusing the last place, `FAIL` replies including `WX NO COVERAGE`, `NO` and
`AGN` on their own lines and their free repeats (a bare one is ignored), `AGN` after
a read-back, `AGN <n> <code> K K` for chunk K, `KN` keyed run together as the over,
a message whose last word is `K`, codes in two groups, an open on fresh lines
replacing a pending one, the 10-minute pending and `AGN` windows), lost read-backs
and results (a result also after the window has ended, and listening ending once
it can no longer be repeated), a readout longer than the 8-minute ID interval,
identified between two chunks, replayed and wrong codes, garbled callsigns, 10 to
30 wpm, SNR down to 0 dB, a sloppy hand key, sidetone, USB echo off, CI-V
Transceive frames from someone at the radio, a load the tuner matches and one
beyond its range (the window stays silent), listening windows (a high-SWR lockout
cleared by the next window's tune), listening all the time (a re-tune before a reply
once the last tune is old, a high-SWR lockout cleared by it, split or ∂TX switched
on at the radio, the dial and mode changed while the node is idle, band noise and
other stations calling, and a call after a long quiet spell answered the first
time), and radio faults (SWR rising after the tune,
fold-back, stuck transmit or key, also on the last over, a transmitter that will
not unkey, one that only the watchdog gets off transmit, refused status commands,
NG and lost or late CI-V replies, a readout the radio refuses, a tuner that never
finishes, a node started with transmitting already inhibited). Each one checks the
exact text keyed, what the gateway did (messages sent, inbox marked read only once
keyed), `last_seq`, that the node sent nothing the manual does not allow (any
unknown, malformed or disallowed CI-V frame, `17` while the keyer is busy, not on
the air or out of band, `1C 00 01`), the radio's settings as the node left them
(frequency, CW, power, keyer speed, break-in and its delay), that the node forced
receive after a fault and never otherwise, the station ID (`DE N0DE` after every
tune that matched, and no stretch of a transmission longer than 8 minutes without
one), that the owner is told once of a transmit inhibit (when it latches, or at
start-up) and never otherwise, and safety bounds: key-down and transmit lengths, no
transmit past the break-in delay plus the 3 s stuck margin, duty cycle, the radio on
receive when the node stops, and the tuner cycles expected.

The mock answers every command the driver uses with the bytes Section 19 of the
manual gives, keys `17` text at the set key speed (time-scaled), models semi
break-in switch-on and hang, the `1C 00` status, Po and SWR meters that read only
while the key is down, SWR and power fold-back, the tuner (2 to 3 s, matching loads
under 3:1 to below 1.5:1, p. 11-2), the transmitter's frequency coverage (p. 16-2),
CI-V Transceive frames to 00h for changes made at the radio (on by default,
p. 12-10) and USB echo (on by default in the mock; the radio's own default is off,
p. 12-11). It records what it keyed, with timestamps, and can drop, delay or NG a
reply, stick in transmit, or never finish tuning.

**What the mock cannot prove.** It is written from the same manual as the driver,
so it cannot catch a place where the real radio differs from the manual, or a
misreading shared by both (the [hardware test plan](docs/hardware-test-plan.md)'s
step 0 checks the bytes against the manual by hand, and steps 1 to 13 against the
radio). The manual gives only end points for key speed and break-in delay, so the
mock assumes they are linear in between. There is no RF: real SWR, power output,
the tuner's real timing, RF in the USB or audio, the serial link and the sound card
are untested, and so is the hardware PTT timer. The CW is synthetic and the noise
white, so real band conditions and real fists are tested only on the air. In a
time-scaled run the CI-V reply timeout, the watchdog tick and the forced-receive
retry pause stay in real time, so they take `scale` times longer in radio time;
`hfnode selftest --scale 1` runs everything at its real speed, real-time margins
included (about 23 minutes with one job per scenario, `--jobs 100`).

**Sweep: where it stops working.** The scenarios check one point each and all
pass, so they cannot show where the node breaks. `hfnode selftest --sweep` runs a
complete TX exchange (open, read-back, `OK`, `SENT`; with `--rx` also an RX
readout) over a grid of field operator speed (5, 8, 10, 13, 15, 18, 20, 25, 30,
35 wpm: the decoder's range), SNR in 2500 Hz (clean, 20, 10, 6, 3, 0, -3, -6 dB)
and keying (machine: 2% jitter; hand: 12% jitter, gaps stretched 1.4 times, 25 Hz
off pitch), 3 trials per cell with their own noise and jitter (`--trials`,
`--wpm`, `--snr`, `--keying` choose the grid; `--jobs`, `--scale` as above). The
operator repeats a transmission that gets no answer up to 3 times, and answers a
read-back that is not exactly the message with `NO` (on the line the `OK` would
have used) and starts over once on fresh lines; it never commits a wrong read-back.
A run succeeds only if the exact message reached the gateway, once, and `SENT` was
keyed. It prints, per keying, a matrix of successes/trials (with the extra
transmissions a success needed, `w` for a garbled read-back that still parsed, `W!`
for a wrong message delivered, `S!` for a safety violation), how much of what the
operator keyed the node decoded exactly, and the edges; `--csv PATH` writes one row
per run. Wrong messages delivered and safety violations (any failed safety, CI-V,
settings, forced-receive, self-decode or station ID check) are hard failures at any
SNR. It exits non-zero on a hard failure or on any failed trial in the should-pass
region: machine-keyed 10-30 wpm at 6 dB and above, hand-keyed 10-25 wpm at 10 dB
and above, set well inside the edges measured below.
The default grid is 480 runs, about 3.5 minutes on a 4-core laptop (the run is paced
by the time scale, not the CPU); `tests/sweep_e2e.rs` runs a few cells of it in a
few seconds.

**Seeds fix the audio, not the outcome.** Each trial's seed (`audio_seed` in the
CSV) fixes its noise and keying jitter. The node and the mock radio run on the wall
clock, though: the mock's radio time is real time times the scale, and the node's
transmit guard uses `Instant`. So where the node's transmissions fall against the
operator's audio, and which received audio the node drops while it transmits,
depend on thread scheduling, and a re-run with the same seed is not the same run.
In five sweeps of the default grid on one machine (4 at once), 479 of 480 trials
had the same outcome each time (machine-keyed 5 wpm at 10 dB went 2/3, 2/3, 1/3,
2/3, 2/3), but 8 trials needed a different number of transmissions and 119 logged
a different number of receptions. Re-running the edge cells with `--jobs 1` or
`--jobs 16` flipped 2 of 36 trials (hand-keyed 35 wpm at 0 dB, machine-keyed 5 wpm
at 10 dB), and on another machine machine-keyed 5 wpm at -6 dB went from 0/3 to
1/3. Read a single count at an edge as give or take one trial; the should-pass
region stays clear of the edges for this reason.

Measured on 2026-10-04 (default grid, 3 trials per cell, 100x real time, 4 at once;
the success counts below came out the same in five sweeps, except the one trial
named above). One more sweep the same day, after the station ID, `NO` and `AGN` on
lines and listening past the window were added, gave the same counts but for
machine-keyed 5 wpm at 20 dB, 1 of 3 instead of 2 of 3, an edge cell within its one
trial:

- **Machine-keyed:** 8 to 35 wpm pass every trial from clean down to -3 dB. At -6
  dB almost nothing gets through (1 success in 30 runs in each sweep, at 8 wpm). 5
  wpm passes clean and from 6 to -3 dB, but 1 of 3 trials fails at 20 dB and at 10
  dB.
- **Hand-keyed:** 8 to 30 wpm pass down to -3 dB (20 wpm down to 0 dB), 35 wpm down
  to 0 dB, 5 wpm down to 3 dB; -6 dB fails at every speed.
- **At 10 dB:** machine-keyed 8 to 35 wpm, hand-keyed 5 to 35 wpm pass every trial.
- **Repeats** start at 20 dB at most speeds (0.3 to 0.7 extra transmissions per
  exchange on average) and, hand-keyed, even on a clean signal at 5, 15, 18, 20 and
  35 wpm: with any noise only about half of the operator's transmissions decode
  exactly (85% word for word, with noise characters around them), at 20 dB as at
  -3 dB.
- **Integrity and safety:** 15 read-backs in 480 runs were garbled text that still
  parsed (for example `RUNNING LAEE HOME SUN`, hand-keyed and clean); the operator's
  read-back check caught every one. No wrong message was delivered and no safety
  bound was broken in any run.

**Test vectors.** `hfnode testvectors --out DIR` writes the field operator's side of
a session as WAV files at several speeds and noise levels, with `manifest.txt`
giving what each decodes to and what a node should answer (or, for a noisy file
whose decode the node would read differently, that it will not give that answer),
for playing into a bench radio or checking the decoder (`hfnode decode`). Their codes come from a
fixed, public test-only key (written alongside as `test-only.key`): never use it on
the air.

## Commands

| Command | Transmits? | What it does |
|---|---|---|
| `hfnode keygen --out FILE` | no | Create a 256-bit key (mode 0600). |
| `hfnode codes --config C [--from N] [--count N]` | no | Print the code table (default 100 lines). |
| `hfnode sim --config C [--offline]` | no | Type field transmissions, see the node's replies. |
| `hfnode synth TEXT --out F [--wpm] [--pitch] [--snr] [--jitter]` | no | Write CW to a WAV file. |
| `hfnode decode FILE [--pitch HZ]` | no | Decode CW from a WAV file. |
| `hfnode selftest [--scenario NAME] [--scale N] [--list] [-v]` | no | Run the scenarios against the mock IC-7300 (no hardware). |
| `hfnode selftest --sweep [--wpm ..] [--snr ..] [--keying ..] [--trials N] [--rx] [--csv F]` | no | Sweep speed x SNR x keying with complete exchanges; print where it breaks. |
| `hfnode testvectors --out DIR [--wpm 12,18,25] [--snr clean,10]` | no | Write test field transmissions as WAV files with a manifest. Test-only key. |
| `hfnode devices` | no | List serial ports and audio inputs, marking the radio's. Opens nothing. |
| `hfnode messages --config C check [--since H] [--save-raw DIR] [--dump ROWID]` | no | Show how TX reaches each contact and what the node would take from its mailbox and Messages. Changes nothing. See [docs/texting.md](docs/texting.md). |
| `hfnode messages --config C send [--via imessage\|google-voice\|email] NAME TEXT` | no | Really send one message to a contact by the route TX would use. |
| `hfnode filter --config C test` | no | Screen ten sample replies with the configured filter model and check the verdicts. With Claude, each is a paid API call. |
| `hfnode filter --config C screen TEXT [--from NAME]` | no | Show what one reply would be keyed as. |
| `hfnode listen --config C` | no | Decode live audio from the radio and print it. |
| `hfnode record --config C --out F [--seconds N]` | no | Record the radio's audio as the node hears it; print the peak level. |
| `hfnode radio --config C check` | no | Read-only preflight: identify the radio and read every setting that could make it transmit, one PASS, WARN or FAIL line each. Writes nothing. |
| `hfnode radio --config C status` | no | Read the frequency and TX/RX state. |
| `hfnode radio --config C rx` | no | Stop the keyer and force the radio to receive. |
| `hfnode radio --config C setup` | no | Set frequency, CW mode, power, keyer speed, semi break-in. |
| `hfnode radio --config C tune` | **yes** | Set up, then run the internal antenna tuner. Not identified: `run` identifies its tunes, here you do. |
| `hfnode radio --config C cw TEXT` | **yes** | Set up, then key TEXT and log the SWR reading. |
| `hfnode keyer --config C check` | no | Any radio through the keyer box ([docs/keyer.md](docs/keyer.md)): greet the box, show its limits and why it last started, and check the radio's audio (band level, no key held at the radio). Keys nothing. |
| `hfnode keyer --config C rx` | no | Stop the box and confirm the key is open, by the box and by the radio's audio. |
| `hfnode keyer --config C key TEXT` | **yes** | Key TEXT through the box with every check `run` makes but the storm stand-down; report whether the radio was heard sending it. |
| `hfnode keyer --config C sidetone` | **yes** | Key `DE <call>` and measure the radio's sidetone: its delay, level and pitch. |
| `hfnode keyer --config C hangtest` | **yes** | Hang the box's control loop mid-transmission: its watchdog must open the key within 0.5 s. Then identifies. |
| `hfnode keyer --config C stucktest` | **yes** | Identify, then make the box hold its key down: its 1 s limit must open the key and lock the box until it is unplugged. |
| `hfnode storm --config C` | no | Ask the NWS once whether the storm stand-down would hold now. The stand-down applies to `run` only; the bench commands below do not check it. |
| `hfnode run --config C` | **yes** | Run the node (and email `[email] alert_to` if transmitting is inhibited). |

Logging goes to stderr (the journal, under systemd) at level `info`; `RUST_LOG`
replaces that level. `hfnode` itself logs only one start-up line at debug level, so
`RUST_LOG=debug` mostly adds the libraries' messages (SMTP, IMAP, HTTP). To see
every CI-V frame and how long each reply took, use `RUST_LOG=info,civ=trace`:
`RUST_LOG=civ=trace` alone shows the frames but hides everything else the node
logs. For the service, `sudo systemctl edit hfnode` and add
`Environment=RUST_LOG=info,civ=trace` under `[Service]`; take it out again
afterwards, as it logs frames every tenth of a second while keying.

Ctrl-C (or Ctrl-Break on Windows), or a stop from systemd or launchd, makes a
command that has started writing to the radio stop its keyer and confirm receive
before it exits. On Windows, closing the window, logging off or a restart ends it
without that; check receive afterwards (`hfnode radio --config C rx`).

A running node owns the radio: stop it before using the radio yourself. Listening
all the time, it puts its frequency, mode, power and keyer settings back every
`schedule.check_minutes` (10) while idle, and before every transmission.

The node keeps its state in `state_dir`: `last_seq`, `inbox.json`, `wx_last.json`
(the last weather place each field callsign confirmed), `rx.log` (every decoded
transmission), `health.csv` (every tune and SWR reading, including the one from the
station ID after the tune when the node starts listening, and any `tx-status` or
`check` fault), if it has stopped transmitting, `tx-inhibited`, and the texting
files listed in [docs/texting.md](docs/texting.md#files-in-state_dir).

## Documentation

- [docs/raspberry-pi-setup.md](docs/raspberry-pi-setup.md): installing on the Pi (or other Linux), radio menu settings, secrets, systemd.
- [docs/macos-setup.md](docs/macos-setup.md): running the node on a Mac.
- [docs/windows-setup.md](docs/windows-setup.md): running the node on Windows.
- [docs/hardware-test-plan.md](docs/hardware-test-plan.md): staged bench plan, from checking CI-V bytes to the first on-air exchange.
- [docs/operating.md](docs/operating.md): the field operator's guide, with exchange formats.
- [docs/texting.md](docs/texting.md): reaching contacts by text (Google Voice) and iMessage, and checking it.
- [docs/reply-filter.md](docs/reply-filter.md): the reply filter: Claude or a local Ollama model, choosing and testing a model.
- [hfnode.example.toml](hfnode.example.toml): every config key, with comments.
