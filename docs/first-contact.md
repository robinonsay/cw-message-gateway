# First contact with the IC-7300

The order for connecting the node to the real radio for the first time, so that
nothing can damage it. It comes before, and gates, the bench steps in
[hardware-test-plan.md](hardware-test-plan.md); the mapping between the two is at
the end.

Every stage says what goes over the wire, whether it can transmit, how it passes,
and what to set `station.commissioned` to once it has. **The software refuses any
command whose stage has not been reached**, so a stage cannot be skipped by mistake:

| `station.commissioned` | Allows, in addition | Transmits? |
|---|---|---|
| `none` (default) | `radio check`, `radio status`, `radio rx`, `listen` | Never |
| `link` | `radio setup` (writes settings) | Never |
| `setup` | `radio tune` | A 2-3 s tuner carrier |
| `tune` | `radio cw` | CW, at most 10 W |
| `keying` | `power_watts` above 10 | CW at the configured power |
| `done` | `run` (the node, and the service) | Unattended |

Manual references are to ICOM's *IC-7300 Full Manual* (`IC-7300_ENG_FM_12b`), page
and line of the text copy in the project files (`reference/IC-7300_ENG_FM_12b.txt`).
The byte-by-byte check of every command against that manual is in
[civ-audit.md](civ-audit.md).

## What the software enforces by itself

These hold whatever the stage or config, and are covered by unit tests:

- **Serial control lines are dropped.** With USB SEND or USB Keying (CW) set to DTR
  or RTS, a raised line transmits or holds the key down (p. 12-11, lines
  6895-6927). Linux and macOS raise both lines when a port is opened; the driver
  lowers them straight after opening and will not use the port if it cannot.
- **Read-only preflight before any write.** `setup`, `tune`, `cw` and `run` first
  read, and refuse unless: the radio answers `19 00` as an IC-7300 (94h); it is on
  receive (`1C 00`); USB SEND, USB Keying (CW) and USB Keying (RTTY) are all OFF
  (`1A 05 00 78`, `00 79`, `00 80`); SPLIT is off (`0F`); ∂TX is off (`21 02`). `run`
  also requires the radio's Time-Out Timer (CI-V) to be set (`1A 05 00 29`).
- **Read-back after setup.** After setting the radio up, the node reads back
  frequency, mode (must be CW, not CW-R), break-in (must be semi), split, ∂TX, RF
  power (never above what was sent), key speed and break-in delay, and stops if any
  differs. An OK only means the radio accepted a command.
- **Only two frames can transmit:** `17` (CW text, at most 30 characters, which the
  radio's keyer sends and then stops) and `1C 01 02` (one tuner cycle). The driver
  refuses to send `1C 00 01` (force transmit), refuses frequencies outside the
  radio's 30 kHz-74.8 MHz, and treats any reply that is not exactly the documented
  shape as an error, never as a guess.
- **Config limits.** Baud must be one of the radio's CI-V USB rates and the CI-V
  address within 02h-DFh; frequency inside the transmit coverage table; power
  1-100 W, and at most 10 W until stage `keying`.

## Stage 0: before the USB cable goes in

No computer connected to the radio yet.

**Software.** On the computer that will drive the radio (the Pi or the Mac):

```sh
cargo test --all
hfnode selftest          # 0 failed
```

**Radio, front panel only.** Record the firmware version (MENU > SET > Others >
Information > Version, p. 15, line 7876). The CI-V `1A 05` item numbers the driver
uses are those of manual revision 12b; stage 1 confirms they match this firmware.

Set and photograph each of these screens:

| Item (MENU > SET > ...) | Set to | Why |
|---|---|---|
| Connectors > USB SEND | **OFF** | A DTR/RTS line would transmit (line 6895) |
| Connectors > USB Keying (CW) | **OFF** | A DTR/RTS line would key a carrier (line 6906) |
| Connectors > USB Keying (RTTY) | **OFF** | Same (line 6917) |
| Connectors > Inhibit Timer at USB Connection | **ON** (default) | Covers the moment the port opens (line 6928) |
| Connectors > CI-V > CI-V USB Port | **Unlink from [REMOTE]** (default) | USB on its own; baud and echo settings apply |
| Connectors > CI-V > CI-V Address | **94h** (default) | Must equal `station.civ_address` |
| Connectors > CI-V > CI-V USB Baud Rate | **115200** | Must equal `station.baud` |
| Connectors > CI-V > CI-V USB Echo Back | **OFF** (default) | Less traffic; the driver handles either |
| Connectors > CI-V > CI-V Transceive | **OFF** | No unsolicited frames; the driver handles either |
| Function > Time-Out Timer (CI-V) | **3 min** | The radio's own limit on CI-V transmissions (line 6281) |
| Others > Emergency > Tuner | **not ticked** (default) | Otherwise the tuner works into SWR above 3:1 (p. 13, line 6015) |

And on the main screen: **SPLIT off, XIT/∂TX off, RIT off, RF POWER at 0%** (so
that anything unexpected in stages 1 and 2 is at minimum power), CW mode.

**Hardware, from stage 1 onwards:** a 50 Ω dummy load rated well above 10 W on ANT;
nothing in the KEY jack; nothing on ACC; no external amplifier, antenna switch or
relay in the line (the manual warns that slower external equipment can reflect
power back into the IC-7300, TX Delay item, line 6274). Keep a hand near the POWER
switch during every stage that can transmit.

## Stage 1 `link`: read only

Connect the USB cable. Bench config as in hardware-test-plan.md, "Before you start",
with `power_watts = 10` and `commissioned = "none"`.

```sh
hfnode radio --config $C check
hfnode radio --config $C status
```

`check` only reads: `19 00`, `1C 00`, `1A 05 00 78`, `00 79`, `00 80`, `0F`, `21 02`,
`1A 05 00 29`, `1A 05 01 97`, `03`, `04`, `14 0A`, `16 47`, `1C 01`,
`1A 05 00 71`, `1A 05 00 75`, each sent without data, which reads the item.

**Pass:**

1. Every line is PASS or info, and `preflight passed` is printed.
2. Every value matches the radio's own screen: frequency to the hertz, mode, RF
   power about 0%, break-in, tuner, Time-Out Timer, the USB lines.
3. **The item numbers match this firmware.** On the front panel change Time-Out
   Timer (CI-V) from 3 to 5 min and CI-V USB Echo Back from OFF to ON; run `check`
   again and see both values change; put both back. Turn the VFO dial and see the
   frequency follow. (If a value does not follow, the radio's firmware numbers the
   `1A 05` items differently from the manual: stop and report it.)
4. Write down the raw Echo Back value with Echo Back OFF. The command table says
   `00=ON, 01=OFF` (line 8978); the factory default is OFF (line 6879). Either way
   the driver works; this settles which way round the table is.

Then set `commissioned = "link"`.

## Stage 2 `setup`: writes settings, never transmits

```sh
hfnode radio --config $C setup
```

Sends the preflight reads, then `1C 00 00` (receive), `05` (frequency),
`06 03 01` (CW, FIL1), `14 0A 00 26` (RF power level 26 for 10 W), `14 0C` (key
speed), `14 0F 01 85` (break-in delay 10.0 dots), `16 47 01` (semi break-in), then
reads every one of them back.

**Pass:** `configured and read back: ...`, the radio did not transmit, and the
display shows the frequency, CW, BK-IN, RF POWER about 10%, KEY SPEED 18 wpm and
BKIN DELAY 10.0d. Write down the displayed key speed and break-in delay: the
manual gives only the end points of those scales (lines 8683, 8696), so these two
readings confirm the linear mapping the driver uses.

Then set `commissioned = "setup"`.

## Stage 3 `tune`: the first transmission

A tuner cycle, 2-3 s of carrier into the dummy load (p. 11-2, line 5911).

```sh
hfnode radio --config $C tune
hfnode radio --config $C status
```

**Pass:** `tuned`, the tuner reads ON afterwards, `status` says
`transmitting: false`, a `tune` line in `health.csv`, and the Po meter during the
cycle never above about 10%. Note whether the radio tunes at the set power or its
own.

Then set `commissioned = "tune"`.

## Stage 4 `keying`: CW at 10 W into the dummy load

Run hardware-test-plan.md steps 6 (short CW and the SWR reading), 9 (watchdog:
proves that `17 FF` and `1C 00 00` really end a keyer message, which the manual does
not say) and 7 (high-SWR lockout into a deliberately mismatched load), in that
order, at 10 W.

**Pass:** all three pass. Then set `commissioned = "keying"`.

## Stage 5 `done`: power, timers and a full exchange, still into the dummy load

Run hardware-test-plan.md steps 8 (power calibration at 10, 25, 40, 50 W: never
above the configured power), 10 (hardware transmit timer) and 11 (full exchange
into dummy loads).

Also check the radio's own Time-Out Timer: in CW mode with nothing on the KEY jack,
push TRANSMIT (no key, so no RF) and confirm the radio returns to receive by itself
after 3 minutes. The manual says the timer applies to "transmitting initiated by a
CI-V command or pushing TRANSMIT" (line 6283); whether that includes text keyed with
`17` cannot be tested without a three-minute keyer message, which the node never
sends, so the hardware timer remains the backstop.

**Pass:** all pass. Then set `commissioned = "done"`, which allows `hfnode run`;
continue with hardware-test-plan.md steps 12 and 13 on the air.

## How to stop

Fastest first: the radio's POWER switch; `Ctrl-C` and then `hfnode radio rx` (the
port is held exclusively while a command runs, so `rx` from a second terminal will
not open it until the first has exited); for the service, `sudo systemctl stop
hfnode`, which runs `hfnode radio rx` afterwards.

## Mapping to hardware-test-plan.md

| This file | hardware-test-plan.md |
|---|---|
| Stage 0 | Step -1 (self-test), step 0 (desk check, done in civ-audit.md), "Before you start" |
| Stage 1 `link` | Step 1, plus `radio check` and the firmware mapping check |
| (no stage) | Steps 2 and 3 (receive audio): any time after stage 1; they do not use CI-V |
| Stage 2 `setup` | Step 4, plus the read-back |
| Stage 3 `tune` | Step 5 |
| Stage 4 `keying` | Steps 6, 9, 7 (in that order) |
| Stage 5 `done` | Steps 8, 10, 11 |
| After `done` | Steps 12, 13 |
