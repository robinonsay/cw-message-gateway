# Any radio: the keyer box

The node can run any CW radio, not only the IC-7300: an IC-7300 without CI-V, a
Yaesu, a Mountain Topper, a cheap QRP rig. It hears the radio through its headphone
jack and a sound card, and keys it through its key jack with a small USB box built
from a Raspberry Pi Pico 2, the **keyer box**. The node never controls the radio
itself: you set the frequency, mode and power at the radio.

```
radio PHONES ──audio──► sound card ──► hfnode: CW decoder + sidetone monitor
radio KEY    ◄─opto──── keyer box  ◄──USB── hfnode: keyer rig
```

The box keys the radio like a straight key, from text the node sends it, with its
own Morse timing and its own limits. The node listens to the radio's **sidetone** to
check that the radio really keyed what the box sent, and that its key is open again
afterwards.

The same box can also drive an FM handheld through its headset jack, holding its
PTT and keying a Morse tone into its microphone: see "A handheld through its
headset jack" below. A Quansheng UV-K1 with NR7Y's paddle rework, running NR7Y's
firmware in its CW mode, is keyed instead as a straight key through the same jack:
see "A UV-K1 on NR7Y in CW mode (paddle-reworked)" below.

The IC-7300 over CI-V ([hardware-test-plan.md](hardware-test-plan.md)) works as
before; `station.rig` picks one or the other.

## What you need

- A Raspberry Pi Pico 2 (RP2350), with headers or wires soldered on. Not the Pico
  2 W: its GP25 belongs to its wireless chip, not an LED.
- A PC817 optocoupler, a 470 Ω resistor and a 4.7 kΩ resistor. The 4.7 kΩ is
  **required**, not optional: see the wiring below. Other optocouplers have not
  been checked against this circuit; a 4N25 or 4N35 is a different part with
  different ratings, and its collector current at 4.5 mA of LED current may be too
  low to key a radio reliably. Use a PC817 unless you have read the datasheet of
  what you have.
- A 50 Ω dummy load that takes your radio's full power, for the bring-up tests.
- A plug for the radio's key jack: a 6.35 mm (1/4") stereo plug for the IC-7300
  (its KEY jack, manual text lines 519-521), a 3.5 mm stereo plug for most small rigs.
  Check your radio's manual.
- A USB sound card with a line or mic input (or a computer's own line input), and a
  cable from the radio's PHONES (or EXT SP) jack to it. Headphone level can be far
  more than a mic input wants: start with the radio's volume low, or use a line
  input.
- A USB cable for the box.

## Wiring the box

```
Pico 2                         PC817                     key plug
GP16 (pin 21) ──470 Ω──► 1 anode     collector 4 ──────── tip
GND  (pin 23) ─────────► 2 cathode   emitter   3 ──────── sleeve
GP16 ──4.7 kΩ── GND                                       (ring: not connected)
```

- GP16 high lights the optocoupler's LED, which closes the key: about 4.5 mA, well
  inside the PC817's ratings.
- **The 4.7 kΩ resistor is required.** It holds GP16 low whenever nothing drives it
  (while the chip starts, resets or is unplugged), and the RP2350 needs it for that:
  erratum RP2350-E9 says the chip's own internal pull-down can leave a pad that is
  not being driven sitting at about 2 V, which is enough to light the optocoupler
  and key the radio. An external pull-down of 8.2 kΩ or less is Raspberry Pi's own
  fix; 4.7 kΩ is that with margin. Do not leave it out, and do not rely on the
  chip's internal pull-down.
- The optocoupler keeps the computer's ground off the radio's **key line**.
  Polarity matters on its output: collector to the tip (the radio's key line, which
  the radio pulls up), emitter to the sleeve (the radio's ground). It does not
  isolate the station: the audio cable from the radio's headphone jack ties the
  computer's ground to the radio's anyway. Treat the computer and the radio as
  sharing a ground, and keep the key lead short.
- **Use a stereo plug, wired tip and sleeve, with the ring left open.** A mono plug
  shorts the ring to the sleeve inside the jack. On a radio set to Paddle or Bug,
  the ring is the dash paddle: a shorted ring is a dash paddle held down, and the
  radio keys continuously by itself, with nothing the box or the node can do about
  it. Check the radio is set to Straight (below) before plugging anything in.
- A clip-on ferrite on the key lead and another on the audio cable, at the radio
  end, keep RF out of the box and the sound card. Add them before the first test
  with power: RF getting into GP16 is one of the few things that could key the
  radio without the node asking.
- If the radio's headphone output carries any DC, a 1 µF film capacitor in series
  with each audio lead at the radio end blocks it. Whether the IC-7300's PHONES jack
  is DC-coupled is not documented in its manual; a multimeter on the jack, with the
  radio on and no plug in it, tells you.
- The Pico 2's own LED (GP25) lights while the key is closed, and flashes if the box
  has tripped.
- **Never attach an SWD probe or debugger while the box is plugged into the radio.**
  The firmware keeps its clock and its watchdog running while a debugger halts the
  chip, so a halt with the key closed ends in a watchdog reset half a second later,
  but a debugger can also rewrite any register, the watchdog's included, and then
  nothing is left to open the key.

Check the box before it goes anywhere near a radio. Plug the box into the computer,
with nothing running and nothing in the key jack, and with a multimeter:

1. **Volts, tip to sleeve: 0 V.** The optocoupler's output is open, so a voltmeter
   across it reads the meter's own leakage, which is 0.00 V on any meter. Anything
   else means the transistor is conducting (a wrong part, a solder bridge, the
   collector and emitter swapped).
2. **Diode test, tip to sleeve, both ways round.** Put the meter on its diode or
   continuity range, touch red to tip and black to sleeve, then swap them. Both ways
   must read open (no beep, "OL", or the meter's over-range). A reading in either
   direction means the output is shorted or the part is in backwards: do not plug it
   into the radio.

Step 2 cannot tell a working PC817 from one whose LED is wired backwards: both
read open. No meter test on the wired box can tell either. From GP16's pad to
ground the 4.7 kΩ, the LED and the pad's own protection diodes conduct in parallel,
whichever way round the meter is (the safety audit's report, "Meter checks, box
only"). So check the LED's wiring against the diagram above instead:

3. **By eye and by continuity, box unplugged:** PC817 pin 1 through the 470 Ω to
   GP16 (pin 21), pin 2 to GND (pin 23), pin 4 to the tip, pin 3 to the sleeve.
4. **Volts across the 470 Ω, box plugged into the computer, nothing running: 0 V.**
   Anything else means the LED is lit with nothing asking, wired to a supply pin,
   and would key the radio.

An LED wired backwards keys nothing, safely: the first keying test shows it, with
the node reporting the radio not heard keying.

## Flashing the firmware

The firmware is in `firmware/pico2-keyer`. GitHub builds it on every push, branches
included, so take it from the right run: the CI run for the commit on `main` that
the safety audit names, not simply the latest one. Download that run's
`pico2-keyer-firmware` artifact and unzip it to get `pico2-keyer.uf2`.

1. Unplug the box from the radio.
2. Check the file you are about to copy: `sha256sum pico2-keyer.uf2` on Linux,
   `shasum -a 256 pico2-keyer.uf2` on a Mac, against the SHA-256 the safety audit
   published for that commit (the project's `audit/report.md`). The
   `pico2-keyer.uf2.sha256` in the same zip, and the checksum in the run's log and
   summary, only show that the download is intact: they come from the same run.
3. Hold the Pico 2's BOOTSEL button while plugging it into the computer. It appears
   as a drive called `RP2350`.
4. Copy `pico2-keyer.uf2` onto that drive. The Pico 2 restarts as the keyer box.
5. **Unplug it and plug it in again.** The first boot after flashing reports
   `OTHER`, not `POWER`, and the node will not key a box that reports `OTHER`: the
   replug makes it `POWER`. `hfnode keyer --config C check` prints what the box
   reports (C is your `hfnode.toml`).

`hfnode devices` then marks it (`<- the keyer box`). Its USB name is `PICO2-KEYER`,
and the node opens no port that does not carry that name.

CI's build is pinned to one compiler (`firmware/pico2-keyer/rust-toolchain.toml`)
and stamps the commit into the firmware, which the box reports in `HELLO`. Set
`[keyer] firmware_build` to that build id (the first eight characters of the
commit; the CI log and summary print it) and the node refuses to talk to a box
running anything else.

The file you flash is always the CI download, checked against the SHA-256 the
audit published (step 2). Rebuilding the firmware yourself is a way to check that
hash, not a way around it:

- **On Linux, `sh firmware/pico2-keyer/build.sh COMMIT`** (in
  [firmware/pico2-keyer/README.md](../firmware/pico2-keyer/README.md), "Building it
  yourself") gives a UF2 byte for byte the same as CI's for that commit, wherever
  your checkout is. It builds the commit at one fixed path,
  `/tmp/pico2-keyer-build`, which is how CI and the audit build it. No other way of
  building is checked to give CI's file.
- **On a Mac,** a build, `build.sh` included, has never been compared with CI's.
  If its SHA-256 matches, the download is what that commit builds to. If it
  differs, that says nothing about the download: go by the audit's hash.
- **A plain `cargo build`** lays the code out differently depending on where the
  checkout is, and without the build id the box reports `-`. Fine for
  development, never for operating.

## Setting up the radio

Set these at the radio; the node cannot read or change them.

- **Key type: Straight**, for the jack the box plugs into, before anything is
  plugged in. IC-7300: MENU, KEYER, EDIT/SET, CW-KEY SET, Key Type: Straight (lines
  3048-3051; for an external keyer the manual says to select Straight, lines
  999-1002). On a radio with only a paddle input, use its straight-key setting. This
  is not a preference: in Paddle or Bug mode a closed contact sends dits by itself,
  the box's own key-down limit never sees one long key-down, and the node's sidetone
  check cannot tell the radio's keyer from its own text.
- **Break-in on**, semi or full (IC-7300: VOX/BK-IN until BKIN or F-BKIN shows,
  lines 2730-2738 and 2796-2808).
- **Sidetone on, and heard in the headphone output.** The node listens for it. On the
  IC-7300, Side Tone Level (default 50 %) and Side Tone Level Limit (lines
  3011-3017). With the limit ON the manual says it "disables the CW side tone when
  you rotate AF RF/SQL (inner) above the side tone level": if `hfnode keyer
  sidetone` hears nothing, turn the limit OFF or the AF gain down.
- **CW mode, the agreed frequency, and the power you want.** Set `station.frequency_hz`
  to the same frequency: the node only logs it and checks it is in an amateur band.
- **The radio's own keyer speed does not matter**: in straight-key mode the box times
  every element.
- **Volume** so that band noise is plainly audible but not clipping. `hfnode keyer
  check` reports the level.

The IC-7300's Time-Out Timer does not cover keying from the KEY jack (it covers
transmitting started by CI-V or TRANSMIT, lines 6281-6285), and most QRP rigs have
none: the box's limits and the node's checks below are what stop a stuck key.

## Configuration

In `hfnode.toml`:

```toml
[station]
rig = "keyer"
serial_port = "/dev/ttyACM0"   # the box; on Linux better its /dev/serial/by-id/ name
frequency_hz = 7_030_000       # what the radio is set to
key_speed_wpm = 18
max_key_seconds = 46           # at least 46 at 18 wpm, 42 at 20: hfnode says if too low

[audio]
device = "..."                 # the sound card the radio's headphone jack goes into
pitch_hz = 600                 # the radio's CW pitch

[keyer]
commissioned = "none"          # the bring-up stage passed, below
# sidetone_hz = 600            # if the sidetone pitch differs from audio.pitch_hz
# min_level_dbfs = -65         # quieter band noise than this: the node will not key
# max_duty_percent = 50        # at most this share of duty_window_secs keying (max 50)
# duty_window_secs = 600       # the window it is measured over (max 600)
# firmware_build = "a1b2c3d4"  # the box must report this build in HELLO
```

`hfnode devices` lists the serial ports and audio inputs. On Linux, the box's
`/dev/serial/by-id/usb-..._PICO2-KEYER_...-if00` name does not change when other USB
devices come and go; `/dev/ttyACM0` can. On a Mac it is `/dev/cu.usbmodem...`, on
Windows a `COM` port.

`baud`, `civ_address` and `power_watts` are not used with the box.

## Bring-up

Like the IC-7300's, in stages: `[keyer] commissioned` names the last one passed,
and commands that need a later one are refused. Every step from "listen" on keys the
radio: do them yourself at the radio, at its lowest power, into a dummy load, with
the radio's power switch in reach and this page's "Stopping it by hand" read first.

**Stage zero: measure the radio's own key line**, with a meter and no computer
involved. The box is built for a line the radio pulls up to a few volts and that
draws about a milliamp, which is what nearly every modern transceiver does, but
nothing in the node can check it:

- Radio on, in CW, nothing in the key jack. Measure **tip to sleeve** with a
  voltmeter: expect +3 V to +15 V (the IC-7300's KEY jack is not specified in its
  manual; measure yours). A **negative** voltage, or more than **17 V**, is outside
  the PC817's ratings as wired here.
- Short the tip to the sleeve through a milliammeter (or a 1 kΩ resistor, and
  measure the volts across it): expect well under 1 mA. More than **1 mA** is more
  than the PC817's output should carry for years.
- If the line is negative, over 17 V, or over 1 mA, **do not use this circuit.**
  Use a photoMOS relay instead (an AQY212GH or AQY211EH: a few hundred volts either
  polarity, tens of mA, no polarity to get wrong), driven from GP16 through the same
  470 Ω resistor, with the same 4.7 kΩ pull-down.
- Then the box's own meter checks, under "Wiring the box" above.

**Stage none** (keys nothing):

1. `hfnode devices`: the box is listed and marked.
2. `hfnode keyer --config C check`: the box answers with its limits (run 60 s,
   key-down 1000 ms, link timeout 2000 ms), its key is up, the audio is arriving and
   the band level is above `min_level_dbfs`, and there is no steady tone at the
   sidetone pitch (a carrier, or the key held at the radio).
3. `hfnode listen --config C`: the node decodes CW on the band through the sound
   card. Tune to a busy CW frequency if yours is quiet, then back.

Then set `commissioned = "listen"`.

**Stage listen, with no RF at all first.** Before the radio can put out power,
prove the whole chain keys and is heard:

4. Turn the radio's **power output to its minimum** and take the antenna off; if
   your radio can be put in a transmit-inhibit or tune-disable state, use it. Plug
   the **dummy load** in. A dummy load is required from here on, for every bring-up
   test: these tests key the radio on purpose, including one that holds the key down
   for a second.
5. `hfnode keyer --config C key "TEST"`: the radio keys TEST, and the node reports
   it heard the sidetone follow the box (`heard`). If not, it says what it measured.
6. `hfnode keyer --config C sidetone`: keys `DE <call>` and measures the sidetone's
   delay, level and pitch. It passes only if the sidetone is at least 15 dB over the
   band noise; if it says so, set `[keyer] sidetone_hz`, or change the radio's
   sidetone (monitor) level. The level it measures is kept in `state_dir`, and the
   node uses it afterwards to tell a quiet sidetone held on from band noise.
7. **Stepped power.** Repeat step 5 at each power setting you mean to use, lowest
   first, into the dummy load, watching for anything that changes with power: the
   node mis-hearing its own sidetone, the box tripping, the radio keying when
   nothing asked it to. RF getting into the key lead shows up here and nowhere else.
   Add the ferrites before you blame anything else.

Then set `commissioned = "keying"`.

**Stage keying** (tests the box's own limits; each identifies first, and each holds
the key down on purpose). Into the dummy load, at minimum power:

8. `hfnode keyer --config C hangtest`: the node identifies, then makes the box's
   control loop hang during a short transmission. Its hardware watchdog must reset
   it and open the key within half a second, without the node's help. The box comes
   back by itself, tripped (`WATCHDOG`): it keys nothing more until it is
   **unplugged and plugged in again**, which you do afterwards. The test fails if
   the box did not come back tripped, or if the node did not measure the key down at
   all, so a passing run means something was really keyed. As after `stucktest`,
   expect `state_dir/tx-inhibited` afterwards: read it, check it says the watchdog
   fired, then remove it with the node stopped.
9. `hfnode keyer --config C stucktest`: the node identifies, then makes the box hold
   its key down. Its 1 s key-down limit must open the key and trip the box. Unplug
   the box and plug it in again afterwards. A tripped box makes the node latch its
   transmit inhibit as soon as anything asks the box for its state, so expect
   `state_dir/tx-inhibited` to be there afterwards: read it, check it says the box
   tripped, then remove it with the node stopped.
10. `hfnode keyer --config C linktest`: the node keys a long message and then stops
    talking to the box, as it would if it were killed or its cable came out. The box
    must open its key by itself within its 2 s link timeout and report the run ended
    `LINK`.

Then set `commissioned = "done"`, and `hfnode run` will start.

`hfnode keyer --config C rx` stops the box and checks the key is open at any time,
and that no steady tone is heard. If it cannot confirm the key open (the key seen
held at the radio, no audio from the radio, or a steady tone at the pitch), it
latches the transmit inhibit, as the node does: look at the radio before clearing
it.

## What stops a stuck key

Fastest first:

1. **The box's key-down limit.** No element is longer than a dash at 5 wpm
   (720 ms). A key-down past 1 s opens the key and trips the box: it refuses to key
   until it is unplugged and plugged in again, and the node latches its transmit
   inhibit. The loop also times the key **pin** by its own clock readings, apart
   from its Morse timeline and at the box's own 1 s whatever limits the keyer was
   given, so runs sent back to back cannot hold the pin high past that limit, and a pass of the loop that comes more than 10 ms late with the key
   down trips the box too.
2. **The box's rest and duty budget.** It refuses a new run until the key has been
   up for a second, and refuses one whose key-down time is more than its budget: the
   key down at most 55% of any 10 minutes, and half the time in the long run. On a
   handheld the budget counts the whole time the PTT is down, not only the tone.
   The node waits these out before each run, and keeps its own window (`[keyer]
   max_duty_percent`, `duty_window_secs`).
3. **The box's hardware watchdog.** Its control loop feeds the watchdog once per
   pass and nothing in a pass waits. If the loop stalls for 0.5 s the chip resets and
   its key pin goes back to open, and the box comes back tripped: a restart never
   clears a trip or refills the duty budget (the box keeps both in registers a reset
   leaves alone and unplugging clears). After a `TEST HANG` the loop stops feeding it
   on purpose; if the reset has not happened 1 s later, the loop opens the key
   itself. A panic or a processor fault opens the key first, then stops, for the
   watchdog to reset the box.
4. **The box's clock check.** Every 50 ms the loop compares its clock, which every
   limit above is timed on, with the processor's own count of its cycles. A clock
   that stops or slows while the processor runs on (or a loop that stops checking)
   opens the key, trips the box and stops it feeding its watchdog. If the processor
   clock itself stops, no software runs to see it: the watchdog, on the other
   clock, resets the chip.
5. **The box's link timeout.** A run stops when no valid line has come from the node
   for 2 s (the node checks in every 0.25 s while keying): hfnode was killed or
   hung, the computer crashed. It stops at once when USB goes away: the cable
   pulled, the computer's USB reset or suspended. Closing the port is not itself a
   stop — the box is told nothing about it — so what ends the run is one of those
   two. `hfnode keyer linktest` checks it.
6. **The box's run limit.** No run longer than 60 s.
7. **No keying over a tone.** Before every piece, a steady tone at the sidetone
   pitch over the last second (a station's carrier, or the key already closed at
   the radio) stops the node keying.
8. **The node's sidetone check after every piece.** It must hear the sidetone
   follow the box's elements (or it stops and keys nothing more until its next
   retune: the cable is out, the radio is off or not in straight key, the sidetone
   is off). Then it must hear the sidetone stop: a tone that goes on after the box
   opened its key means the key is closed at the radio (a shorted optocoupler or
   cable). It judges "stopped" against the band's level from before the run and
   against the sidetone level `hfnode keyer sidetone` measured, not the audio just
   before the run, which may already be that tone; and a tone that comes back
   within 10 s, having never been heard to stop for half a second, counts as held
   too, so that a dropout in the audio does not clear it. Any such verdict latches
   the transmit inhibit and emails `alert_to`, as for the IC-7300, and the rig
   refuses to key again for as long as the node runs, whether or not the key then
   lets go. If the audio stops before it can tell, it takes the key as held, and
   does the same.
9. **A steady tone for 30 s** at the sidetone pitch latches the inhibit too. The
   node looks for one before every transmission and every quarter second while
   idle.
10. **The station's watchdog** (`max_key_seconds`) and the inhibit file in
    `state_dir`, as for the IC-7300. A stop that cannot confirm the radio back on
    receive — including Ctrl-C and a stop from systemd — writes that file, so a node
    stopped with a stuck key does not start up keying. A node started on a box that
    is already tripped keys nothing, latches that file and emails `alert_to`.

What the node cannot see:

- **SWR or power.** The box reads nothing back from the radio. There is no high-SWR
  cut-out and no "no output" check on this rig, which the IC-7300 over CI-V does
  have: a bad load, a cut coax or an antenna that fell down looks exactly like a
  good one here. That is why the bring-up tests want a dummy load, and why keeping
  the antenna in good order is on you.
- **The radio transmitting without keying.** In CW that puts out no power.
- **A radio whose sidetone stays silent while it keys.** That fails the sidetone
  check, so the node stops and keys nothing more until its next retune — a radio in
  that state is keyed once per retune while nobody fixes it, not once ever.

## Stopping it by hand

If the radio is transmitting and you want it stopped now, in this order:

1. **Pull the key plug out of the radio.** That opens the key whatever the box, the
   computer or the software is doing, and it is the only step that does not depend
   on any of them.
2. **Switch the radio off.** Its own power switch, not the computer's.
3. Pull the box's USB cable. The box loses power and its key pin goes low, but a
   radio already latched into transmit by a stuck key stays there until step 1 or 2.
4. Only then deal with the computer: Ctrl-C in the node's terminal (it stops the
   box, forces receive, and writes the inhibit file if it cannot confirm receive),
   or `systemctl --user stop hfnode`.

Ctrl-C is last on that list on purpose. It has to reach a running program, get the
radio's attention over USB and be answered; pulling the plug does not.

Afterwards, `state_dir/tx-inhibited` may be there: read it, find out what happened,
and only then stop the node, delete the file and start it again.

## False alarms

All of these stop the node from transmitting until you remove the inhibit file,
which is the safe way to be wrong:

- A station answering on your exact pitch the instant the node unkeys, and carrying
  on for half a second, looks like the sidetone going on. The operating guide's
  wait before answering avoids it.
- A carrier on your frequency at the sidetone pitch for 30 s. A shorter one only
  holds the node's reply back: it does not key over it.
- A tone at your pitch that starts within 10 s of the node's over, if the node never
  heard the key open in between. Half a second of quiet after the over is enough to
  clear it, which the usual wait before answering gives.

And the node's own limits can refuse a run rather than inhibit anything: the box's
rest, its duty budget, or the node's duty window. It says which, and waits.

## A handheld through its headset jack

**This section is for a stock UV-K1: its own firmware, and no paddle rework.**
NR7Y's paddle rework removes R72, which cuts the 3.5 mm sleeve, the contact this
cable holds as the PTT, from the radio's PTT (NR7Y's rework guide, as the safety
audit read it). So the cable below cannot key a reworked radio: for one running
NR7Y's firmware, see "A UV-K1 on NR7Y in CW mode (paddle-reworked)" below. Any
other combination (NR7Y without the rework, or the rework on the stock firmware) is
not covered here: ask the safety audit first. The settings list in this
section ("Setting up the radio, every session") is the stock firmware's.

The same box can drive an FM handheld, here the Quansheng UV-K1, through its
two-pin headset jack (the "K-plug"). The radio keeps its own firmware and needs no
programming cable. An FM handheld has no key jack and no CW mode: it transmits
while its PTT is held, and sends whatever reaches its microphone. So with `[keyer]
output = "ptt"` the box holds the PTT for each piece and keys a 700 Hz tone into the
microphone in Morse. That is MCW: an FM carrier with a keyed audio tone. The node
hears the radio through its speaker output. With the squelch open the speaker
carries receive noise, which goes quiet while the radio transmits and comes back
after.

```
radio speaker ──10 kΩ / 1 kΩ / 1 µF──► sound card ──► hfnode: decoder, receive-noise monitor
radio PTT     ◄──PC817 (GP17)────────┐
radio mic     ◄──filter, 1 µF (GP18)─┤ keyer box ◄──USB── hfnode: keyer rig (MCW)
radio PTT     ──BAT85──► GP19 ───────┘
radio ground  ── box ground, sound card ground
```

Manual references below are lines of `pdftotext -layout` output of the stock UV-K1
manual (`documentation/K1_EN.pdf` in github.com/armel/k1-teardown), as in the
project's safety audit.

**Nothing here has been tried on a radio.** You measure on your own K1, on the
bench and before anything keys: which contact is which, whether its time-out timer
ends a transmission held through the jack, and the tone level.

Part 97 allows MCW on 2 m only from 144.1 to 148 MHz (97.305(c), from memory, not
checked against the eCFR). The node refuses `output = "ptt"` outside 50.1-54,
144.1-148, 222-225 and 420-450 MHz. The other station must send MCW too: any FM
radio with a Morse tone going into its microphone.

Do not use an AIOC, or any other cable that keys the PTT from a sound card or a
serial line. Such a cable has none of the box's limits, so a PTT it holds stays
held: an unlimited carrier.

### What you need, on top of the box

- A second PC817, a 470 Ω resistor and a 4.7 kΩ resistor, for the PTT, wired as
  the key line is.
- For the tone: two 10 kΩ resistors, two 22 nF capacitors, a 47 kΩ resistor, a
  1 kΩ trimmer and a 1 µF film or ceramic capacitor rated 25 V or more.
- For the PTT line: a BAT85 Schottky diode and a 1 nF capacitor.
- For the speaker: a 10 kΩ resistor, a 1 kΩ resistor and a 1 µF capacitor.
- A two-pin K-plug with its six contacts on separate wires: a 3.5 mm and a
  2.5 mm three-contact plug, 12 mm apart, as Kenwood-style speaker-mics use. A
  cheap speaker-mic to cut up does. Not an AIOC.
- Two clip-on ferrites.
- A meter, a 1 kΩ resistor and a clip lead, to identify the contacts.
- A second 2 m receiver, to set the tone level.
- A 50 Ω dummy load for the radio's antenna connector, rated for 5 W.

### The cable

**Identify the contacts before you build anything** (the audit's C1). The manual
names the jack ("Headphone/Microphone Jack", K1_EN.txt:185) but gives no pinout.
The one schematic found is the AIOC's, a USB cable made for radios with this
connector (github.com/skuep/AIOC, `kicad/k1-aioc/k1-aioc.kicad_sch` at commit
159895a, line 2466):

> K connector consists of two TRS connectors (3.5mm and 2.5mm) spaced 12mm apart.
> Upper connector (2.5mm) T: SPK+, R: TxData (from Radio), S: GND.
> Lower connector (3.5mm) T: +V/PTT2, R: MIC+, S: PTT1/RxData (to Radio).

That schematic covers this connector in general and was not checked against a K1's
board. It puts the PTT on the same contact as the radio's serial input. The old
pinout in [handheld.md](handheld.md) had the two plugs the other way round, and was
written from memory. So check each contact on your own radio, with the bare plug in
the jack and each of its six contacts on its own wire:

1. **Radio off, battery out.** Measure continuity from each contact to the battery's
   negative contact on the radio. The contacts that beep are ground (expect the
   2.5 mm sleeve).
2. **Radio on, on its battery only.** No USB-C, no charger, squelch 0, volume up,
   power L, the dummy load on. Measure the DC volts from each contact to ground.
   Expect a few volts on the PTT contact, the serial contact and +V (about 3.3 V is
   a logic level). Then measure AC volts: the speaker contact shows the hiss.
3. **Only then, the PTT.** Touch the contact the AIOC calls PTT (the 3.5 mm sleeve)
   to ground through the 1 kΩ for a second, and touch no other contact. If the red
   transmit light comes on (K1_EN.txt:165), that is the PTT. A speaker-mic's
   switch does the same with no resistor; the 1 kΩ keeps any other contact safe if
   you picked the wrong one. This transmits, so use the dummy load.

Write the results down:

| Contact | Ground (radio off) | DC volts (radio on) | Hiss | Keys the radio | Used for |
|---|---|---|---|---|---|
| 2.5 mm tip | | | | not tried | speaker (AIOC: SPK+) |
| 2.5 mm ring | | | | not tried | **not connected** (AIOC: serial from the radio) |
| 2.5 mm sleeve | | | | not tried | ground (AIOC: GND) |
| 3.5 mm tip | | | | not tried | **not connected** (AIOC: +V/PTT2) |
| 3.5 mm ring | | | | not tried | microphone (AIOC: MIC+) |
| 3.5 mm sleeve | | | | | PTT (AIOC: PTT1, serial to the radio) |

Build the cable only if your table gives everything in the last column: one
ground; one contact with the hiss; one that keys the radio and has 2.5-25 V on it
with the PTT open; and the microphone. If it does not, stop and ask. Do not guess.

Put the PTT contact's DC volts in `[keyer] ptt_contact_volts`. With `output =
"ptt"` the node keys nothing until that value is there, and it refuses a value
outside 2.5-25 V. Below 2.5 V the PTT line could not read high; above 25 V is too
close to the diode's 30 V rating.

**Used:** ground, PTT, microphone, speaker. **Never connected:** the serial contact
from the radio and the +V contact. Cut them short and insulate them inside the
plug. The serial port can rewrite the radio's memory, calibration included (the
audit's report, §2), so nothing of ours goes near it. If the PTT does share its
contact with the radio's serial input, as the AIOC's label says, the box only ever
holds that contact low, exactly as a speaker-mic's PTT does. A steady low is a
serial "break", never a frame, and nothing of ours sends serial on it. The PTT
line's diode can only pull the contact up, through the chip's own pull-up (at most
about 0.1 mA), to the idle level of a serial input.

**The speaker to the sound card:** 10 kΩ in series from the speaker contact, 1 kΩ
from there to ground, and 1 µF from there to the sound card's input. That gives
about 1/11 of the radio's output. The radio can put out up to about 2 V RMS (audio
power at least 0.5 W, K1_EN.txt:508). It also blocks DC in both directions and puts
at least 11 kΩ on the radio's output, which is safe whether its amplifier drives one
side of the speaker or both. Set the level with the radio's volume and `hfnode
keyer check`.

**RF:** put a clip-on ferrite at each end of the cable, keep the wires short, and
fit the 1 nF on GP19. 5 W on 2 m can couple into a PTT wire and hold it down. The
box's PTT line check and the radio's time-out timer are what catch that, and the RF
step of the bring-up below is where you find out.

### The box

Wired as well as the key line, which stays as it is. One box can carry both; the
node's `[keyer] output` picks which one it uses.

```
Pico 2                         PC817 #2                   K-plug
GP17 (pin 22) ──470 Ω──► 1 anode     collector 4 ──────── PTT contact
GND  (pin 23) ─────────► 2 cathode   emitter   3 ──────── ground contact
GP17 ──4.7 kΩ── GND

GP19 (pin 25) ──► BAT85 anode, cathode (the band) ──────── PTT contact
GP19 ──1 nF── GND
```

The tone, from GP18 (pin 24) to the microphone contact:

1. GP18, then 10 kΩ, to a point A; 22 nF from A to ground.
2. A, then 10 kΩ, to a point B; 22 nF from B to ground.
3. B, then 47 kΩ, to one end of the 1 kΩ trimmer; its other end to ground.
4. The trimmer's wiper, then 1 µF, to the microphone contact.

- **The PTT (the audit's C9).** It works like the key line. GP17 high lights the
  PC817's LED through 470 Ω, about 4.5 mA. The 4.7 kΩ holds GP17 low whenever
  nothing drives it (erratum RP2350-E9, as for GP16). The collector goes to the PTT
  contact and the emitter to ground. Figures from the Sharp PC817 data sheet
  (D2-A03101EN), as the design spec gives them:
  - collector-emitter saturation voltage at most 0.2 V (at IF 20 mA, IC 1 mA);
  - dark current at most 100 nA (at VCE 50 V);
  - BVCEO 80 V;
  - current transfer ratio 50-600 % at IF 5 mA.

  At the lowest ratio, 4.5 mA in the LED can sink 2.25 mA. A 30 kΩ pull-up on the
  PTT contact passes about 0.11 mA, so the output is saturated, far under the radio
  chip's input-low threshold of 0.99 V (0.3 VCC, PY32F071 data sheet Rev 0.7, Table
  5-20, via the audit). With the LED off, 100 nA through a 30-70 kΩ pull-up moves
  the contact by 7 mV at most. The bring-up measures the contact under 0.4 V while
  the box holds the PTT.
- **The tone.** GP18 runs a 700 Hz square wave on the RP2350's PWM, on only inside
  a Morse element. The two RC poles take its harmonics down: the third about 21 dB
  and the fifth about 33 dB under the tone. The 47 kΩ and the trimmer bring it to a
  microphone's level, at most about 14 mV peak. The 1 µF passes the tone and blocks
  the microphone's bias. Start with the trimmer at its lowest, and set it during the
  bring-up.

### The PTT sense

GP19 reads the PTT contact through the BAT85. The anode is on GP19, the cathode (the
end with the band) on the PTT contact, the pin's own pull-up is on, and 1 nF goes
from GP19 to ground.

- **PTT held** (by the box, the radio's own button, or a fault): the contact is
  under 0.2 V. It pulls GP19 down through the diode to under 0.45 V (BAT85 forward
  voltage at most 240 mV at 0.1 mA, Nexperia BAT85 data sheet), which is under the
  RP2350's 0.8 V input-low level.
- **PTT open, radio on:** the diode is reverse biased and GP19 reads high.
- **Cable out, or the sense wire open:** nothing is on the diode's cathode, and
  GP19 reads high on its pull-up, as if the PTT were open.
- **Radio off:** the reading means nothing, and nothing relies on it. The node
  checks the receive noise before keying.

A resistor divider would not do here. Erratum RP2350-E9 can hold an undriven input
near 2.2 V, which could read high with the PTT held: the unsafe way. The diode can
only ever pull the contact up, by at most about 0.1 mA, and never down, so it
cannot key the radio.

The box uses the line three ways. It refuses `MCW` while the line reads low: the
PTT already held (or the radio off, if its contact then reads low). 100 ms after it
closes the PTT, the line must read low, or the box ends the run `LINE` with nothing
keyed: the cable is out, the sense wire is open, or the radio is off. And 100 ms
after it lets the PTT go, the line must read high, or the box trips: something else
is holding the PTT. A cable that is out does not stop `MCW` from starting, since
the line reads high; the node refuses first anyway, as no receive noise reaches it,
and the box's 100 ms check ends the run if it does start.

### Setting up the radio, every session

The node cannot read any of this. Go through the list each time before the node
keys, and check the antenna before every session the node runs on its own. Lock the
keypad last: unlocking it for any change means going through the list again. Each
item below that names something the radio can transmit by itself is one the box's
limits do not cover.

- **Battery only.** No USB-C cable and no charging base while keying: the manual
  forbids transmitting while charging (K1_EN.txt:108). The blue light, which is on
  while charging (K1_EN.txt:167), must be off. Nothing of the node's goes into the
  radio's USB-C socket.
- **The radio's own antenna, screwed straight onto the radio, with no coax run.**
  Check it is tight before every session. The manual says to use the standard
  antenna (K1_EN.txt:60), and its only word on loads is "Do not transmit when the
  antenna is not installed" (K1_EN.txt:68). "What the node cannot see" below says
  why this matters.
- **Power L** (F+6, K1_EN.txt:280).
- **TOT 1 min** (menu 20, K1_EN.txt:306). This is the radio's own limit and needs
  neither the box nor the node, if the bring-up shows it works through the jack.
- **VOX OFF** (menu 15, K1_EN.txt:285). With VOX on, any sound into the microphone
  keys the radio and none of the box's limits apply. F+7 toggles VOX (K1_EN.txt:282),
  so **lock the keypad**: long-press # (K1_EN.txt:468), or turn AUTOLK on (menu 24,
  K1_EN.txt:315).
- **AL-MOD SITE** (menu 34, K1_EN.txt:294). Set to TONE, the alarm transmits.
- **PTT-ID OFF** (menu 42, K1_EN.txt:309) and **ROGER OFF** (menu 46,
  K1_EN.txt:320). Each adds tones to every transmission.
- **BCL OFF** (menu 12, K1_EN.txt:278). With the squelch open the channel always
  looks busy, and busy-channel lock would then refuse every transmission.
- **Squelch 0** (open). The node listens to the receive noise.
- **The agreed frequency** in 144.1-148 MHz, the same as `station.frequency_hz`, in
  frequency mode (F+3), with **no repeater offset** (SFT-D, menu 8, OFF,
  K1_EN.txt:265-269) and **not reversed** (no R on the display; F+8,
  K1_EN.txt:283).
- **TDR (menu 17) OFF and WX (menu 18) OFF**: no "DW" on the display. With WX at
  CHAN_A or CHAN_B, every transmission goes out on that channel, whatever the main
  channel shows (K1_EN.txt:299-302, :345-351). With dual watch, the channel that
  last heard a call becomes the transmit channel for a while, shown as ">"
  (K1_EN.txt:240-242, :353-355, :356-361). Set channel B to the same frequency as A
  as well (F+2, enter it, F+2 back), so that a ">" can never send anywhere else.
- **Not scanning, and not in NOAA or FM-radio mode**; **NOAA_S (menu 49) OFF**
  (K1_EN.txt:325). A long press of * starts a scan (K1_EN.txt:429), F+5 the NOAA
  mode (K1_EN.txt:464) and F+0 the FM radio (K1_EN.txt:287). In each, pressing the
  PTT answers the call it found, or leaves the mode for a call on the channel
  (K1_EN.txt:432-436, :459-461): not the agreed frequency for certain.
- **D-DCD OFF (menu 43) and D-RSP NULL (menu 39)** (K1_EN.txt:304, :311). With DTMF
  decoding on and the response at REPLY or BOTH, the radio answers a DTMF call with
  an automatic callback, transmitting by itself (K1_EN.txt:411-419).
- **SCR (menu 11), R-DCS, R-CTCS, T-DCS and T-CTCS (menus 4-7) and SAVE (menu 14)
  OFF** (K1_EN.txt:255-263, :275-277, :282-284). Not for safety: the node listens
  to the receive noise, which a tone squelch or the battery saver would cut up, and
  scrambling alters the audio that goes out.
- **Hands off the side keys: the keypad lock does not cover them**
  (K1_EN.txt:467-469). A long press of side key 1 transmits a 1750 Hz tone, and a
  long press of side key 2 sounds the alarm (K1_EN.txt:212-215). Side key 2 pressed
  while the PTT is held enters Air Copy, from which MENU sends the radio's settings
  on 410.0125 MHz (K1_EN.txt:461-470). So while anything can hold the PTT, lay the
  radio on its back with nothing pressing on its sides.
- **MIC sensitivity** (menu 29, K1_EN.txt:326): leave it as set during the bring-up.

### Configuration for a handheld

```toml
[station]
rig = "keyer"
serial_port = "/dev/ttyACM0"   # the box
frequency_hz = 144_150_000     # what the radio is set to, in 144.1-148 MHz
key_speed_wpm = 18
max_key_seconds = 48           # hfnode says if this is too low

[audio]
device = "..."                 # the sound card the radio's speaker goes into
pitch_hz = 700                 # the other station's MCW tone, for the decoder

[keyer]
output = "ptt"                 # MCW on the handheld's PTT ("key": the key jack)
commissioned = "none"
firmware_build = "a1b2c3d4"    # the build id CI printed for the UF2 you flashed
# ptt_contact_volts =          # your own measurement of the PTT contact, below
```

Leave `ptt_contact_volts` out until you have measured the PTT contact yourself
("The cable" above, steps 2 and 3): with it missing the node keys nothing, which is
the point. Then put in the DC volts step 2 gave on the contact step 3 showed is the
PTT.

`firmware_build` is the eight-character build id of the UF2 you flashed, from the
CI run the safety audit names ("Flashing the firmware"). The node refuses a box
that reports another.

`min_level_dbfs` is the receive noise's level here: with the squelch open it is
always there.

### Bring-up with a handheld

As "Bring-up" above, with these differences. You do every step that keys yourself,
at the radio, at power L, into the dummy load, with the K-plug in reach: pulling it
out unkeys the radio.

**Stage zero** (no computer): the contact table above, then the box's own meter
checks on its PTT output, as for the key output (0 V, and open both ways, between
its PTT and ground wires, with the box plugged into the computer and nothing
running; PC817 #2's wiring checked by eye and continuity against the diagram above,
and 0 V across GP17's 470 Ω). Then **the time-out timer** (the audit's C4), with no
box at all: TOT set to 1 min, the dummy load on, hold the PTT contact to ground with
the clip lead and watch the red light. It must go out by itself after about a
minute. If it does not, the radio's own limit does not cover the jack: write that
down, because the box's and the node's limits are then all there is.

C is your `hfnode.toml`, as above.

**Stage none:** `hfnode keyer --config C check`. The box answers with its PTT limit
(60 s), its PTT up and its PTT line high, and the receive noise is at band level
(squelch 0).

**Stage listen:**

1. `hfnode keyer --config C key "TEST"`: the node reports that the radio went quiet
   while the box held the PTT and came back on receive after. Listen on the second
   receiver.
2. **Tone level** (C7). On the second receiver, turn the trimmer up from its lowest
   until the tone is clean and plainly readable, and no further. It must not be
   distorted or over-deviated: the radio's rating is 5 kHz wide and 2.5 kHz narrow
   (K1_EN.txt:492-493).
3. **The PTT contact under the box** (C9). Put the meter on the PTT contact to
   ground while `hfnode keyer --config C key "TTTTTTTTTT"` runs. It must read under
   0.4 V while the red light is on, and what you measured before once it is off.
4. **RF** (C10). At the highest power you will use, with the whole cable connected,
   run `hfnode keyer --config C key "TEST"` several times. Every run must end with
   the node saying the radio is back on receive, and the red light out. Fit the
   ferrites before you blame anything else.
5. **Heat** (C8). Send one full-length piece (30 characters) at that power, then
   feel the radio. The manual gives no duty cycle. The box allows a steady carrier
   for at most half of any stretch of time plus 30 s. If the radio is more than
   warm, use power L and shorter pieces.

`hfnode keyer --config C sidetone` is refused: a handheld has no sidetone.

**Stage keying:** `hfnode keyer --config C hangtest`, `stucktest` and `linktest`,
as above. Each closes the PTT on purpose. The limits they check allow for the 500 ms
lead and for the radio switching to transmit and back.

### What stops a stuck PTT

Fastest first:

1. **The box's PTT line check.** 100 ms after the box lets the PTT go, the line
   must read high, or the box trips and the node latches its transmit inhibit.
2. **The box's watchdog** (0.5 s), **its clock check**, and **a panic or fault**,
   which opens the key and the PTT in its first register write.
3. **The box's link timeout** (2 s) and **USB going away**.
4. **The box's PTT cap.** It refuses an `MCW` whose PTT time is not under 60 s, and
   the PTT down for 60 s trips the box, both by its timeline and by its own watch
   on the PTT pin, which holds to the box's 60 s whatever limits the keyer was
   given. The cap is `PTT_MS` in crates/keyer-core/src/lib.rs. The tests
   that fail without it are `the_ptt_limit_trips_the_box_whatever_holds_the_run_open`
   (crates/keyer-core/src/keyer/tests.rs) and
   `the_ptt_pin_guard_trips_at_the_ptt_limit_whatever_the_keyer_says`
   (crates/keyer-core/src/control/tests.rs).
5. **The box's duty budget.** The whole PTT time counts, lead and tail included:
   a steady carrier for at most half of any stretch of time plus 30 s.
6. **The node's receive-noise check.** The noise must come back within 1.5 s of
   the PTT opening, or the node latches its inhibit and emails `alert_to`; so does
   30 s with no noise at all.
7. **The radio's own TOT** (1 min), if the bring-up shows it works through the
   jack.
8. **You, at the radio**: pull the K-plug.

Not covered: a PTT held by RF on a radio whose TOT does not work through the jack.
The box's line check sees it and the node stops, but neither can let go of a PTT
something else is holding.

### What the node cannot see

- **The antenna.** The radio reports nothing back: no SWR and no power. The manual
  describes no protection against a bad load or a missing antenna. Its only word is
  "Do not transmit when the antenna is not installed" (K1_EN.txt:68), with the
  antenna rated at 50 Ω (K1_EN.txt:488). So a broken antenna or a cut coax keys
  exactly like a good one, and the radio may be damaged. That is why the
  per-session list asks for the radio's own antenna, screwed straight onto it,
  checked before each session, at power L. With an external antenna on coax,
  nobody would notice a cut coax while the node runs on its own, so do not run it
  unattended that way. The safety audit grades unattended use as RISK at best, and
  only after the bring-up above has passed.
- **The radio's settings** (above), its battery level, and whether it is on the
  charger.
- **The other station's squelch.** The 500 ms lead before the Morse is there for
  it to open.

### Stopping a handheld by hand

1. **Pull the K-plug out of the radio.** That opens the PTT, whatever the box, the
   computer or the software is doing.
2. **Switch the radio off.**
3. Then the box's USB cable, then the computer, as in "Stopping it by hand" above.

## A UV-K1 on NR7Y in CW mode (paddle-reworked)

For a Quansheng UV-K1 that has NR7Y's paddle rework and runs NR7Y's CW firmware,
v1.3 or v1.3.1, the release build as published, unmodified. The radio sends real
CW from its own CW mode. The box's key output, through the PC817 of "Wiring the
box" or through one MOSFET (below), closes the radio's PTT line as a straight key,
with `[keyer] output = "key"`, and NR7Y keys its carrier while the line is closed.
The node hears the radio's sidetone in its speaker, through a sound card, and checks
it with the same sidetone monitor as on any radio.

```
radio speaker, 2.5 mm tip   ──10 kΩ / 1 kΩ / 1 µF──► sound card ──► hfnode: decoder, sidetone
radio PTT line, 3.5 mm tip  ◄──PC817 or MOSFET (GP16)── keyer box ◄──USB── hfnode: keyer
radio ground, 2.5 mm sleeve ── PC817 emitter or MOSFET source, sound card ground
```

NR7Y references below are to its source, github.com/briand/uv-k1-k5v3-firmware-custom
at tag v1.3.1 (commit 47075bf), with paths under `App/` unless noted, as the
project's safety audit read it. v1.3 differs from v1.3.1 only in the hand key's
release debounce (`app/cwkeyer.c:99`). The rework is NR7Y's rework guide
(github.com/briand/cw-firmware-docs at 5320013: `docs/hardware/rework-overview.md`,
`rework-uv-k1.md` and `cw-paddle-input.md`) and the older "Paddle Rework" PDF in the
firmware's repository. The wiring, settings and stages are the audit's rulings for
this route (`audit/req-handheld-nr7y-cw-ruling.md`, and
`audit/req-handheld-nr7y-ruling.md` for the settings it keeps), and its ruling for
the bench on the air (`audit/req-on-air-bench-ruling.md`).

**Nothing here has been tried on a radio.** The audit's grade is GO WITH STEPS for
connecting with no RF and for keying on the bench with you at the radio, with the
wiring, settings, stages and on-air routine below: into a 2 m dummy load, or on the
radio's own antenna screwed straight on, at Power LOW 1, never coax. One or the other
is on the radio even in the stages with no RF: a fault can key it, and the manual
says "Do not transmit when the antenna is not installed" (K1_EN.txt:68). On the air
the radio has no protection against a bad load, and nothing in the box would notice
one either, so the audit grades that risk (its H3) RISK on the bench too: check the
antenna each session. Running it unattended is not cleared: see "What stops a stuck
key on NR7Y CW" below.

### The rework, and what it does to the jack

NR7Y's rework guide ("Rework Description" in `rework-overview.md`) makes four
changes:

1. R72 removed: the 3.5 mm sleeve is cut from the PTT. It is now only the radio's
   serial-input line (PA10), which the firmware drives low as a paddle's ground in
   its Port modes (`app/cwhardware.c:546-551`).
2. R70 removed: the 3.5 mm tip loses its +3.3 V feed.
3. A wire from the 3.5 mm ring to the SWDIO pad: the dah input, PA13
   (`app/cwhardware.c:456-457`, `:616-629`). The ring is still the microphone line
   too.
4. A wire from the 3.5 mm tip to the PTT pin (PB10): the dit, or straight-key,
   input.

The older PDF calls change 3 the sleeve; the docs site and the firmware both say the
ring. After the rework a speaker-mic's PTT no longer works, which is why the MCW
cable of the section above cannot key this radio. The key contact is expected on the
**3.5 mm tip**, and the touch test in "Bring-up on NR7Y CW" finds it with no RF.

**If your radio's rework is not this one** (R72 and R70 out, the tip wired to the
PTT, the ring to the SWDIO pad), for example NR7Y's older beta (a wire to a
transistor leg) or only its older "straight key mod", or you are not sure which you
did: stop and ask the safety audit before anything else here. A photo of the board
settles it.

### Wiring for NR7Y CW

- **The key is the box's key output**, GP16, through one of two switches, both
  GO WITH STEPS for the bench and both with the audited firmware unchanged:
  - **The PC817**, the 470 Ω and the required 4.7 kΩ pull-down, exactly as in
    "Wiring the box" above. Its **collector** goes to the **3.5 mm tip** (the PTT
    line) and its **emitter** to the **2.5 mm sleeve** (the radio's ground).
  - **Or one logic-level N-channel MOSFET** (a 2N7000 in TO-92, or similar) and the
    same required 4.7 kΩ pull-down: two parts, the fewest that are safe. See "The
    one-MOSFET key" below.

  Either way the return goes to the 2.5 mm sleeve, **never to the 3.5 mm sleeve**,
  which after the rework is the radio's serial input.
- In CW mode with `CWkin` at `PTT HandKey`, NR7Y reads the PTT line itself as a
  straight key (`app/cwkeyer.c:600-660`): closing the contact is key down, and
  opening it is key up after the release debounce. It is the same line a
  speaker-mic's PTT pulls.
- **Ground is the radio's true ground**: the 2.5 mm sleeve if the AIOC label in "The
  cable" above holds, confirmed by continuity to the battery's negative contact with
  the radio off. Never a contact NR7Y drives as a "port ground".
- **The speaker to a USB sound card**, as in "The cable" above: from the speaker
  contact (the 2.5 mm tip if the AIOC label holds), 10 kΩ in series, 1 kΩ from there
  to ground, and 1 µF from there to the sound card's mic or line input. Its ground is
  the 2.5 mm sleeve. None of the three can be left out: the 1 µF blocks DC both
  ways (the speaker amplifier's DC is not known, and a mic input puts a bias on its
  plug), the 10 kΩ keeps the amplifier's load at 11 kΩ or more whether or not it is
  bridge-tied, and the 1 kΩ cuts up to about 2 V RMS to a mic input's level.
- **Never connected:** the 2.5 mm ring (the radio's serial output), the 3.5 mm ring
  (the microphone and the rework's SWDIO wire) and the 3.5 mm sleeve (the serial
  input). Cut them short and insulate them. Only three contacts are used: key,
  ground and speaker.
- The key and ground can be on one two-pin K-plug or on two separate plugs: the key
  on a 3.5 mm plug's tip, and ground and speaker on a 2.5 mm plug. Either way the
  switch's return (the PC817's emitter or the MOSFET's source) goes to the 2.5 mm
  sleeve.
- A clip-on ferrite at each end of the cable (the audit's C10).

Not any of these:

- **Not NR7Y's own straight-key hookup for a reworked radio**: a plain TRS key in the
  3.5 mm jack with `CWkin` at `Port HandKey` (`rework-uv-k1.md`, "Post rework"). The
  safety audit has not cleared it, for two reasons. Its return is the 3.5 mm sleeve,
  the radio's serial input driven low by the firmware (`app/cwhardware.c:546-551`),
  which would put the cable on the serial contact that stays unconnected here. And
  in `Port HandKey` either the tip or the ring keys the radio
  (`app/cwkeyer.c:610-620`), so a mono plug, or any short from the ring to the
  sleeve, is a key held down, with no time-out in CW to end it; NR7Y's own stuck-key
  check runs only at power-on and when that menu item is chosen (`main.c:222`,
  `app/menu.c:1137`). With `PTT HandKey` and the return on the 2.5 mm sleeve, only
  the tip keys the radio, and a short on the 3.5 mm ring or sleeve keys nothing.
- **Not a mono plug.** It shorts the ring to the sleeve inside the jack, and both
  rings stay unconnected here.
- **Not a 2.5 mm to 3.5 mm adapter from the radio straight into the computer.** It
  would pass the 2.5 mm ring, the radio's serial line, and the full speaker level
  with no DC block. A TRS plug in a Mac's headset jack meets the Mac's headphone
  output, not an input: the Mac would then drive the radio's speaker and serial
  lines. A Mac's headset jack can't take the radio's wires: use the USB sound card.
  On an attended bench only, the Mac's built-in
  microphone may listen to the radio's speaker instead ("The Mac's microphone, on
  the bench" below); a station left running keeps the wired feed.
- **Not a Pico 2 pin wired straight to the key contact** (the audit's grade: NO-GO).
  The box's safe state is its key pin driven low: at power-up, in every fault
  handler and through a watchdog reset (`firmware/pico2-keyer/src/main.rs:14-17`,
  `:174-181`). The radio's PTT transmits when it is pulled low (`App/board.c:86-88`,
  `driver/gpio.h:79`). Wired directly, every safe state of the box would key the
  radio, with no radio time-out in CW to stop it, and a Pico that is resetting, in
  BOOTSEL or unpowered does not leave the line open either. A switch turns "pin
  low" into "line open": the PC817's or the MOSFET's output is open whenever GP16 is
  low or undriven, whatever the Pico does. A direct wire would need new firmware,
  with a new build id and hash and a fresh audit, and would still leave the
  resetting and unpowered cases unresolved.
- **Not any other switch** than the PC817, the one MOSFET below, or the photoMOS
  named under "Bring-up" above (an AQY212GH or AQY211EH) if the key line's
  measurement rules out the PC817. The safety audit also allows an NPN transistor
  (a 2N3904 or similar, with a 4.7-10 kΩ base resistor and the pull-down), but it
  saves nothing over the PC817 and is not described here. Any other part or circuit
  goes to the audit first.
- **Not without the 4.7 kΩ pull-down**, whichever switch. It is the only guard
  against erratum RP2350-E9: a pad nothing drives (while the chip starts, resets,
  sits in BOOTSEL or is unplugged) can float near 2 V ("Wiring the box"), enough to
  turn on a MOSFET, a transistor or the optocoupler's LED and key the radio at
  power-up, with no radio time-out in CW.

### The one-MOSFET key

The fewest parts the safety audit clears on the key side: one logic-level N-channel
MOSFET (a 2N7000 in TO-92, or similar) and the 4.7 kΩ pull-down.

```
Pico 2                   2N7000 (or similar)       K1
GP16 (pin 21) ─────────► gate      drain ───────── 3.5 mm tip (the PTT line)
GND  (pin 23) ───────────────────► source ──────── 2.5 mm sleeve (ground)
GP16 ──4.7 kΩ── GND
```

- Gate to GP16 (pin 21), the 4.7 kΩ from the gate to GND (pin 23), drain to the
  3.5 mm tip, source to the Pico's GND and the radio's true ground (the 2.5 mm
  sleeve).
- GP16 high turns it on and closes the key, exactly as it lights the PC817's LED,
  so the audited firmware is unchanged: only the wiring differs.
- **The MOSFET sits in the box**, at GP16, so the gate wire is a few centimetres
  long. The long wire to the radio is the drain, with a clip-on ferrite on it at the
  radio end (the audit's C10).
- **What only the PC817 adds:** a key path isolated from the Pico's ground, and an
  output that takes a line of either polarity or a higher voltage. On this radio
  neither is needed once the key line's measurement shows a positive logic-level
  line, since the line is a 3.3 V processor pin with a pull-up (`App/board.c:73`,
  `:85-87`), and with the wired speaker feed the audio cable ties the two grounds
  anyway. The one side effect: the key wire becomes a second ground path between the
  Pico and the radio, which can bring hum into the receive audio. That is a nuisance, not damage; if it
  confuses the node, use the PC817.
- **RF on the key wire.** With the PC817, RF picked up on the long key wire stays on
  the radio's side; with a transistor it reaches the Pico's ground. What that can do
  fails safe, as far as the code shows: a Pico that resets drives nothing and the
  4.7 kΩ holds the switch off, and a USB link that drops makes the box open its key
  within its 2 s link timeout. A switch that RF turns on, or a key held by any fault,
  is caught by the box's 1 s key-down limit and the node's sidetone monitor, the same
  as with the PC817. The radio runs on its battery, so its ground floats and the
  shared ground carries no supply current: it is not a path for damage. With the
  Mac's built-in microphone in place of the wired feed, the PC817 would leave the
  box and the radio with no wire in common; a transistor makes the key wire and its
  ground the only one.
- **Change to the PC817 and tell the safety audit** if keying on the air (the first
  RF stage of "Bring-up on NR7Y CW") shows the radio keying by itself, a key held
  after the box lets go, box resets or USB drop-outs.
- **The audit's grades:** on the bench, GO WITH STEPS, as for the PC817. Unattended,
  a transistor is acceptable in principle on this battery handheld, graded with the
  rest of the route after the bench; the hardware timer ("What stops a stuck key on
  NR7Y CW") is still required.

Conditions for the MOSFET:

- **The key line's measurement** (stage zero of "Bring-up on NR7Y CW", step 2) must
  read **+2.5 V to +5 V** open and carry **under 1 mA** through 1 kΩ. A negative
  reading means stop: the MOSFET's body diode would conduct and key the radio. Over
  5 V means stop and use the PC817.
- **Its own box meter checks**, in place of steps 1 and 2 of "Wiring the box": box
  plugged into the computer, nothing running, nothing in the radio.
  - Volts, drain to source: **0 V**.
  - Diode test, red on the drain and black on the source: **open**.
  - Red on the source and black on the drain: **one diode drop** (about 0.4-0.8 V).
    That is the body diode, and it is normal.
  - A beep, or near 0 V either way round, means it is shorted: do not plug it in.
  - Volts, gate to GND: **0 V**.
  - With the box unplugged from USB, gate to GND reads about **4.7 kΩ**: the
    pull-down is there.
- **The zero-RF stages prove the rest**, at break-in off with no RF: leakage
  pulling the line low with the box idle shows as a sidetone with no keying (the
  30 s watch in stage none), and a MOSFET that does not turn fully on at 3.3 V shows
  as "not heard" (stage listen, step 3).

### Parts for NR7Y CW

The Pico 2 with its key output, either:

- **the fewest parts:** one logic-level N-channel MOSFET (a 2N7000 or similar) and
  one 4.7 kΩ resistor ("The one-MOSFET key");
- **or as audited:** one PC817, a 470 Ω and a 4.7 kΩ resistor ("Wiring the box");
- if the key line's measurement rules both out, one AQY212GH or AQY211EH photoMOS
  with the same two resistors as the PC817;

and:

- for the speaker, a 10 kΩ and a 1 kΩ resistor and a 1 µF film or ceramic capacitor
  rated 25 V or more;
- a USB sound card with a mic or line input (on an attended bench, the Mac's
  built-in microphone may stand in for it and the speaker parts);
- a two-pin K-plug with its six contacts on separate wires: a cut-up Kenwood-style
  speaker-mic. Not an AIOC, and not a 2.5 mm to 3.5 mm adapter as it is;
- two clip-on ferrites;
- a multimeter, a 1 kΩ resistor and a clip lead;
- the radio's own antenna; or, to keep the bench off the air, a 2 m dummy load,
  50 Ω, rated 5 W or more, with an adapter for the radio's antenna socket;
- the box's USB cable, and solder, tape or a terminal block.

Not needed here: the second PC817, the BAT85 and the 1 nF of the PTT output, and
the MCW tone parts (the microphone stays unconnected). A second 2 m receiver is
optional, handy for hearing the signal.

### Setting up NR7Y, every session

This list replaces the stock one ("Setting up the radio, every session" above) for
these radios. The node can read none of it. Go through it each time before the node
keys. Lock the keypad last: unlocking it for any change means going through the list
again. The names are NR7Y's menu items.

**First, stop if the menu shows `AlarmT`, `ANI ID` or `D Resp`.** Those items exist
only in builds with the alarm or DTMF calling (`ui/menu.c:113-124`), so the radio is
not running the CW release build this list was read from. Stop and ask the safety
audit.

CW:

- **`Mode`: `CW`.** It puts the radio's keyer in charge of its PTT (`radio.c:809`;
  `app.c:1510`).
- **`CWkin`: `PTT HandKey`** (the first item). It reads only the PTT line
  (`app/cwkeyer.c:600-660`; `settings.h:68-80`). **Never a Port mode**
  (`Port HandKey`, a Port iambic or a Port+Btn mode), even on a reworked radio: they
  drive the serial input (PA10) low as a paddle's ground and make PA13 an input
  (`app/cwhardware.c:536-551`, `:616-629`). Never a `USB Port` mode (the USB-C data
  lines), and never an iambic or bug mode, in which a held contact becomes
  auto-repeated elements.
- **`CWbkin` (break-in): `OFF` for every zero-RF stage, `ON` only for the keyed
  stages**, when you decide to key. Off, a key-down plays the sidetone and transmits
  nothing (`app/cwapp.c:102-144`). On, the radio puts out a real carrier and goes
  back to receive 300 ms after key-up (`misc.c:89`, `app/cwapp.c:204-208`).
- **`CWvol`: 1 to 6, never 0** (4 is the default). Write it down and keep it. At 0
  there is no sidetone (`app/cwkeyer.c:191-215`): the node then cannot hear its own
  keying and refuses to key, which is safe, but a stuck key could not be heard
  either.
- **`CWfreq`: the same as `[keyer] sidetone_hz`** (600 Hz for both by default). It
  runs 450-800 Hz in 50 Hz steps: the menu runs 0-7 (`app/menu.c:404-407`,
  `:1111`).
- **`CWmsg1` to `CWmsg4` and `CWmrpt`: never play, repeat or record a message.** A
  message playing or repeating transmits by itself, and repeats on a timer
  (`app/app.c:2086-2092`; `app/menu.c:1188`).

What else can make it transmit, for longer, or somewhere else:

- **`F1Shrt`, `F1Long`, `F2Shrt`, `F2Long` and `M Long`: all five `NONE`.** The side
  key actions include PLAY and REPEAT CW MSG, CODE PRACTICE, MODE, POWER HIGH, PTT,
  1750Hz, MUTE (a calibration write, below), VOX, SCAN and RX MODE
  (`ui/menu.c:502-563`).
- **`VOX`: `OFF`.** A VOX trigger prepares a transmission, and in CW mode that starts
  a CW carrier (`app.c:1146-1157`, `:1365-1366`; `functions.c:370-375`,
  `:251-283`).
- **`Power`: `LOW 1`**, the lowest. Never `USER` (a level stored apart,
  `radio.c:665-666`) or `HIGH`, and nothing above LOW 1 without the safety audit. CW
  uses the channel's power (`radio.c:1555`). The watts at LOW 1 have not been
  measured.
- **`Roger`, `PTT ID`, `STE` and `RP STE`: all `OFF`.** The end of a CW transmission
  sends the end-of-transmission tail (`app/cwapp.c:57`): Roger adds a burst
  (`radio.c:1525-1534`), PTT ID adds tones, STE a tail tone and 200 ms of carrier
  (`radio.c:1496-1511`), and RP STE holds the transmitter up to 1 s more
  (`app.c:1094-1097`, `:1856-1864`).
- **`RxMode`: `MAIN ONLY`**, and VFO B set to the same frequency as A.
  `DUAL RX RESPOND` and `CROSS BAND` can transmit on the other VFO
  (`ui/menu.c:257-263`).
- **`TxODir`: `OFF`**: no repeater offset, and no + or - on the display.
- **The frequency: 144.1-148.0 MHz on the display, the same as
  `station.frequency_hz`, checked before every keyed step.** `F Lock`, in the hidden
  menu, can be anything up to UNLOCK ALL (`ui/menu.c:348-367`): the FCC label does
  not limit where this firmware transmits.
- **Not scanning.**
- **`BusyCL`: `OFF`.** With it on, a busy channel refuses the PTT: safe, but it would
  confuse the keyed stages.
- **`TxTOut`: `01m:00s`, `SetPTT`: `CLASSIC`, `SetTOT`: `VISUAL`.** None of them does
  anything in CW; they matter if `Mode` is ever FM again (`ONEPUSH` latches the
  transmitter on with one press, `app.c:1522-1548`; `SOUND` and `ALL` play the
  time-out alert through the transmitter, `app.c:1187-1226`).
- **`SetLck`: `KEYS ACTIONS`**, so that with the keypad locked the side keys are
  locked too (`app.c:2479-2480`). Not the PTT option, which blocks the PTT while
  locked (`app.c:2456-2459`).
- **The keypad locked, last**: a long press of F (`generic.c:45-56`).

For the node to hear the radio:

- **Squelch 0** (open). The node needs the band noise (`[keyer] min_level_dbfs`) to
  tell the sidetone from the band.
- **`BatSav`, `Scramb`, `RxDCS`, `RxCTCS`, `TxDCS` and `TxCTCS`: all `OFF`**, for a
  continuous receive noise.
- **`Mic`, `Compnd` and `Filter`**: write them down as found, and keep them.

At the radio:

- **Battery only.** No USB-C cable and no charging base while anything can key: the
  manual forbids transmitting while charging (K1_EN.txt:108), and nothing in the
  firmware stops it (`battery.c:167-187` only shows it on the display).
- **A 2 m dummy load, or the radio's own antenna screwed fully on and undamaged**
  with nothing metal touching it, whenever the radio is on with anything plugged in.
  Never coax, an adapter cable or an external antenna: the manual's only rule on
  loads is "Do not transmit when the antenna is not installed" (K1_EN.txt:68), and
  nothing in the radio or the box would notice a bad one.
- **The radio on its back, its antenna pointing away from you, the box, the key lead
  and the computer**, and at least 30 cm from the box and the key lead. The manual's
  own exposure note is 25 mm from the face, antenna up and away (K1_EN.txt:16-18).
- **The clip-on ferrite on the key lead**, at the radio end.
- **Hands off the keypad and the side keys while anything can hold the key.** The
  keypad lock does not apply while the radio transmits (`app.c:2456`). In CW the
  digits send no DTMF (`app.c:2569-2573`), but any key stops or starts something.
  Lay the radio on its back with nothing pressing on its sides.
- **Power the radio on with nothing holding its key**, and **never power it on
  holding PTT and side key 1**: that opens the hidden menu (`helper/boot.c:60-61`),
  which clears the keypad lock and saves (`main.c:172-184`).

Calibration: stay out of the three things that write it. The radio's calibration has
a flash sector of its own, which the menu's Reset never touches
(`settings.c:764-785`), so a damaged one stays damaged.

- **The hidden menu** (`FrCali`, `BatCal`, `BatTyp`, `F Lock`, `350 En`, `Reset`;
  `app/menu.c:58-80`, `settings.c:1285-1288`). Never enter it.
- **`SetVol`.** It sets a gain that every settings save writes into the calibration
  sector (`app/menu.c:1081-1082`, `settings.c:1451-1457`). Never change it. The
  volume knob is fine.
- **`MUTE`** (a side-key action). It sets that gain to 0 (`app/action.c:806`), and a
  save while muted writes the 0 into calibration, so the radio boots silent. The
  side keys at `NONE` cover it.

Ordinary menu changes do not damage calibration: the flash is rewritten only where
its bytes differ (`driver/py25q16.c:284`), and with `SetVol` untouched that byte
does not. Recommended, not required: keep any calibration file saved before NR7Y was
flashed. If there is none, one read-only dump (UVTools2 read, once, with the K-plug
out and nothing keyed) makes a loss recoverable. Never restore or write calibration.

### Configuration for NR7Y CW

```toml
[station]
rig = "keyer"
serial_port = "/dev/ttyACM0"   # the box
frequency_hz = 144_150_000     # what the radio is set to, in 144.1-148 MHz
key_speed_wpm = 18
max_key_seconds = 46           # at least 46 at 18 wpm: hfnode says if too low

[audio]
device = "..."                 # the sound card the radio's speaker goes into
pitch_hz = 600                 # the radio's CW pitch

[keyer]
output = "key"                 # the key output, as a straight key (the default)
commissioned = "none"
firmware_build = "a1b2c3d4"    # the build id CI printed for the UF2 you flashed
# sidetone_hz = 600            # the radio's CWfreq, if it differs from pitch_hz
```

- **`output = "key"`.** Never `"ptt"` with the radio in CW: the PTT output would
  hold a steady carrier for each whole piece. The key output's 1 s key-down limit
  fits CW elements (a dash at 18 wpm is 200 ms), and with it the node runs its
  sidetone monitor.
- **`sidetone_hz`** (by default `pitch_hz`) must be the radio's `CWfreq`.
- **No `ptt_contact_volts`.** It belongs to the PTT output: leave it out.
- The node accepts 144-148 MHz with `output = "key"`
  (`crates/hfnode/src/keyer.rs`, `BANDS_HZ`). Keep to 144.1-148.0 MHz, as the list
  above says.
- `firmware_build` as in "Configuration for a handheld" above.

### Bring-up on NR7Y CW

As "Bring-up" above, with these differences. You do every step yourself, at the
radio, at Power LOW 1, into a 2 m dummy load or on the radio's own antenna (never
coax), with the plug in reach: pulling it out of the 3.5 mm jack opens the key. The
dummy load or the antenna is on in the zero-RF stages too, in case a fault keys the
radio. `CWbkin` stays `OFF` until the keyed steps. The time-out test of "Bring-up with a handheld" is dropped: NR7Y has no time-out in
CW ("What stops a stuck key on NR7Y CW" below), and holding a carrier for over a
minute would prove nothing the code doesn't already show. C is your `hfnode.toml`.

**Stage zero** (no computer, no RF):

1. **Find the contacts** (the audit's C1), with the bare plug in the jack and each
   of its six contacts on its own wire, as in "The cable" above. Radio off, battery
   out: continuity from each contact to the battery's negative contact; the ones
   that beep are ground (expect the 2.5 mm sleeve). Continuity only with the battery
   out; on a powered radio, the meter's volts ranges only. Then radio on, on its
   battery only, in CW with `CWbkin` `OFF`, so that a slipped probe gives only a
   sidetone: DC volts from each contact to ground. Expect about 3.3 V on the 3.5 mm
   tip (the PTT line), and possibly on the serial input. AC volts show the hiss on
   the speaker contact.
2. **Measure the key line**, radio in CW, nothing on the 3.5 mm tip but the meter.
   Open, tip to ground must read **+2.5 V to +15 V** for the PC817, and **+2.5 V to
   +5 V** for the MOSFET. Shorted to ground through a milliammeter (or the 1 kΩ,
   measuring the volts across it), it must carry **well under 1 mA** for the PC817,
   and **under 1 mA** for the MOSFET. A negative reading rules out the MOSFET (its
   body diode would key the radio), and over 5 V means the PC817 instead. If the
   PC817's limits fail too, use the photoMOS, as in "Bring-up" above.
3. **The touch test.** Radio in CW, `CWkin` at `PTT HandKey`, `CWbkin` `OFF`. Touch
   the 3.5 mm tip to ground through the 1 kΩ for a second, and touch no other
   contact. The sidetone must sound and the red transmit light stay off: that
   identifies the key contact, and shows that break-in off puts out no carrier. If
   the red light comes on, pull the plug out and stop. If the sidetone comes from
   any contact other than the 3.5 mm tip, stop and ask the safety audit before the
   cable goes in.
4. The box's own meter checks on its key output: under "Wiring the box" above for
   the PC817, under "The one-MOSFET key" for the MOSFET.

Write the results down:

| Contact | Ground (radio off) | DC volts (radio on) | Hiss | Touch test | Used for |
|---|---|---|---|---|---|
| 2.5 mm tip | | | | not tried | speaker |
| 2.5 mm ring | | | | not tried | **not connected** (serial from the radio) |
| 2.5 mm sleeve | | | | not tried | ground |
| 3.5 mm tip | | | | | key (the PTT line) |
| 3.5 mm ring | | | | not tried | **not connected** (microphone, dah) |
| 3.5 mm sleeve | | | | not tried | **not connected** (serial to the radio) |

Build the cable only if your table gives one ground, one contact with the hiss, and
the 3.5 mm tip sounding the sidetone with +2.5 V to +15 V on it (+5 V at most for
the MOSFET). If it does not, stop and ask. Do not guess.

**Stage none** (no RF, `CWbkin` `OFF`), with the cable in:

1. `hfnode keyer --config C check`, then `hfnode listen --config C`, as in
   "Bring-up" above.
2. **Watch for 30 s** with the box idle: no sidetone and no red light. A sidetone
   here means the key is closed at the radio: pull the plug out.

Then set `commissioned = "listen"`.

**Stage listen, with no RF first** (`CWbkin` still `OFF`), the red light staying off
throughout:

3. `hfnode keyer --config C key "TEST"` must report `heard`.
4. `hfnode keyer --config C sidetone` must pass.

Then, **with RF**, when you decide to key: `CWbkin` `ON`, Power LOW 1, the antenna
checked (or the dummy load on), the plug in reach and the ferrites fitted. On the
air, go through "Before every keyed step" below first.

5. `hfnode keyer --config C key "TEST DE <call>"`, several times: `heard`, a clean
   unkey every time, and the red light off between.
6. `hfnode keyer --config C sidetone` passes again.

On the antenna, RF reaches the key lead for real here (the audit's C10): the radio
keying when nothing asked it to, or not unkeying, is what this stage looks for. Pull
the plug out first.

Nothing above LOW 1 without the safety audit. Then set `commissioned = "keying"`.

**Stage keying** (RF): `hangtest`, `stucktest` and `linktest`, as in "Bring-up"
above, each after the routine below. Each keys a few seconds at most and identifies
itself. Write down what each showed: the safety audit grades unattended use from
these results. Then `commissioned = "done"` lets `hfnode run` start, with you at the
radio: running it unattended is not cleared (below).

**If the node reports the radio not heard.** NR7Y v1.3 holds each key-up 40 ms late
(v1.3.1: 10 ms), so every element sounds up to 40 ms long, which at 18 wpm takes up
most of a 67 ms gap. The node may then judge the keying not heard and refuse to key,
which is safe. Whether it does is for the bench to show. If it does, the fix is a
change to the node or NR7Y v1.3.1, and either goes through the safety audit first;
reflashing the radio is not cleared.

**Before every keyed step, on the air** (the audit's on-air routine; `<call>` is
your callsign):

1. **The frequency**, inside your privileges, set on the radio and checked on its
   display: 144.1-148.0 MHz. `F Lock` can allow more, so the display is the check.
2. **Listen** until you are sure it is clear: the radio's speaker, or `hfnode
   listen`.
3. **`hfnode keyer --config C key "QRL? DE <call>"`** (`CWbkin` `ON`), then listen
   again. Go on only if it is still clear.
4. Run the step with your hand on the plug.
5. **Identify** with `DE <call>` at the end of each group of steps, and at least
   every 10 minutes. `sidetone`, `hangtest` and `stucktest` (before) and `linktest`
   (after) send it already, and so does any text you key that includes it. A bare
   `key "TEST"` does not: key `"TEST DE <call>"` instead.
6. Stop at once if anyone reports interference.

### The Mac's microphone, on the bench

For an attended bench only (stages none to keying, with you at the radio), the Mac's
built-in microphone may pick up the radio's speaker in place of the 10 kΩ / 1 kΩ /
1 µF feed and the USB sound card (the audit's grade: GO WITH STEPS). A station left
running keeps the wired feed, and before any unattended grade the node's results
are repeated on it.

It is acceptable on the bench because nothing keys on what the node hears: the
monitor only refuses (sidetone not heard) or latches, both safe, the box's limits use
no audio, and you are at the radio with the plug in reach. What gets weaker is the
node's own second line:

- The band level now includes the room, so the node cannot tell that the radio is
  off or turned down; with the wired feed that shows as no audio.
- Moving the radio, the Mac or yourself changes the level. A tone held by a stuck
  key can then fail the steady-tone test (each slice within 1.5 dB of the last,
  `crates/hfnode/src/keyer/monitor.rs`), or be dismissed as more than 10 dB under
  the last good sidetone.
- Room sounds near the sidetone pitch (a voice, a whistle, an alert) can make it
  judge wrongly. That fails safe: it refuses or latches.

The key output's checks apply unchanged: the sidetone 15 dB or more over the band,
10 dB over the key-up audio in 85% of the slices, a delay of up to 500 ms.

Every session:

1. **Placement:** the radio on its back, speaker up, 10-30 cm from the Mac, its
   antenna pointing away from the Mac. Mark both positions; nothing moves during the
   session.
2. **The room** quiet: the Mac's sound output muted and Do Not Disturb on (alerts
   would reach the microphone), no music, TV or other radios, and no talking or
   whistling during a keyed command.
3. **The Mac's input:** System Settings > Sound > Input, the built-in microphone,
   its input volume fixed (write it down). If Control Center offers a Mic Mode while
   hfnode listens, choose Standard.
4. **The radio:** squelch 0, the volume knob fixed (mark it with tape), `CWvol`
   fixed (write it down).
5. `hfnode keyer --config C check`: all ok. Write down the band level with the
   radio's volume at zero and at the mark, as the record of how much of the "band"
   is the radio.
6. With `CWbkin` `OFF`: `key "TEST DE <call>"` reports `heard`, `sidetone` passes,
   and the red light stays off. Then the 30 s idle watch: no steady-tone alarm.
7. Run `sidetone` again after any change to the placement, the volume knob,
   `CWvol` or the input volume: it keeps the level the node uses to recognise a held
   key.

If `sidetone` cannot reach 15 dB over the band with `CWvol` at 6, the radio 10 cm
away and the room quiet, use the wired feed. Stage keying run with the microphone
still proves the box's own limits, which use no audio; the sidetone level, delay and
contrast it records hold for that placement only.

### What stops a stuck key on NR7Y CW

Fastest first:

1. **The box's key-down limit** (1 s), here the first stop. It opens the key and
   trips the box.
2. **The box's watchdog** (0.5 s), **its clock check**, and **a panic or fault**,
   which open the key.
3. **The box's link timeout** (2 s) and **USB going away**.
4. **The box's run limit** (60 s), **its rest and its duty budget.**
5. **The node's sidetone monitor**, with the key output's rules (the MCW section's
   receive-noise check does not apply here). The sidetone must be at least 15 dB
   over the band noise (`hfnode keyer sidetone`), and at least 10 dB over the key-up
   audio in 85% of the slices, at an audio delay of up to 500 ms. A stuck key is the
   tone held 0.5 s past the box's key-up, or a steady tone for 30 s. NR7Y plays the
   sidetone in the speaker while it transmits (`functions.c:279-283`;
   `radio.c:1557-1564`) and stops it at key-up (`radio.c:1571-1581`). A stuck key
   latches the transmit inhibit and emails `alert_to`, but the node cannot open the
   line.
6. **You, at the radio**: pull the plug out of the 3.5 mm jack.

Not there:

- **The radio's time-out.** NR7Y switches it off in CW: every poll with the key held
  sets the countdown to 0 (`app/cwapp.c:151`, `:181`), and the countdown fires only
  from a value that is not 0 (`scheduler.c:38-43`, `:65`).
- **A check of the key line.** The key output has no line sense; the BAT85 sense is
  the PTT output's only.

So nothing independent of the box ends a carrier held by a shorted optocoupler or
MOSFET, or by RF on the lead. **Running it unattended is not cleared on this route**, and it needs
one thing more than the MCW route: an independent hardware timer that opens the key
line (or cuts the radio's power) after a few seconds of continuous key-down. Even
after the bench and with that timer, the safety audit grades unattended use RISK at
best, and never on coax.

### Stopping NR7Y CW by hand

1. **Pull the plug out of the radio's 3.5 mm jack** (the K-plug, or the 3.5 mm plug
   if the key has its own). That opens the key, whatever the box, the computer or
   the software is doing.
2. **Switch the radio off.**
3. Then the box's USB cable, then the computer, as in "Stopping it by hand" above.

## Running it outside

The box, the sound card and a Raspberry Pi can sit with the radio, away from the
house. The node's [Raspberry Pi guide](raspberry-pi-setup.md) applies as written,
with this page's configuration. The box needs nothing but its USB cable.

`hfnode run` works under the `deploy/` service and supervise scripts with
`rig = "keyer"`: the systemd unit lets the node open the box (a `ttyACM` device) and
the sound card, and after every stop or crash the unit and the scripts run `hfnode
radio rx`, which with this rig is `hfnode keyer rx`: the box stopped, its key
confirmed open and no sidetone heard, or the transmit inhibit latched. When `run`
stops, its last receive check still hears the radio: the node closes the sound card
after it, not before.

Run it in the foreground, where Ctrl-C is the stop and you can read what it says,
until the whole bring-up above has passed on your radio. Running it unattended is a
separate question: see the safety audit's verdict for your setup, and the control
operator rules for automatic CW, before leaving it on its own.
