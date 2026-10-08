# A handheld for testing on 2 m

> **Parked.** This route (custom firmware on the radio, commands over its USB-C
> port) is parked: nothing here is to be flashed onto a radio. A handheld is now
> driven with its stock firmware through its headset jack, by the keyer box: see
> [A handheld through its headset jack](keyer.md#a-handheld-through-its-headset-jack)
> in keyer.md, which also has the jack's wiring from evidence and the checks to make
> on your own cable. The code below stays, tested against simulations only.

`hfnode` can drive a Quansheng handheld (a UV-K1 or UV-K5 v3; not the older UV-K5,
which has a different processor, from memory) instead of the IC-7300, so that the
whole system can be tried locally on 2 m before going on HF: the code sheet, the
protocol, the decoder, the gateways and the filter, all with two handhelds across
the room.

The node's handheld runs the NR7Y CW firmware with commands added for `hfnode`
([firmware/uv-k1](../firmware/uv-k1/README.md)): the node sends it text over the
radio's USB-C port and its keyer sends it, the way the IC-7300's keyer takes CI-V
commands. The command set is in [handheld-protocol.md](handheld-protocol.md).
Everything in `hfnode` has been tested against a simulated firmware, and the
firmware's own logic against a simulated radio; nothing here has keyed a real
radio.

## What is the same and what is not

The same as on the IC-7300: the code sheet and protocol, the decoder, the gateways,
the filter, the storm stand-down, the station ID, the software watchdog, keyer
pieces of at most 30 characters with receive confirmed after each, the transmit
inhibit, and the radio checked before every transmission.

Different:

- **You set the radio up, the node checks it.** The firmware sets nothing: the
  frequency, CW, the power and break-in are set at the radio ("Setting the radio
  up" below). The node reads them at start-up, before every transmission and every
  `schedule.check_minutes` (10 by default), and transmits nothing while any of them
  is wrong, saying what to change.
- **No tuner, no SWR or power meter.** A window start checks the radio without
  transmitting. Instead of SWR, the transmitter's state is read back from the radio.
- **Power** is `[handheld] power`, the level the radio must be set to: `low` (any of
  its LOW1 to LOW5), `mid` or `high`; its `USER` level is refused, and
  `station.power_watts` is not used. Use a low level across a room.
- **Duty cycle.** A handheld is not built for long transmissions: at most
  `max_duty_percent` of any `duty_window_secs` on the air (50 % of 5 minutes by
  default), and a long reply waits on receive between pieces. The count is kept
  while one command runs; each `hfnode` command starts it afresh. The firmware keeps
  its own budget besides (about half the time, after 165 s at once), so the node
  refuses settings over 50 %, or over 150 s at once.
- **A shared channel.** The node keys only once the squelch has been closed for
  `busy_quiet_ms`, and gives up on the transmission after `busy_max_wait_secs`.
- **Frequency**: 144-148, 222-225 or 420-450 MHz, simplex; a repeater offset or split
  left on the radio stops it transmitting. (Whether the radio itself transmits on
  222-225 MHz has not been checked.) `hfnode` warns outside the CW and weak-signal
  ends of the bands (144.000-144.275 MHz on 2 m), since CW among FM channels
  surprises their users.

`hfnode radio ...` is for the IC-7300 and refuses a handheld config; use
`hfnode handheld ...`.

## What you need

- **The node's handheld**, flashed with the firmware in
  [firmware/uv-k1](../firmware/uv-k1/README.md) (how to build and flash it is there),
  and **the field handheld**, which must send CW and receive it in a CW or USB mode.
  Keep both antennas on before anything keys.
- **A USB-C cable** from the radio to the computer, for the commands.
- **An audio cable carrying only the radio's receive audio**, from its speaker output
  to a sound card input on the computer, with nothing on the microphone and PTT. Not
  a cable that wires the PTT, such as the AIOC: in CW the radio keys for as long as
  its PTT is held, and its own time-out timer does not work in CW, so a PTT line
  stuck on would key it with none of the firmware's limits to stop it. Which contact
  carries what is not taken from memory: the evidence and the meter checks are in
  [The cable](keyer.md#the-cable) in keyer.md. Set the level with the radio's volume
  knob, checking with `hfnode record`.
- **A config file of its own for the handheld**, so that tests never touch the HF
  node's transmit inhibit, health log or codes. Copy `hfnode.example.toml` to, say,
  `~/handheld.toml`, and in it:
  - set `state_dir` to a folder of its own, for example `"~/handheld-state"`;
  - in `[station]`, uncomment `rig = "handheld"`, and set `frequency_hz` (for example
    `144_060_000`), `serial_port` to the radio's USB-C port (the port that appears
    when you plug the radio in: on a Mac a `/dev/cu.usbmodem...`, on Linux a
    `/dev/ttyACM...`, on Windows a `COM` port; on Linux the steadier name under
    `/dev/serial/by-id/` survives the radio restarting), and `max_key_seconds` to 60
    or less (more is refused: the firmware ends any run at a minute);
  - set `[audio] device` to the sound input of the audio cable (`hfnode devices`
    lists them; the default is the IC-7300's) and `pitch_hz` to the radio's `CWfreq`
    (600 Hz unless changed);
  - uncomment `[handheld]` and its lines at the end;
  - for `hfnode run`, give it a key of its own (`hfnode keygen --out
    ~/handheld.key`, then `[auth] key_file`) and print its own code sheet, so that
    codes sent on 2 m mean nothing on HF; and the rest of the setup `run` needs
    (docs/macos-setup.md: `[storm]`, email, the filter).

## Setting the radio up

At the radio, in the firmware's menu (the names as read from its source):

- **`Mode`: CW**, on `station.frequency_hz`, **simplex**: **`TxODir`** off (no
  offset), and **`RxMode`: `MAIN ONLY`** (dual watch off, so that receive and
  transmit are the same VFO).
- **`Power`** at the level `[handheld] power` names.
- **`CWbkin` (break-in) on.** Without it the keyer only sounds the sidetone, and the
  node refuses to key.
- **`CWkin` (the key input): `PTT HandKey` or `Side Btn Iambic`** (or its Reversed).
  Not a `Port` mode, which reads a key on the headset jack where the audio cable is
  plugged in, and not a `USB Port` mode, which takes the USB-C port for a key and
  cuts off the node's commands.
- **`BatSav` (battery saver) off** (inferred: it may turn the receiver off between
  checks, and the decoder would miss the start of a call), and probably **`SetOff`**
  (sleep after idle) off too (inferred).
- **`Sql` (squelch)** closed on an empty channel, so that the busy check works.

Then `hfnode handheld --config "$C" check` (below) reads the frequency (receive and
transmit), the mode, the power and break-in, and says what to change. It cannot read
the key input, the battery saver or the squelch: check those yourself. It also prints
how long the frequency has been quiet; if that stays at 0, the squelch is open and
the node would never key.

Stop `hfnode` before using the radio by hand, and don't run CHIRP, UVTools2 or any
other program on its USB-C port while `hfnode` is running.

## Bring-up

Like the IC-7300's, in stages. The stage passed is `commissioned` in the
`[handheld]` section of the handheld's config file (uncomment that line);
`commissioned` under `[station]` is the IC-7300's and does nothing here. Commands
that need a later stage are refused. Each command opens the handheld by asking the
firmware for its `HELLO`, stopping anything it is sending and confirming receive.

Every command needs the config file: `--config` goes straight after
`hfnode handheld` (before `check`, `key` and so on), or after `listen` or `run`. With
`C=~/handheld.toml`:

1. **`hfnode handheld --config "$C" check`** (stage `none`). The firmware answers,
   with its transmit limit and link timeout, reads receive, and the radio's settings
   match the config. Never transmits.
2. **`hfnode listen --config "$C"`**. Send CW by hand from the field handheld; the
   node prints what it decodes. When it reads you correctly, set
   `commissioned = "listen"`. If it decodes nothing, record a few seconds with
   `hfnode record` and listen to it; two handhelds may be a few hundred Hz apart, so
   widen `[audio] bandwidth_hz` if the tone is off `pitch_hz` (inferred).
3. **`hfnode handheld --config "$C" key "VVV DE W1ABC"`**, with your own call in place
   of W1ABC (`key` sends only the text you give it), and listen on the field
   handheld. The command reports the handheld back on receive after each piece. Then
   **`hfnode handheld --config "$C" linktest`**: it keys a long message and goes
   silent, as if the node had died, and passes only if the firmware stops on its own
   within its link timeout, not its transmit limit (it refuses to run if the two are
   too close to tell apart); it then sends `DE <call>`. When both are right, set
   `commissioned = "keying"`.
4. **`hfnode handheld --config "$C" hangtest`**, with your hand on the radio's power
   switch. It keys a long message and has the firmware hang, as a crash would: on the
   field handheld you hear a steady tone, since the hang lands just after the first
   key-down (if you heard no steady tone, run it again). Nothing in the firmware can
   stop the carrier then but its watchdog, which should reset the radio about 3 s
   later, and the reset should turn the transmitter off. The node waits, opens the
   radio again, and passes only if the firmware restarted 1 to 8 s after the hang
   (it reports how long it has been up), so a restart by hand or none at all fails.
   Either way it then sends `DE <call>`, as it does when the test fails after
   keying. If the carrier goes on past 10 s, switch the radio off: the watchdog does
   not work, and the radio must not be left to the node. When it passes and you
   heard the carrier stop, set `commissioned = "done"`. (If the computer gives the
   radio's port a new name after the reset, the test reports that the radio did not
   answer: check the name, or use a `/dev/serial/by-id/` name on Linux, and run it
   again.)
5. **`hfnode run --config "$C"`**.

`linktest` and `hangtest` key without the busy-channel wait and the duty cycle.

## How it is kept from sticking on transmit

Each keying run is ended by the first of:

1. the end of its text, read back from the radio (`STATUS`); a run read back as
   ended well before its text could have gone out fails the transmission;
2. the station's software watchdog (`max_key_seconds`), and its check that the radio
   is back on receive after each piece;
3. the node sending `STOP` once the run has gone 2 s past the end of its text (with
   50 ms a character for the radio switching back to transmit after a gap), or
   `max_key_seconds` plus 5 s, whichever is sooner, which also fails the
   transmission;
4. the firmware's link timeout (2 s): the node keeps the link alive only while a run
   should last, so a crash, a killed process or a pulled cable ends the run within
   that timeout;
5. the firmware's own transmit limit (a minute);
6. the firmware's check of every stop: a transmitter on 0.5 s to 1 s after a stop
   (still on, or keyed again by a held or stuck paddle) gets the radio reset by its
   watchdog, about 2 s later, however often the node sends `STOP` meanwhile;
7. the watchdog itself, about 3 s after the firmware hangs during a run (step 4 of
   the bring-up tests it).

Across runs, the firmware's key-down budget keeps a computer that goes on sending
to about half the time on the air, and a run in which it never read the radio chip
transmitting stops it taking any more until the radio is restarted, since its stop
checks could not see a stuck transmitter.

The radio's own transmit time-out timer does not work in CW, so it is not counted.

If the radio still reads transmitting after the node has tried to stop it, the
transmit inhibit latches as on the IC-7300, and nothing more is sent. The node
tries `STOP` again once `max_key_seconds` have passed since the run began, and at
each periodic check; meanwhile the firmware's own stop check should have reset the
radio. To clear the inhibit: stop `hfnode`, check that the radio is on receive, read
the log for why it latched, and delete `tx-inhibited` in the handheld's `state_dir`.

## Not yet checked on a radio

- The firmware is built in CI on every push (firmware/uv-k1/README.md, "Building")
  but has never been flashed.
- What the firmware was read to do and the host test assumes: that its time-out
  timer is cleared on every CW key-down, that its end-of-transmission routine ends a
  transmission, that the radio chip's transmit bit is set while the CW engine
  transmits and clears on receive (if it is never set, the first `key` is refused
  with `CHECK` after its text goes out), that a held paddle keys the radio again
  after a stop, and the watchdog's timing (its clock is not precise).
- That a reset turns the transmitter off soon enough: the radio chip, which also
  switches the power amplifier, keeps transmitting through the processor's reset
  until the start-up code resets it. The patched firmware does that as soon as the
  chip's pins are set up, before the display's start-up, but how long the
  bootloader takes first is not known. `hangtest` checks it.
- From memory: the UVTools2 steps, the UV-K5 models, and the band plan below. The
  jack's wiring and the AIOC's PTT are now from evidence, in
  [The cable](keyer.md#the-cable) in keyer.md.

## Rules

From memory, not checked against the current text of Part 97 (the eCFR, the
official online code of federal regulations):

- CW is allowed anywhere in the US amateur bands (47 CFR 97.305(a)); the ARRL band
  plan puts it at the bottom of 2 m (144.05-144.10 MHz for general CW).
- The station ID rule is the same as on HF (97.119(a)): your call at the end of each
  exchange and at least every 10 minutes. The node identifies as on the IC-7300
  ([operating.md](operating.md)): every reply ends with `DE <call> K`, and a long one
  has `DE <call>` inside it, with any wait for the duty cycle counted. Nothing is
  tuned at start-up, so no `DE <call>` is sent then.
- With you at the computer and the radio, the node is under local control. Leaving it
  to answer while you are away is automatic control, which Part 97 allows only for
  some kinds of station (97.109(d)); that question is open for the HF node too. Until
  it is settled, run it on a handheld only while you are with it.
