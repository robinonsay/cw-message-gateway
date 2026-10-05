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

The IC-7300 over CI-V ([hardware-test-plan.md](hardware-test-plan.md)) works as
before; `station.rig` picks one or the other.

## What you need

- A Raspberry Pi Pico 2 (RP2350; the Pico 2 W works too, its radio unused), with
  headers or wires soldered on.
- A PC817 optocoupler (a 4N25 or 4N35 also works), a 470 Ω resistor and a 4.7 kΩ
  resistor.
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
- The 4.7 kΩ resistor holds GP16 low whenever nothing drives it (while the chip
  starts, resets or is unplugged), so the key is open then.
- The optocoupler keeps the computer's ground and the radio's key line apart.
  Polarity matters on its output: collector to the tip (the radio's key line, which
  the radio pulls up), emitter to the sleeve (the radio's ground).
- The Pico 2's own LED (GP25) lights while the key is closed, and flashes if the box
  has tripped.

Before plugging it into a radio, check it with a meter: plug the box into the
computer, and with nothing running the plug's tip to sleeve must read open.

## Flashing the firmware

The firmware is in `firmware/pico2-keyer`. GitHub builds it on every push to the
repository; download the `pico2-keyer-firmware` artifact from the latest run of the
CI workflow and unzip it to get `pico2-keyer.uf2`.

1. Unplug the box from the radio.
2. Hold the Pico 2's BOOTSEL button while plugging it into the computer. It appears
   as a drive called `RP2350`.
3. Copy `pico2-keyer.uf2` onto that drive. The Pico 2 restarts as the keyer box.

`hfnode devices` then marks it (`<- the keyer box`). Its USB name is `PICO2-KEYER`.

## Setting up the radio

Set these at the radio; the node cannot read or change them.

- **Key type: straight** for the jack the box plugs into. IC-7300: MENU, KEYER,
  EDIT/SET, CW-KEY SET, Key Type: Straight (lines 3048-3051; for an external keyer
  the manual says to select Straight, lines 999-1002). On a radio with only a paddle
  input, use its straight-key setting.
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
```

`hfnode devices` lists the serial ports and audio inputs. On Linux, the box's
`/dev/serial/by-id/usb-..._PICO2-KEYER_...-if00` name does not change when other USB
devices come and go; `/dev/ttyACM0` can. On a Mac it is `/dev/cu.usbmodem...`, on
Windows a `COM` port.

`baud`, `civ_address` and `power_watts` are not used with the box.

## Bring-up

Like the IC-7300's, in stages: `[keyer] commissioned` names the last one passed,
and commands that need a later one are refused. The steps from "listen" on
transmit: do them yourself at the radio, at low power, into a dummy load where you
can, and with the radio's power switch in reach.

**Stage none** (keys nothing):

1. `hfnode devices`: the box is listed and marked.
2. `hfnode keyer --config C check`: the box answers with its limits (run 60 s,
   key-down 1000 ms, link timeout 2000 ms), its key is up, the audio is arriving and
   the band level is above `min_level_dbfs`, and no tone is held at the radio.
3. `hfnode listen --config C`: the node decodes CW on the band through the sound
   card. Tune to a busy CW frequency if yours is quiet, then back.

Then set `commissioned = "listen"`.

**Stage listen** (transmits short tests):

4. `hfnode keyer --config C key "TEST"`: the radio keys TEST, and the node reports
   it heard the sidetone follow the box (`heard`). If not, it says what it measured.
5. `hfnode keyer --config C sidetone`: keys `DE <call>` and measures the sidetone's
   delay, level and pitch. If it says so, set `[keyer] sidetone_hz`.

Then set `commissioned = "keying"`.

**Stage keying** (tests the box's own limits; each identifies):

6. `hfnode keyer --config C hangtest`: during a short transmission the node makes
   the box's control loop hang. Its hardware watchdog must reset it and open the
   key within half a second, without the node's help. The box comes back by itself.
7. `hfnode keyer --config C stucktest`: the node makes the box hold its key down.
   Its 1 s key-down limit must open the key and lock the box. Unplug the box and
   plug it in again afterwards.

Then set `commissioned = "done"`, and `hfnode run` will start.

`hfnode keyer --config C rx` stops the box and checks the key is open at any time.

## What stops a stuck key

Fastest first:

1. **The box's key-down limit.** No element is longer than a dash at 5 wpm
   (720 ms). A key-down past 1 s opens the key and trips the box: it refuses to key
   until it is unplugged and plugged in again.
2. **The box's hardware watchdog.** If its 1 ms control loop stalls for 0.5 s, the
   chip resets and its key pin goes back to open.
3. **The box's link timeout.** A run stops when no line has come from the node for
   2 s (the node checks in every 0.25 s while keying), and at once when the USB
   cable comes out: the computer crashed, hfnode was killed, the cable was pulled.
4. **The box's run limit.** No run longer than 60 s.
5. **The node's sidetone check after every piece.** It must hear the sidetone
   follow the box's elements (or it stops and keys nothing more until its next
   retune: the cable is out, the radio is off or not in straight key, the sidetone
   is off). Then it must hear the sidetone stop: a tone that goes on after the box
   opened its key means the key is closed at the radio (a shorted optocoupler or
   cable). The node then latches the transmit inhibit and emails `alert_to`, as for
   the IC-7300.
6. **A steady tone for 30 s** at the sidetone pitch, whenever the node checks the
   radio (before every transmission and while idle), latches the inhibit too.
7. **The station's watchdog** (`max_key_seconds`) and the inhibit file in
   `state_dir`, as for the IC-7300.

What the node cannot see: the radio transmitting without keying (in CW that puts
out no power), or a radio whose sidetone stays silent while it keys. The second
fails the sidetone check on the first transmission, so the node never keys such a
radio twice.

## False alarms

Both of these stop the node from transmitting until you remove the inhibit file,
which is the safe way to be wrong:

- A station answering on your exact pitch the instant the node unkeys, and carrying
  on for half a second, looks like the sidetone going on. The operating guide's
  wait before answering avoids it.
- A carrier on your frequency at the sidetone pitch for 30 s.

## Running it outside

The box, the sound card and a Raspberry Pi can sit with the radio, away from the
house. The node's [Raspberry Pi guide](raspberry-pi-setup.md) applies as written,
with this page's configuration. The box needs nothing but its USB cable.
