# Decoder box protocol

How the decoder box ([decoder.md](decoder.md)) talks over its USB serial port: to
anything as plain text, and to hfnode in framed lines. The rules are in one place in
code, `crates/decoder-core`: the box's firmware (`firmware/pico2-decoder`) runs
them, and its tests run the same code against a board in memory. This is the
decoder box's own protocol, version 1; the keyer box's
([keyer-protocol.md](keyer-protocol.md)) is separate and unchanged.

USB CDC serial; the baud rate is ignored. The box shows up as `PICO2-DECODER`, with
serial number `2` (the keyer box's is `1`).

## Text mode

From power-up, and whenever the link to hfnode is gone, the box types what it hears:

```text
CQ CQ DE K8ONS K
```

- Each character as soon as it is decoded; a space at each gap between words.
- `\r\n` once the tone has been quiet for 2 s after something was typed, so each
  transmission starts a line.
- `*` for a pattern that is not in the Morse table.
- Nothing else: no prompts, no status. A terminal program (`screen`, PuTTY, the
  Arduino serial monitor) at any baud rate shows it.

A character that arrives while the USB queue is full (nothing is reading the port)
is dropped.

## Framed lines

Commands, and everything the box sends in hfnode mode, are framed as the keyer
box's lines are (`crates/keyer-core/src/frame.rs`): printable ASCII ending in `\n`
(a `\r` before it is ignored), at most 96 characters before it:

```text
<id> <body>*<cs>
```

- `<id>`: two uppercase hex digits, chosen by the sender of a command and repeated
  in its reply. Lines the box sends on its own use `00`, which hfnode never uses.
- `<cs>`: two uppercase hex digits, the XOR of every byte before the `*`.

A line that does not decode (bad checksum, too long, not ASCII) is ignored: no
reply, and it does not count as hearing from hfnode. Commands are answered in
either mode.

## Commands

| Command | Reply |
|---|---|
| `HELLO` | `OK HELLO <version> <adc rate> <rate> <slice> <uptime ms> <boot> <build> <name>`, and hfnode mode from then on |
| `STATUS` | `OK STATUS <mode> <hold> <pitch> <bandwidth> <monitor> <n> <lost> <gaps> <errors>` |
| `CONFIG <pitch> <bandwidth> <monitor>` | `OK CONFIG <n>`, a fresh decoder at those settings |
| `NEW` | `OK NEW <n>`, a fresh decoder: what it was in the middle of is dropped |
| `FLUSH` | the characters of the word in progress (`CH`), then `OK FLUSH <n>` |
| `RESET LEVELS` | `OK RESET LEVELS <n>`, the decoder's signal and noise levels forgotten |
| `RESET SPEED` | `OK RESET SPEED <n>`, its speed estimate forgotten |
| `HOLD` | `OK HOLD <n>`: the decoder hears nothing until `RESUME` |
| `RESUME` | `OK RESUME <n>`: the decoder hears again, its levels forgotten |
| `TEXT` | `OK TEXT`, and text mode from then on |

- `HELLO`: protocol version `1`; the ADC's sample rate (32000) and the decoder's
  (8000), in hertz; the samples in one monitor slice (80, 10 ms); time since the
  box started; why it started (`POWER`, `WATCHDOG` or `OTHER`); `<build>`, the
  eight characters of the commit CI built the firmware from, or `-` for a build
  from a working tree; its name, `PICO2-DECODER`.
- `<n>` in every reply and line: the samples at 8000 Hz the box has taken since it
  started. It places each line in the audio: a reply's `<n>` is where in the audio
  the command took effect.
- `STATUS`: `<mode>` `TEXT` or `HFNODE`; `<hold>` 1 while held; the settings, in
  hertz; `<lost>` the lines dropped because the USB queue was full; `<gaps>` the
  times audio was lost (`GAP`); `<errors>` the ADC conversions it flagged as
  failed, each replaced by the last good one.
- `CONFIG`: `<pitch>` the radio's CW pitch, the tone the decoder listens for, 300
  to 1500; `<bandwidth>` the decoder's filter width, 30 to 500; `<monitor>` the
  radio's sidetone pitch, where `SL` measures, 300 to 1500. At power-up 600, 150
  and 600. A value out of range, or a pitch and width the decoder cannot run,
  gets `ERR CONFIG RANGE` and nothing changes.
- `HOLD` and `RESUME` are for while the node transmits: the radio's own sidetone
  is not decoded, and the levels it leaves behind are dropped. `SL` goes on
  through a hold: that is when the monitor needs it.
- A command it knows with the wrong arguments gets `ERR <word> ARGS`, or `ERR
  CONFIG ARGS`; one it does not know gets `ERR <word> UNKNOWN`.

## hfnode mode

After `HELLO`, until `TEXT`, until the USB link goes away (a bus reset, the cable
pulled, the computer suspending or deconfiguring the port), or until no valid line
has arrived for 2 s (so hfnode sends one at least every second), the box sends
only framed lines, each with id `00`:

| Line | When |
|---|---|
| `CH <n> <events>` | as characters are decoded |
| `ST <n> <key> <partial> <idle ms> <wpm>` | every 400 samples (50 ms) |
| `SL <n> <tone>,<total> ×5` | every five monitor slices (50 ms) |
| `GAP <n> <why>` | when audio was lost |

- `CH`: up to 60 events, oldest first: a character, `_` for the gap between two
  words, `*` for a pattern not in the Morse table. `<n>`: the samples taken when
  the last of them was decoded.
- `ST`: the decoder's state after `<n>` samples, as hfnode's node loop reads it:
  `<key>` 1 while it hears the tone; `<partial>` 1 while a word is in progress;
  `<idle ms>` how long the tone has been quiet; `<wpm>` its speed estimate, in
  tenths of a word per minute.
- `SL`: the sidetone monitor's measurements of five consecutive 10 ms slices, the
  first starting after `<n>` samples: for each, the power at the monitor pitch and
  the total power, in tenths of a dB relative to full scale. They are what
  hfnode's monitor measures from sound-card audio, by the same code
  (`cw::level`).
- `GAP`: `<why>` `OVERRUN` (the box fell a whole ring of samples behind the ADC),
  `FIFO` (the ADC's own queue overflowed) or `HALTED` (the ADC's DMA stopped: the
  box stops feeding its watchdog, which restarts it). The samples lost are not
  counted in `<n>`.

A line that does not fit in the USB queue whole is dropped, never cut, and counted
in `STATUS`.

## Audio

What the box decodes and measures is the ADC's 12-bit samples of GP26, taken at
32 kHz, low-pass filtered (flat to 0.1 dB up to 1.5 kHz, at least 70 dB down from
5 kHz) and decimated to 8 kHz, with the bias the wiring puts on the pin taken off.
Full scale (0 dBFS) is the ADC's whole 0-3.3 V range, which the wiring in
[decoder.md](decoder.md) maps to about 10 V peak to peak at the radio's output.
