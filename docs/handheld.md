# A handheld for testing on 2 m

`hfnode` can drive a Quansheng handheld (a UV-K1 or UV-K5 v3) instead of the IC-7300,
so that the whole system can be tried locally on 2 m before going on HF: the code
sheet, the protocol, the decoder, the gateways and the filter, all with two
handhelds across the room. Set `station.rig = "handheld"` and add a `[handheld]`
section (the end of `hfnode.example.toml`).

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
  few minutes, and transmits nothing while any of them is wrong, saying what to
  change.
- **No tuner, no SWR or power meter.** A window start checks the radio without
  transmitting. Instead of SWR, the transmitter's state is read back from the radio.
- **Power** is `[handheld] power`, the level the radio must be set to: `low` (any of
  its LOW1 to LOW5), `mid` or `high`; `station.power_watts` is not used. Use a low
  level across a room.
- **Duty cycle.** A handheld is not built for long transmissions: at most
  `max_duty_percent` of any `duty_window_secs` on the air (50 % of 5 minutes by
  default), and a long reply waits on receive between pieces.
- **A shared channel.** The node keys only once the squelch has been closed for
  `busy_quiet_ms`, and gives up on the transmission after `busy_max_wait_secs`.
- **Frequency**: 144-148, 222-225 or 420-450 MHz, simplex; a repeater offset or split
  left on the radio stops it transmitting. `hfnode` warns outside the CW and
  weak-signal ends of the bands (144.000-144.275 MHz on 2 m), since CW among FM
  channels surprises their users.

`hfnode radio ...` is for the IC-7300 and refuses a handheld config; use
`hfnode handheld ...`.

## What you need

- **The node's handheld**, flashed with the firmware in
  [firmware/uv-k1](../firmware/uv-k1/README.md) (how to build and flash it is there).
- **A USB-C cable** from the radio to the computer, for the commands. Set
  `station.serial_port` to its port: `hfnode devices` lists them (on a Mac a
  `/dev/cu.usbmodem...`, on Linux a `/dev/ttyACM...`, on Windows a `COM` port).
- **An audio cable carrying only the radio's receive audio**, from its speaker output
  (the 3.5 mm plug of the two-pin jack) to a sound card input on the computer, with
  nothing on the 2.5 mm plug (microphone and PTT). Not a cable that wires the PTT,
  such as the AIOC: in CW the radio keys for as long as its PTT is held, and its own
  time-out timer does not work in CW, so a PTT line stuck on would key it with none
  of the firmware's limits to stop it. (The jack's wiring is from memory.)
- **The field handheld**, which must send CW and receive it in a CW or USB mode.
- `[audio] device` set to that sound input, and `[audio] pitch_hz` to the firmware's
  CW pitch.

## Setting the radio up

At the radio, in the firmware's menus (their names in the NR7Y docs):

- **CW mode**, receive and transmit, on `station.frequency_hz`, **simplex**: no
  offset, no split, and **dual watch off** (so that receive and transmit are the same
  VFO).
- **Power** at the level `[handheld] power` names.
- **Break-in on.** Without it the keyer only sounds the sidetone, and the node
  refuses to key.
- **The key input (CWkin) on the PTT or a side button, not one of the USB modes**,
  which take the USB port for a key and stop the node's commands.
- **Battery saver off** (inferred: it may turn the receiver off between checks, and
  the decoder would miss the start of a call).
- **The squelch** closed on an empty channel, so that the busy check works.

Then **`hfnode handheld check`** reads them all and says what to change.

Stop `hfnode` before using the radio by hand, and don't run CHIRP, UVTools2 or any
other program on its USB-C port while `hfnode` is running.

## Bring-up

Like the IC-7300's, in stages: `[handheld] commissioned` names the last one passed,
and commands that need a later one are refused. Each command opens the handheld by
asking the firmware for its `HELLO`, stopping anything it is sending and confirming
receive.

1. **`hfnode handheld check`** (stage `none`). The firmware answers, with its
   transmit limit and link timeout, reads receive, and the radio's settings match
   the config. Never transmits.
2. **`hfnode listen`**. Send CW by hand from the field handheld; the node prints what
   it decodes. When it reads you correctly, set `commissioned = "listen"`.
3. **`hfnode handheld key "VVV DE N0CALL"`** and listen on the field handheld. The
   command reports the handheld back on receive after each piece. Then
   **`hfnode handheld linktest`**: it keys a long message and goes silent, as if the
   node had died, and passes only if the firmware stops on its own within its link
   timeout, not its transmit limit (it refuses to run if the two are too close to
   tell apart); it then sends `DE <call>`. When both are right, set
   `commissioned = "keying"`.
4. **`hfnode handheld hangtest`**, with your hand on the radio's power switch. It
   keys a long message and has the firmware hang, as a crash would. Nothing in the
   firmware can stop the carrier then but its watchdog, which must reset the radio
   about 3 s later: listen on the field handheld for the carrier stopping. The node
   then opens the radio again, finds it on receive and sends `DE <call>`. If the
   carrier goes on past 10 s, switch the radio off: the watchdog does not work, and
   the radio must not be left to the node. When it passes and you heard the carrier
   stop, set `commissioned = "done"`.
5. **`hfnode run`**.

## How it is kept from sticking on transmit

Each keying run is ended by the first of:

1. the end of its text, read back from the radio (`STATUS`); a run read back as
   ended well before its text could have gone out fails the transmission;
2. the station's software watchdog (`max_key_seconds`), and its check that the radio
   is back on receive after each piece;
3. the node sending `STOP` once the run has gone 2 s past the end of its text, or
   `max_key_seconds` plus 5 s, whichever is sooner, which also fails the
   transmission;
4. the firmware's link timeout (2 s): the node keeps the link alive only while a run
   should last, so a crash, a killed process or a pulled cable ends the run within
   that timeout;
5. the firmware's own transmit limit (a minute);
6. the firmware's check of every stop: a transmitter still on 0.5 s after a stop
   gets the radio reset by its watchdog;
7. the watchdog itself, if the firmware hangs during a run (step 4 of the bring-up
   tests it).

The radio's own transmit time-out timer does not work in CW, so it is not counted.
If the radio still reads transmitting after the node has tried to stop it, the
transmit inhibit latches as on the IC-7300, and nothing more is sent until it is
cleared; the node goes on sending `STOP` until it reads receive.

## Not yet checked on a radio

- That the firmware built with the stop check runs at all: it was built once before
  that was added, and never flashed.
- What the firmware was read to do and the host test assumes: that its time-out
  timer is cleared on every CW key-down, that `CW_EndTxNow` ends a transmission, that
  the radio chip's transmit bit clears on receive, and the watchdog's timing (its
  clock is not precise).
- From memory: the UVTools2 steps, the jack's wiring, the AIOC's PTT, and the band
  plan below.

## Rules

From memory, not checked against the eCFR:

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
