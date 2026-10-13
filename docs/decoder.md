# The decoder box: Morse to letters on a Pico 2

The **decoder box** is a Raspberry Pi Pico 2 that listens to a radio's headphone
audio and types the Morse it hears, as letters, over USB. Plug it into any computer
and open its serial port in a terminal: each transmission it hears comes up as a
line of text. It needs four parts besides the Pico 2, and nothing from this project
to run.

```
radio PHONES ──audio──► 4 parts ──► Pico 2: CW decoder ══USB══► "CQ CQ DE K8ONS K"
```

hfnode can talk to it too, in framed lines with the decoder's state and the
sidetone monitor's measurements beside the letters
([decoder-protocol.md](decoder-protocol.md)), so that a station can use it in place
of a sound card and the computer's own decoder. hfnode does not use it yet.

It decodes with hfnode's own decoder (`crates/cw`), built without the standard
library and with libm's arithmetic; its tests run that path on a computer
(`crates/decoder-core`).

## It never transmits

The decoder box has no transmit path. It is its own Pico 2, not the keyer box
([keyer.md](keyer.md)): the keyer box's firmware stays as it was audited, and
nothing the decoder does can delay the keyer's watchdog or its fault handling.

Its firmware is still written for the case where it is flashed onto the keyer box
by mistake. The keyer box's key, PTT and tone pins (GP16, GP17, GP18) are driven
low at boot, before anything else starts, driven low again on every pass of its
loop, and driven low first by its fault and panic handlers; no code in the image
drives them high. CI reads the built image's machine code and fails if any store
could ([firmware/pico2-decoder/check_pins.py](../firmware/pico2-decoder/check_pins.py)).

## What you need

- A Raspberry Pi Pico 2 (RP2350). Not the Pico 2 W: its GP23 and GP25 belong to its
  wireless chip, and this firmware drives both.
- A 1 µF capacitor (film or ceramic; an electrolytic with its + side towards the
  Pico), and three 10 kΩ resistors.
- A lead from the radio's PHONES jack, never EXT SP: a speaker output gives watts,
  not a headphone level, and its sleeve may not be ground. On the IC-7300 and most
  radios it is a 3.5 mm stereo plug: tip and sleeve only, ring unconnected and
  insulated. Never a mono plug or a mono adapter, which shorts the right channel
  to ground. Check your radio's manual.
- A USB cable.

## Wiring

Into GP26, the ADC's first input:

| Part | From | To | Why |
|---|---|---|---|
| C1 1 µF | radio phones tip | R3 | Blocks the radio's DC: the ADC cannot read below 0 V |
| R3 10 kΩ | C1 | GP26 (pin 31) | With R1 and R2, divides the audio by 3 and limits the current into the pin |
| R1 10 kΩ | GP26 (pin 31) | ADC_VREF (pin 35) | With R2, holds the pin at mid-scale, 1.65 V, so the audio swings both ways |
| R2 10 kΩ | GP26 (pin 31) | AGND (pin 33) | The other half of the bias |
| wire | radio phones sleeve | AGND (pin 33) | Audio ground |

```
radio tip ──C1 1 µF──R3 10 kΩ──┬── GP26 (pin 31)
                               ├── R1 10 kΩ ── ADC_VREF (pin 35)
                               └── R2 10 kΩ ── AGND (pin 33)
radio sleeve ────────────────────── AGND (pin 33)
```

- Any three equal values from 10 to 47 kΩ work. The high-pass corner is about
  10 Hz, far below a CW tone.
- The bias comes from ADC_VREF, the ADC's own filtered reference (Pico 2 datasheet
  §5.3), not from 3V3, so noise on the supply partly cancels.
- The ADC reads 0 to 3.3 V; through the divide-by-3 that is about 10 V peak to
  peak at the radio, more than a headphone output gives. Past that, R3 keeps the
  current into the pin's protection diodes under 1 mA up to about 30 V peak to
  peak.
- No filter capacitor at the pin: the firmware samples at 32 kHz and filters
  digitally before it decodes at 8 kHz (decoder-protocol.md, "Audio").
- The firmware drives GP23 high, which puts the Pico 2's regulator in the mode with
  less ripple on the ADC's supply (Pico 2 datasheet §5.3). No parts.
- Nothing connects to GP16, GP17 or GP18.

## Using it

1. Plug it into a computer. It shows up as a USB serial port named
   `PICO2-DECODER`: `/dev/cu.usbmodem…` on a Mac, `/dev/ttyACM…` on Linux, a COM
   port on Windows.
2. Open the port in a terminal program at any baud rate: on a Mac, `ls
   /dev/cu.usbmodem*` to find it and `screen /dev/cu.usbmodem11 115200` with that
   name; PuTTY on Windows.
3. Set the radio to CW with its CW pitch at **600 Hz**, turn its volume down, and
   tune to a CW signal.
4. Turn the volume up until letters come up. The Pico 2's LED lights while the box
   hears the tone: it should flash with the signal and stay dark between. If it
   stays lit, turn the volume down.

A different pitch, filter width or sidetone pitch is a framed command; for a 700 Hz
pitch (the checksum is the XOR of every byte before the `*`):

```text
01 CONFIG 700 150 700*3F
```

The box answers `01 OK CONFIG <n>*<cs>` and goes on typing at the new pitch. Its
settings go back to 600, 150 and 600 at power-up.

## The firmware

[firmware/pico2-decoder](../firmware/pico2-decoder) is the board: USB, the ADC and
its DMA ring (rustos's drivers), the pins and the watchdog. Everything it does is
in [crates/decoder-core](../crates/decoder-core), which its tests run against a
board in memory, on synthetic Morse at several speeds and noise levels.

CI builds the UF2 on every push (the `pico2-decoder-firmware` artifact) and
reproducibly: `sh firmware/pico2-decoder/build.sh <commit>` gives the same file.
As with the keyer box, flash only a file whose SHA-256 matches the one the safety
review published for that build.
