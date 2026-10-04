# A handheld for testing on 2 m

`hfnode` can drive a Quansheng handheld (a UV-K1 or UV-K5) instead of the IC-7300,
so that the whole system can be tried locally on 2 m before going on HF: the code
sheet, the protocol, the decoder, the gateways and the filter, all with two
handhelds across the room. Set `station.rig = "handheld"` and add a `[handheld]`
section (the end of `hfnode.example.toml`).

The node's handheld runs a CW firmware that keys a carrier on commands over its
serial link, the way the IC-7300's keyer takes CI-V commands. That command set is in
[handheld-protocol.md](handheld-protocol.md); any firmware that keeps it works,
including its safety rules (its own transmit limit and link timeout). Everything in
`hfnode` has been tested against a simulated firmware only; nothing here has keyed a
real radio.

## What is the same and what is not

The same as on the IC-7300: the code sheet and protocol, the decoder, the gateways,
the filter, the storm stand-down, the station ID, the software watchdog, keyer
pieces of at most 30 characters with receive confirmed after each, the transmit
inhibit, and the radio set up again and checked before every transmission.

Different:

- **No tuner, no SWR or power meter.** A window start sets the radio up without
  transmitting. Instead of SWR, the firmware's own transmit state is read back.
- **Power** is `[handheld] power`, the radio's own `low`, `mid` or `high` levels;
  `station.power_watts` is not used. Leave it at `low` across a room.
- **Duty cycle.** A handheld is not built for long transmissions: at most
  `max_duty_percent` of any `duty_window_secs` on the air (50 % of 5 minutes by
  default), and a long reply waits on receive between pieces.
- **A shared channel.** The node keys only once the squelch has been closed for
  `busy_quiet_ms`, and gives up on the transmission after `busy_max_wait_secs`.
- **Frequency**: 144-148, 222-225 or 420-450 MHz, simplex; the node sets both receive
  and transmit and reads them back, so a repeater offset left on the radio is caught.
  `hfnode` warns outside the CW and weak-signal ends of the bands (144.000-144.275
  MHz on 2 m), since CW among FM channels surprises their users.

`hfnode radio ...` is for the IC-7300 and refuses a handheld config; use
`hfnode handheld ...`.

## What you need

- **The node's handheld** with the CW firmware flashed, and its transmit time-out
  timer (in the radio's menu) at its shortest setting.
- **A cable to the computer** carrying the radio's serial link (for the commands)
  and its receive audio (for the decoder). On the two-pin Kenwood-style jack, the
  AIOC (All-In-One-Cable) carries both. The node holds DTR and RTS down, so the
  AIOC's own PTT is never keyed. (The AIOC's wiring is from memory, not checked.)
- **The field handheld**, which must send CW and receive it in a CW or USB mode.
- `[audio] device` set to the cable's sound input, and `[audio] pitch_hz` to the
  firmware's CW pitch. `hfnode devices` lists the serial ports and audio inputs.

## Bring-up

Like the IC-7300's, in stages: `[handheld] commissioned` names the last one passed,
and commands that need a later one are refused. Each command opens the handheld by
asking the firmware for its `HELLO`, stopping anything it is sending and confirming
receive.

1. **`hfnode handheld check`** (stage `none`). The firmware answers, with its
   transmit limit and link timeout, and reads receive. Never transmits.
2. **`hfnode handheld setup`**. Sets the frequency (simplex), CW and the power, and
   reads them back. Never transmits.
3. **`hfnode listen`**. Send CW by hand from the field handheld; the node prints what
   it decodes. When it reads you correctly, set `commissioned = "listen"`.
4. **`hfnode handheld key "VVV DE N0CALL"`** and listen on the field handheld. The
   command reports the handheld back on receive after each piece. Then
   **`hfnode handheld linktest`**: it keys a long message and goes silent, as if the
   node had died, and passes only if the firmware stops on its own within its link
   timeout, not its transmit limit (it refuses to run if the two are too close to
   tell apart); it then sends `DE <call>`. When both are right, set
   `commissioned = "keying"`.
5. **The radio's own timer.** Hold the node's handheld PTT by hand past its
   time-out timer, at low power and with your call, and check that it stops. Then
   set `commissioned = "done"`.
6. **`hfnode run`**.

## How it is kept from sticking on transmit

Each keying run is ended by the first of:

1. the end of its text, read back from the firmware (`STATUS`); a run read back as
   ended well before its text could have gone out fails the transmission;
2. the station's software watchdog (`max_key_seconds`), and its check that the radio
   is back on receive after each piece;
3. the node sending `STOP` once the run has gone 2 s past the end of its text, or
   `max_key_seconds` plus 5 s, whichever is sooner, which also fails the
   transmission;
4. the firmware's link timeout (1 to 3 s): the node keeps the link alive only while
   a run should last, so a crash, a killed process or a pulled cable ends the run
   within that timeout;
5. the firmware's own transmit limit (a minute at most), which also ends a run on a
   firmware that ignores `STOP`;
6. the radio's own time-out timer.

If the firmware still reads transmitting after the node has tried to stop it, the
transmit inhibit latches as on the IC-7300, and nothing more is sent until it is
cleared; the node goes on sending `STOP` until it reads receive.

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
