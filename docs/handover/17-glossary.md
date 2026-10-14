# 17 Glossary

> This page is a glossary, not a subsystem page. It defines, in alphabetical order, the amateur radio, Morse code, RP2350 and project words that the rest of the handover set uses, for engineers who know Rust but have not worked with radios or this chip. Each entry gives a plain definition, then what the word means in this project, with a permalink to where the code defines it, then the page that covers it in depth.

## How to use this page

This page adapts the standard page shape. A glossary has no single subsystem, so "Where it lives" through "Programming against it" are replaced by the glossary itself, grouped by first letter. The page keeps short "Changing it", "Known bugs, limits and flakes", "Work left" and "Sources" sections at the end.

Each entry has three parts:

- **Definition.** What the word means in general, in two to four sentences.
- **In this project.** How the code uses it, with a permalink to the line that defines it. Unless an entry says otherwise, citations are at cw-message-gateway `main` (`44185fa`). Code that exists only on an open pull request is labelled, for example "(PR #31, draft, not on main)".
- **Covered in.** The handover page that explains it in depth.

Some words are not in the code. For those the entry says "Not found in the code at `44185fa`" and names where the word does appear. Project files (the design specs, the safety audit and the bench notes) are not public, so they are named in plain text and cannot be linked.

The `rustos` repository has its own handover set, which explains the RP2350 drivers from the driver side (see [rustos](#rustos)): <https://github.com/robinonsay/rustos/blob/claude/handover-docs-us00cn/docs/handover/README.md>. Its pages are named below in plain text by number and title, for example "rustos page 06 (Multicore)".

Every safety number on this page is a never-loosen rule. The canonical table of every limit and gate, with its code and its audit source, is [13 Safety rules (never loosen)](13-safety-rules.md); where this page and that one ever disagree, the code wins.

Jump to: [0-9](#0-9) · [A](#a) · [B](#b) · [C](#c) · [D](#d) · [E](#e) · [F](#f) · [G](#g) · [H](#h) · [I](#i) · [K](#k) · [L](#l) · [M](#m) · [N](#n) · [O](#o) · [P](#p) · [Q](#q) · [R](#r) · [S](#s) · [T](#t) · [U](#u) · [V](#v) · [W](#w) · [Z](#z)

### Words with more than one meaning

Several words mean different things in different parts of the project. When you read one of them, check which meaning applies.

| Word | Meanings in this project |
|---|---|
| keyer | (1) A radio's internal keyer, which turns text into Morse. (2) The [keyer box](#keyer-box). (3) `keyer_core::keyer::Keyer`, the box's logic. (4) The `[keyer]` config section and the `hfnode keyer` command. |
| run | (1) `hfnode run`, the command that runs the node. (2) One `CW` or `MCW` command to the keyer box, at most 60 s long. See [Run](#run). |
| stage | (1) A bring-up stage: the config key `commissioned`, which uses three different enums. (2) "Stage zero" in `docs/keyer.md`, a meter measurement. (3) A numbered stage in a bench file (project files). See [Stage](#stage). |
| inhibit | (1) The node's transmit inhibit, a latch kept in the file `tx-inhibited`. (2) The IC-7300's own TX Inhibit function. See [Transmit inhibit](#transmit-inhibit). |
| verdict | (1) The safety audit's verdict on an action: GO, GO with steps, NO-GO, not yet, or n/a (project files). (2) `gateway::filter::Verdict`, the reply filter's decision on an inbound message. See [Verdict](#verdict). |
| piece, chunk | A piece is at most 30 characters that one keying command sends (`civ::MAX_CW_CHARS`, `keyer_core::MAX_TEXT`: [`crates/civ/src/lib.rs:24`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L24), [`crates/keyer-core/src/lib.rs:34-36`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L34-L36)). A chunk is a lettered part of a long reply, at most 60 characters by default. Beware: the code's own doc comment calls `protocol::reply::Chunk` "One piece of a long transmission", and `chunk()` splits text with a private `pieces()` helper ([`crates/protocol/src/reply.rs:72-75`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L72-L75), [`crates/protocol/src/reply.rs:93-94`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L93-L94)). On these pages "piece" always means the keying unit. See [Piece](#piece) and [Chunk](#chunk). |
| K | (1) The over prosign "go ahead". (2) A chunk letter, as in `AGN <seq> <code> K K`. See [K](#k-1). |
| watchdog | (1) The keyer box's hardware watchdog, which its control loop must feed every `WATCHDOG_MS = 500` ms ([`crates/keyer-core/src/lib.rs:61-62`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L61-L62)). (2) The host's software keying watchdog, a thread that forces the radio to receive after `station.max_key_seconds` of keying ([`crates/hfnode/src/station.rs:21-24`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L21-L24)). (3) The parked UV-K1 firmware's own hardware watchdog, which resets the radio if it hangs during a run ([`crates/hfnode/src/handheld.rs:27-30`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L27-L30)). The hardware transmit timer of test-plan step 10 is a separate device, not any of these. See [Watchdog](#watchdog). |
| key | (1) A Morse key, or a radio's [key line](#key-line-and-keying). (2) The node's secret key for the code sheet: the file `auth.key_file`, made with `hfnode keygen --out` ([`crates/hfnode/src/config.rs:197-198`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L197-L198), [`crates/hfnode/src/main.rs:31-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L31-L35)). (3) The IC-7300's front-panel and microphone keys, which PR #27 (open, not on main) checks are not set to send a keyer memory (see [Preflight](#preflight)). |
| session | (1) `hfnode::session::Session`, the transaction state machine. (2) In the bench docs, one sitting at the radio. See [Session](#session). |

## 0-9

### 2N3904

**Definition.** A common small NPN transistor. A small current into its base lets a larger current flow from its collector to its emitter, so it works as a switch to ground. Unlike an optocoupler, it does not keep the two sides' grounds apart.

**In this project.** `docs/keyer.md` offers it as the one-transistor key switch for a UV-K1 running NR7Y's CW firmware. GP16 drives the base through 4.7-10 kΩ, the collector goes to the radio's PTT line and the emitter to ground. The 4.7 kΩ pull-down goes on the pin's side of the base resistor, never on the base: [`docs/keyer.md:1039-1057`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1039-L1057). Not found in the code at `44185fa`: it is a wiring choice, and the firmware is the same for either switch. The Quansheng bench uses this switch, the audit's "option C" (`audit/req-handheld-nr7y-cw-ruling.md` §2, project files): GP16 through 10 kΩ to the base, and the 4.7 kΩ pull-down from GP16 to ground at the pin.

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

### 47 CFR 97.119

See [Station ID](#station-id-and-47-cfr-97119). The other Part 97 rules the code cites are under [Part 97](#part-97).

## A

### ADC

**Definition.** An analog-to-digital converter turns the voltage on a pin into a number. The RP2350's ADC is 12-bit, about 9.2 effective bits. On the Pico 2's RP2350A it reads GPIO26 to GPIO29: [`firmware/pico2/src/adc.rs:31-37`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/adc.rs#L31-L37) (rustos master).

**In this project.** The keyer box on `main` does not use the ADC. The [decoder box](#decoder-box) (PR #30, not on main) and the [one-Pico image](#one-pico-image) (PR #31, draft, not on main) sample the radio's audio on [GP26](#gp26) with it. DMA streams the samples into a ring, which the decoder box's one core reads (PR #30: [`firmware/pico2-decoder/src/main.rs:64`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L64), [`firmware/pico2-decoder/src/main.rs:170-178`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L170-L178)) and core 1 reads in the one-Pico image (PR #31: [`firmware/pico2-keyer/src/core1.rs:10-15`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L10-L15)). Neither has run on hardware yet.

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

### ADC_VREF

**Definition.** The Pico 2 pin (pin 35) that carries the ADC's own filtered reference voltage. The ADC measures every input against it, so a clean reference gives clean readings.

**In this project.** The decoder box's audio input (PR #30, not on main) holds GP26 at mid-scale, 1.65 V, with two equal resistors: one to ADC_VREF and one to AGND. The radio's audio then swings both ways around that point. The bias comes from ADC_VREF rather than from 3V3, so supply noise partly cancels: [`docs/decoder.md:52-58`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/docs/decoder.md#L52-L58), [`docs/decoder.md:69-70`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/docs/decoder.md#L69-L70).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### AGN

**Definition.** A Morse operating abbreviation for "again": please repeat. Operators send it when they missed part of a transmission.

**In this project.** `AGN <seq> <code> [letter]` asks the node to repeat its last transmission, or only one lettered chunk of it. `AGN <seq> <code> K K` asks for chunk `K`, while a lone `K` is the over: [`crates/protocol/src/lib.rs:223-236`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L223-L236). An `AGN` spends a fresh code-sheet line, so someone who only overheard the exchange cannot make the node transmit again: [`crates/hfnode/src/session.rs:19-23`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L19-L23).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### AGND

**Definition.** The Pico 2's analog ground pin (pin 33): the ground that analog inputs to the ADC are measured from. It is a pin of its own, separate from the board's GND pins (such as pin 23), and the decoder box's audio wiring returns to it.

**In this project.** In the decoder box's wiring (PR #30, not on main), the radio's audio sleeve and the lower bias resistor both go to AGND: [`docs/decoder.md:52-58`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/docs/decoder.md#L52-L58).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### Antenna tuner

**Definition.** A circuit that matches the antenna's impedance to the 50 Ω the transmitter expects, so that the power goes into the antenna and does not come back into the radio. An automatic tuner finds the match itself. To do that it transmits a carrier for a few seconds, so a tune is a transmission.

**In this project.** The IC-7300 has an internal tuner that CI-V command `1C 01` controls, and `1C 01 02` starts a tuning cycle: [`crates/civ/src/ic7300.rs:141-144`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L141-L144). The `Rig` trait has `start_tune`, `tuner_busy` and `tuner_matched`. A tuner that cannot match bypasses itself without reporting an error, which is why `tuner_matched` exists: [`crates/civ/src/lib.rs:75-85`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L75-L85). The node tunes when it starts listening and before a reply once the last tune is older than `schedule.retune_minutes`: [`crates/hfnode/src/station.rs:42-49`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L42-L49). Rigs without a tuner (the keyer box and the handheld) answer `has_tuner() == false`, and the node never asks them to tune: [`crates/civ/src/lib.rs:110-114`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L110-L114).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md), [05 Node loop and station safety](05-node-loop-station-safety.md).

### AR and SK

See [K](#k-1). `AR` ("end of message") and `SK` ("end of contact") are overs the parser accepts. Run together, `AR` decodes as `+`.

### Audit

**Definition.** In this project, "the audit" is a separate, independent safety review run by its own audit thread (not by the threads that write the code). It covers the code, the radio manuals and the parts' datasheets, and it grades whether an action is safe before Robin (the project owner) takes it.

**In this project.** The audit is not code. Its rubric, grades and rulings live in the project files (`audit/rubric.md`, `audit/final-grades.md`, `audit/report.md` and the `audit/req-*.md` rulings). Every grade is one of PASS, BENCH, RISK, FAIL or N/A, and each action gets a [verdict](#verdict). The audit gates every flash and every keyed test, and it publishes the SHA-256 of each firmware file it clears: [`docs/keyer.md:129-133`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L129-L133). The code refers to its rulings by name, for example the handheld constraint C3 behind the PTT limit: [`crates/keyer-core/src/lib.rs:53-57`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L53-L57). No change may go around the audit.

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### Automatic control

**Definition.** A US amateur-radio term for a station that transmits without its control operator at the control point. The audit's rubric cites it as 47 CFR 97.109(d) (not checked against the eCFR here). Whether a given use is allowed under it is a legal question for the station's licensee, not a technical one.

**In this project.** Running the node unattended would be automatic control. The audit grades hardware only and leaves this question to Robin (the project owner) as licensee (`audit/rubric.md` §G and §H, project files). `docs/keyer.md` sends the operator to "the control operator rules for automatic CW" before leaving the node on its own: [`docs/keyer.md:1526-1529`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1526-L1529). Not found in the code at `44185fa`. See [Part 97](#part-97) and [H1](#h1).

**Covered in.** [16 Work left and known bugs](16-work-left-known-bugs.md) (WL15).

## B

### Bench stage

**Definition.** One numbered step of a hardware bench test. Hardware stages are done by Robin (the project owner) at the radio, only on her go, and each result is recorded for the audit.

**In this project.** The bench notes are project files. `bench/quansheng.md` numbers its stages 0, 0b, 1, H, 2 to 5, 5b and 6 to 8; its stages 0 and 0b check the code and the flash file, and nothing in them touches a radio. `bench/ic7300.md` numbers its stages 0 to 13 (stage 0 checks the code on the host) and maps them to the steps of [`docs/hardware-test-plan.md`](../hardware-test-plan.md). These numbers are not the config's bring-up [stages](#stage), and bench stage 0 is not `docs/keyer.md`'s "stage zero". Not found in the code at `44185fa`.

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### BOOTSEL

**Definition.** A button on the Pico 2. Held while the board is plugged into USB, it makes the RP2350's boot ROM start in its USB mass-storage mode instead of running the program in flash. The board then shows up as a drive called `RP2350`, and a [UF2](#uf2) file copied onto it is written to flash.

**In this project.** Flashing the keyer box is: check the file's SHA-256, hold BOOTSEL while plugging in, copy `pico2-keyer.uf2`, then unplug and plug in again, because the first boot after flashing reports `OTHER`: [`docs/keyer.md:129-140`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L129-L140). A Pico held in BOOTSEL, resetting or unpowered would not leave a radio's key line open either if a bare GPIO were wired straight to it, which is one reason that wiring is graded NO-GO: [`docs/keyer.md:966-977`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L966-L977).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Break-in

**Definition.** A CW radio setting that switches the radio to transmit when the key closes and back to receive after the key has been up for a set time, the break-in delay. With "semi" break-in the radio stays on transmit through short gaps and returns to receive only after the delay. With "full" break-in it returns to receive between elements.

**In this project.** On the IC-7300 the node turns semi break-in on with CI-V `16 47 01`, because text sent with command `17` keys only when break-in is on: [`crates/civ/src/ic7300.rs:14-16`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L14-L16), [`crates/civ/src/ic7300.rs:130-134`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L130-L134). It sets the delay to `BREAK_IN_DELAY_DOTS = 10.0` dots, 3 more than a word gap, so the radio does not drop to receive inside a piece: [`crates/hfnode/src/station.rs:181-183`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L181-L183). **Never loosen** (rule 4.19 in [13 Safety rules (never loosen)](13-safety-rules.md)). Turning break-in off is one of the things the node does when it inhibits transmitting: [`crates/civ/src/lib.rs:177-185`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L177-L185). A radio on the keyer box must have break-in turned on at the radio itself: [`docs/keyer.md:179-180`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L179-L180).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md), [09 Keyer box: host side](09-keyer-host-side.md).

### Build id

**Definition.** A short string that names the exact source a firmware image was built from. A device that reports it lets the host check which image is running before trusting it.

**In this project.** The keyer firmware takes it from the `KEYER_BUILD_ID` environment variable at build time (CI sets it to the short commit) and reports it in `HELLO`. A build from a working tree reports `-`: [`firmware/pico2-keyer/src/main.rs:95-101`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L95-L101). With `[keyer] firmware_build` set, `hfnode` refuses a box that reports any other build: [`crates/hfnode/src/config.rs:121-124`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L121-L124).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [14 Audit, bench and flashing](14-audit-bench-flashing.md).

## C

### Callsign

**Definition.** A station's licence identifier, issued by the national regulator. It must be sent to identify the station (see [Station ID](#station-id-and-47-cfr-97119)). The handover pages use placeholders such as `N0CALL`.

**In this project.** `station.node_call` is the node's own callsign, sent as `DE <call>`. `station.field_calls` lists the field callsigns allowed to open transactions: [`crates/hfnode/src/config.rs:46-49`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L46-L49).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md), [15 Config, CLI and deployment](15-config-cli-deployment.md).

### CAT

**Definition.** "Computer Aided Transceiver": the general name for controlling a radio from a computer over a serial link. Each maker has its own command set. ICOM's is [CI-V](#ci-v).

**In this project.** Not found in the code at `44185fa`. The code and the repo docs say CI-V throughout. The design spec (project files) uses "CAT" once, for the IC-7300's USB control link.

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

### CDC-ACM

**Definition.** The USB device class for a virtual serial port ("Communications Device Class, Abstract Control Model"). A computer shows such a device as a serial port (`/dev/ttyACM0`, `/dev/cu.usbmodem...`, or `COMn`) with no driver to install.

**In this project.** rustos's polled USB CDC-ACM driver is how the keyer box talks to `hfnode`: [`firmware/pico2/src/usb/mod.rs:1-2`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/usb/mod.rs#L1-L2) (rustos master). The box enumerates with vendor ID `0x2e8a`, product ID `0x0009` and product name `PICO2-KEYER`: [`firmware/pico2-keyer/src/main.rs:86-93`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L86-L93), [`crates/keyer-core/src/lib.rs:29`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L29). The text protocol on that port is in [`docs/keyer-protocol.md`](../keyer-protocol.md).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

### Chunk

**Definition.** In this project, one lettered part of a long reply. The field operator can ask for one chunk again instead of the whole reply.

**In this project.** `protocol::reply::Chunk` renders as `<text> = <letter>`, where `=` is the Morse break sign `BT`. There are at most 26 chunks, one per letter A to Z: [`crates/protocol/src/reply.rs:72-88`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L72-L88). `station.chunk_chars` (default 60, and it must be 10-60) sets the longest chunk, and `station.chunk_pause_ms` (default 2000) the pause between chunks: [`crates/hfnode/src/config.rs:77-82`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L77-L82), defaults [`crates/hfnode/src/config.rs:752-757`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L752-L757), range [`crates/hfnode/src/config.rs:950-951`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L950-L951). A chunk is sent as one or more [pieces](#piece).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### CI-V

**Definition.** ICOM's serial command protocol for its radios. Each frame is `FE FE <to> <from> <command> [<sub-command>] [<data>...] FD`, and the radio answers `FB` (OK) or `FA` (NG, "not good"). The IC-7300 carries CI-V over a virtual serial port on its USB cable.

**In this project.** The frame format and the byte constants are in [`crates/civ/src/frame.rs:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/frame.rs#L1-L12). The IC-7300's default address is `0x94` ([`crates/civ/src/ic7300.rs:79`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L79)) and the controller's is `0xE0` ([`crates/civ/src/frame.rs:11-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/frame.rs#L11-L12)). `station.civ_address` sets the address: [`crates/hfnode/src/config.rs:56-58`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L56-L58). Every command the driver sends cites Section 19 of the IC-7300 manual: [`crates/civ/src/ic7300.rs:1-13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L1-L13). The driver never sends `1C 00 01` (force transmit on): [`crates/civ/src/ic7300.rs:22-28`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L22-L28).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

### Code sheet

**Definition.** In this project, the printed table of one-time codes the field operator carries. Each line is a [sequence number](#sequence-number) and its 8-letter code, printed in two groups of four.

**In this project.** `hfnode codes --config C [--from N] [--count 100]` prints it, by default starting one after `last_seq`: [`crates/hfnode/src/main.rs:36-45`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L36-L45), [`crates/hfnode/src/main.rs:552-560`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L552-L560). The rules printed with it are "Use each line once, in order; skipping lines is fine." and "Two lines per message (open, OK), and one more for each NO or AGN.": [`crates/hfnode/src/main.rs:625-629`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L625-L629). Each code is an HMAC-SHA256 of the sequence number, made with the node's secret key, which is the file `auth.key_file`, made with `hfnode keygen --out` ([`crates/hfnode/src/config.rs:197-198`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L197-L198), [`crates/hfnode/src/main.rs:31-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L31-L35)): [`crates/auth/src/lib.rs:105-110`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L105-L110). `format_for_print` splits it into two groups of four: [`crates/auth/src/lib.rs:213-217`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L213-L217). The codes authenticate the operator; they do not encrypt anything: [`crates/auth/src/lib.rs:13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L13).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Core 0 and core 1

**Definition.** The RP2350 has two Arm Cortex-M33 processor cores. After reset, core 0 runs the program and core 1 waits in the boot ROM until core 0 starts it with an entry point and a stack: [`firmware/pico2/src/multicore.rs:26-37`](https://github.com/robinonsay/rustos/blob/410dffa091e247e6e7546c7f2ff465f0af499d5a/firmware/pico2/src/multicore.rs#L26-L37) (rustos PR #6, open, not on master, untested on hardware).

**In this project.** On `main` the keyer box uses core 0 only. In the [one-Pico image](#one-pico-image) (PR #31, draft, not on main, untested on hardware), core 0 runs the keyer and is the only core that keys, while core 1 runs the CW decoder on the radio's audio. Core 1 can reach none of core 0's pins or memory, and core 0 trips the box if core 1 faults or stops: [`firmware/pico2-keyer/src/main.rs:42-50`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/main.rs#L42-L50), [`firmware/pico2-keyer/src/core1.rs:1-8`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L1-L8). Core 1 must move its [heartbeat](#heartbeat) at least every `CORE1_MS = 100` ms, or the box trips `CORE1`: [`crates/keyer-core/src/lib.rs:104-110`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/crates/keyer-core/src/lib.rs#L104-L110). **Never loosen** (rule 1.19 in [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 06 (Multicore).

### CP0

**Definition.** Coprocessor 0 on the RP2350 is the GPIO coprocessor. Each core can use it to set and clear the GPIO output bits with coprocessor instructions instead of memory stores.

**In this project.** Because CP0 drives pins without a store, a memory protection unit cannot stop it. So core 1 in the one-Pico image (PR #31, draft, not on main, untested on hardware) writes `CPACR` to allow only the FPU (coprocessors 10 and 11) and turn every other coprocessor off. Any CP0 instruction on core 1 then faults: [`firmware/pico2-keyer/src/core1.rs:289-308`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L289-L308).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### CW

**Definition.** "Continuous wave": Morse code sent by switching a radio's carrier on and off. Key down means the transmitter is on, key up means it is off. CW gets through at low power and in noise where speech would not.

**In this project.** Everything on the air is CW. The `cw` crate holds the decoder and a synthesizer for tests. The decoder builds without std, so the same decoder can run on a microcontroller; the Pico 2 decoder is in PR #30 and PR #31, not on main: [`crates/cw/src/lib.rs:1-10`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/lib.rs#L1-L10). The keyer box's own Morse table and timing are in `keyer-core`: [`crates/keyer-core/src/morse.rs:1-6`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/morse.rs#L1-L6). `CW <wpm> <text>` is the keyer box command that keys text: [`crates/keyer-core/src/keyer.rs:20-27`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L20-L27).

**Covered in.** [04 CW decoder and audio](04-cw-decoder-audio.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

## D

### DE

**Definition.** Morse for "from", sent before a callsign, as in `CQ DE N0CALL`. The callsign after it identifies the station that is transmitting.

**In this project.** Every transmission the node builds ends with `DE <node_call> K`: [`crates/protocol/src/reply.rs:68`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L68). The parser counts a `DE` from the field as noise in two places: when it finds where a message starts (an `OK`, `NO` or `AGN` counts as the start only if nothing but noise comes before it), and when it reads a `WX` grid: [`crates/protocol/src/lib.rs:200-207`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L200-L207), [`crates/protocol/src/lib.rs:301`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L301), [`crates/protocol/src/lib.rs:308-312`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L308-L312). See [Station ID](#station-id-and-47-cfr-97119).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md), [05 Node loop and station safety](05-node-loop-station-safety.md).

### Decoder box

**Definition.** In this project, a second Raspberry Pi Pico 2 that listens to a radio's headphone audio on [GP26](#gp26), decodes the Morse in it, and sends the letters to the computer over USB serial. It never transmits.

**In this project.** It exists only on PR #30 (open, not on main, untested on hardware): [`firmware/pico2-decoder/src/main.rs:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L1-L12). Its image drives the keyer box's key, PTT and tone pins (GP16 to GP18) low before anything else starts, in case it is ever flashed onto the keyer box: [`firmware/pico2-decoder/src/main.rs:10-14`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L10-L14). On 2026-10-11 Robin (the project owner) chose to run the same decoder on core 1 of the keyer box as well, the [one-Pico image](#one-pico-image); the standalone decoder box stays as the first hardware test of the decoder (design note `design/spec-pico-decoder.md`, project files). See also [Flash file](#flash-file).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### Delta TX (∂TX)

**Definition.** An IC-7300 function that shifts the transmit frequency, by up to ±9.99 kHz, without shifting the receive frequency (IC-7300 manual p. 4-11). With it on, the radio would transmit somewhere other than where it listens.

**In this project.** CI-V `21 02` reads it: [`crates/civ/src/ic7300.rs:164-165`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L164-L165). The preflight fails if it is on ("would transmit off the dial frequency; turn ∂TX off"): [`crates/civ/src/preflight.rs:305-311`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/preflight.rs#L305-L311). The station checks it again before every transmission: [`crates/hfnode/src/station.rs:36-39`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L36-L39). **Never loosen.**

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

### DMA

**Definition.** Direct memory access: a hardware engine that copies data between a peripheral and memory with no processor involved. The RP2350 has 16 DMA channels: [`firmware/pico2/src/dma.rs:16-24`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/dma.rs#L16-L24) (rustos master).

**In this project.** The keyer box on `main` does not use DMA. In the one-Pico image (PR #31, draft, not on main, untested on hardware), DMA channel 0 streams ADC samples into a 16 kB ring for the whole boot. Core 1 only reads the channel's position and the ring: [`firmware/pico2-keyer/src/core1.rs:67-72`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L67-L72). The [decoder box](#decoder-box) (PR #30, not on main, untested on hardware) does the same on its one core: [`firmware/pico2-decoder/src/main.rs:64`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L64), [`firmware/pico2-decoder/src/main.rs:170-178`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/firmware/pico2-decoder/src/main.rs#L170-L178). See also [SECCFG](#seccfg).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

### Dummy load

**Definition.** A 50 Ω resistor that takes a transmitter's full power and turns it into heat without radiating it. It lets a transmitter be tested without putting a signal on the air.

**In this project.** The bring-up stage comments still say a dummy load: stage `setup` allows `radio tune` into one, and stage `tune` means the tuner cycled into it and the radio came back to receive ([`crates/hfnode/src/commissioning.rs:34-39`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L34-L39)), as does the keyer box's parts list ([`docs/keyer.md:38`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L38)). The audit's on-air ruling (`audit/req-on-air-bench-ruling.md` §4, project files) replaced the dummy load on Robin's bench: IC-7300 test-plan steps 1 to 7 run on the tuned antenna at 5 W with her at the radio, and step 8 (the high-SWR lockout) still needs a 100 Ω or 150 Ω load, never above 10 W. The NR7Y bring-up on a UV-K1 runs into a 2 m dummy load or on the radio's own antenna, never coax: [`docs/keyer.md:1304-1307`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1304-L1307).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### Duty budget

**Definition.** A limit on how much of the time a transmitter may be keyed, so that its power amplifier does not overheat. CW at high power for long stretches is what heats a transmitter.

**In this project.** There are four, three on the live routes and one on the parked handheld route, and none may be loosened.

- **The keyer box's own budget** (`DUTY_BUDGET_MS = 60_000`). Every millisecond the key or the PTT is down spends one, and every millisecond it is not earns one back, up to 60 000. A run is refused while the budget holds less than the time it would key. That keeps the transmitter keyed at most 55% of any 10 minutes, and 50% in the long run. The budget starts full only after a power-up: [`crates/keyer-core/src/lib.rs:70-78`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L70-L78), [`crates/keyer-core/src/keyer.rs:689-700`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L689-L700). **Never loosen.**
- **The host's limit for the keyer box.** `[keyer] max_duty_percent` (default 50) of `duty_window_secs` (default 600), and neither may be looser than 50% of 600 s: [`crates/hfnode/src/keyer.rs:117-119`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L117-L119). **Never loosen.**
- **The IC-7300's limit, set by power.** Half of a 10-minute window up to 50 W, then less, down to a quarter at 100 W: [`crates/hfnode/src/station.rs:207-216`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L207-L216). **Never loosen.**
- **The parked handheld route's limit** (`station.rig = "handheld"`). `[handheld] max_duty_percent` (default 50, must be 10-50) of `duty_window_secs` (default 300, must be 60-3600), and their product may be no more than `MAX_DUTY_BUDGET = 150` s, which keeps it within the firmware's own budget of time keyed: [`crates/hfnode/src/config.rs:153-159`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L153-L159), [`crates/hfnode/src/config.rs:784-789`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L784-L789), [`crates/hfnode/src/handheld.rs:77-81`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L77-L81), [`crates/hfnode/src/handheld.rs:218-234`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L218-L234). **Never loosen.**

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [09 Keyer box: host side](09-keyer-host-side.md), [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md), [13 Safety rules (never loosen)](13-safety-rules.md).

## E

### Element

**Definition.** One dot or dash of a Morse character. A dot is one [Morse unit](#morse-unit) long and a dash three.

**In this project.** The keyer box times every element itself. The [key-down limit](#key-down-limit), `KEY_DOWN_MS = 1_000` ms, is longer than the longest element there is, a dash at 5 wpm (720 ms); a key-down past it opens the key and trips the box until it is power-cycled: [`crates/keyer-core/src/lib.rs:45-48`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L45-L48). **Never loosen** (rule 1.1 in [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Erratum RP2350-E9

**Definition.** A published RP2350 chip bug. A pad that nothing drives, with its input buffer enabled, sources a small current and can sit at about 2 V instead of 0 V, and the chip's internal pull-down is too weak to hold it down. Raspberry Pi's fix is an external pull-down of 8.2 kΩ or less.

**In this project.** About 2 V on the key pin is enough to light the optocoupler and key the radio. So the keyer box requires a 4.7 kΩ resistor from GP16 (the key) to ground, and another from GP17 (the PTT output) to ground whenever the PTT output is wired, and must never rely on the internal pull-down: [`docs/keyer.md:57-65`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L57-L65), [`docs/keyer.md:576-578`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L576-L578), [`docs/keyer.md:591-593`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L591-L593). The firmware counts on them to hold both pins low until it drives them: [`firmware/pico2-keyer/src/main.rs:14-18`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L14-L18). **Never loosen: the 4.7 kΩ pull-downs are required** (rule 2.3 in [13 Safety rules (never loosen)](13-safety-rules.md)). rustos's ADC driver notes the same erratum for analog pins, where a pad between the logic levels settles near 2.2 V: [`firmware/pico2/src/adc.rs:90-97`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/adc.rs#L90-L97) (rustos master).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [13 Safety rules (never loosen)](13-safety-rules.md).

## F

### Field operator

**Definition.** In this project, the person away from home with a low-power CW radio, a key and the printed code sheet. They send requests to the node and read its replies by ear.

**In this project.** The field operator sends `TX` (send a text or email), `RX` (read replies) and `WX` (weather) requests: [`README.md:3-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/README.md#L3-L12). The self-test plays a scripted field operator against the node: [`crates/hfnode/src/selftest.rs:9-16`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest.rs#L9-L16).

**Covered in.** [01 System overview](01-system-overview.md), [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Flash file

**Definition.** The exact firmware file, identified by its SHA-256, that may be written to a device. Naming the file by its hash, not by its name or its commit, means the bytes flashed are the bytes that were checked.

**In this project.** For the keyer box it is `pico2-keyer.uf2`, built by CI and uploaded with a `.sha256` beside it: [`.github/workflows/ci.yml:211-216`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L211-L216). The audit's `audit/final-grades.md` (project files) names the cleared file with its CI run, size, build id and SHA-256. A file whose SHA-256 differs from the audit's is never flashed. **Never loosen** (rule 7.2 in [13 Safety rules (never loosen)](13-safety-rules.md)).

The decoder box and the one-Pico image are built into files of their own:

- The decoder box image `pico2-decoder.uf2` (PR #30, not on main) is cleared GO WITH STEPS at `3efaf84`, build `3efaf842`, for a plain Pico 2 with nothing plugged into a radio. Flashed onto the keyer box it replaces the keyer, so reflash the keyer's file and replug before any keying (`audit/report.md`, "PR #30 re-checked", project files).
- The one-Pico image (PR #31, draft, not on main) is cleared to flash only after PR #31 merges, with `hfnode` rebuilt from main after that merge (`audit/final-grades.md`).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### Forced receive

**Definition.** In this project, the station's stop path: stop the radio's keyer, switch the radio to receive, then read the radio's status until receive is confirmed. If receive cannot be confirmed, transmitting is inhibited.

**In this project.** It runs after any failure and on shutdown: [`crates/hfnode/src/station.rs:11-20`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L11-L20). `force_receive` does the stop, and `force_receive_or_latch` also latches the [transmit inhibit](#transmit-inhibit) when receive is not confirmed: [`crates/hfnode/src/station.rs:387`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L387), [`crates/hfnode/src/station.rs:474-475`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L474-L475). The host's software [watchdog](#watchdog) forces receive the same way. **Never loosen.**

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

## G

### Goertzel

**Definition.** The Goertzel algorithm measures the strength of one frequency in a block of samples with a short recursive filter. It is cheaper than a full Fourier transform when only one tone matters, such as a CW pitch.

**In this project.** The keyer box route's [sidetone monitor](#sidetone-monitor) measures each 10 ms slice at the sidetone pitch with it: [`crates/hfnode/src/keyer/monitor.rs:1147-1148`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/monitor.rs#L1147-L1148). PR #30 (not on main) moves the measurement into the `cw` crate as `cw::level`, with `goertzel_coeff`: [`crates/cw/src/level.rs:9-10`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/crates/cw/src/level.rs#L9-L10).

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md), [04 CW decoder and audio](04-cw-decoder-audio.md), [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### GP26

**Definition.** GPIO 26 of the RP2350, Pico 2 pin 31. It is the first of the pins the ADC can read (ADC input 0).

**In this project.** The keyer box on `main` does not use it. In the [decoder box](#decoder-box) (PR #30, not on main) and the [one-Pico image](#one-pico-image) (PR #31, draft, not on main) it takes the radio's headphone audio, through a capacitor and a resistor, biased to mid-scale by two 10 kΩ resistors to ADC_VREF and AGND: [`docs/decoder.md:52-58`](https://github.com/robinonsay/cw-message-gateway/blob/3efaf842e1aab75354e5e19d0384a703d4d786be/docs/decoder.md#L52-L58), [`firmware/pico2-keyer/src/core1.rs:10-15`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L10-L15). Untested on hardware.

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### GPIO

**Definition.** General-purpose input/output: a chip pin that software can drive high or low, or read. On the RP2350 a GPIO involves four hardware blocks: `RESETS`, `IO_BANK0` (which function drives the pin), `PADS_BANK0` (how the [pad](#pad) behaves electrically) and `SIO` (the output and input bits): [`firmware/pico2/src/gpio/gpio.rs:1-20`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/gpio/gpio.rs#L1-L20) (rustos master).

**In this project.** The keyer box uses GP16 for the key (Pico 2 pin 21), GP17 for the PTT, GP18 for the tone, GP19 to read the PTT line and GP25 for the LED: [`firmware/pico2-keyer/src/main.rs:65-77`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L65-L77). The key pin LOW means the key is open. A bare GPIO must never drive a radio that keys on a low line. The audit grades that NO-GO, because every safe state of the box would then key the radio: [`docs/keyer.md:966-977`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L966-L977). **Never loosen.**

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

## H

### H1

**Definition.** In this project, criterion H1 "Bench proven" of the safety audit's rubric (`audit/rubric.md` §H, project files). It is met only when every bench item for keying has been measured on Robin's own hardware and passed, with the result written down; code alone never satisfies it.

**In this project.** Unattended use is graded only from it: no route is cleared for unattended use until her bench results are in (`audit/final-grades.md`, project files). The bench files record each stage's result for it. Not found in the code at `44185fa`. See [Automatic control](#automatic-control) for the separate legal question.

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [16 Work left and known bugs](16-work-left-known-bugs.md).

### Hangtest, stucktest and linktest

**Definition.** In this project, the bring-up tests that make the box (or the handheld firmware) fail on purpose, with the operator at the radio, to show that its own limits end the transmission without the computer's help.

**In this project.** `hfnode keyer hangtest` hangs the box's control loop mid-run, and its watchdog must reset it and open the key within 0.5 s. `hfnode keyer stucktest` makes the box hold its key down, and its 1 s limit must open it and trip the box. `hfnode keyer linktest` keys a long message and then stops talking to the box, and its link timeout must open the key: [`crates/hfnode/src/main.rs:312-321`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L312-L321). The box takes `TEST HANG` and `TEST STUCK` only within `ARM_MS = 2_000` ms of `TEST ARM`: [`crates/keyer-core/src/lib.rs:79-80`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L79-L80). The parked handheld route has `hfnode handheld linktest` and `hangtest`: [`crates/hfnode/src/main.rs:332-339`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L332-L339). Stage `keying` allows the hang and stuck tests, and once both have shown the box opening the key on its own the keyer box's [stage](#stage) may be set to `done`: [`crates/hfnode/src/keyer.rs:133-137`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L133-L137).

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md), [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### Hash

**Definition.** In this project, the SHA-256 digest of a firmware file. Two files with the same SHA-256 are, for any practical purpose, the same bytes.

**In this project.** CI writes the UF2's SHA-256 into the run's summary and uploads it as `pico2-keyer.uf2.sha256`: [`.github/workflows/ci.yml:204-216`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L204-L216). Those only show the download is intact. The check that counts is against the SHA-256 the audit published: [`docs/keyer.md:129-133`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L129-L133). **Never loosen** (see [Flash file](#flash-file)). Any firmware change, a rustos pin move included, is a new build id, a new hash and a new audit (rule 7.3 in [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [02 Codebase, build and CI](02-codebase-build-ci.md).

### Heartbeat

**Definition.** A counter or flag that a running task updates regularly so that another part of the system can see it is still alive. A heartbeat that stops moving means the task has hung or died.

**In this project.** Only in the one-Pico image (PR #31, draft, not on main, untested on hardware): core 1 must move its heartbeat at least every `CORE1_MS = 100` ms, or core 0 trips the box (`CORE1`), opens every output and stops feeding its watchdog: [`crates/keyer-core/src/lib.rs:104-110`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/crates/keyer-core/src/lib.rs#L104-L110). **Never loosen** (rule 1.19 in [13 Safety rules (never loosen)](13-safety-rules.md)). See [Trip](#trip).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### HF

**Definition.** High frequency: radio from 3 to 30 MHz, the shortwave bands. HF signals can travel far by reflecting off the ionosphere, so a home station can reach a field station hundreds of kilometres away. Amateur bands are named by wavelength: the band table runs "160 m to 70 cm", and 2 m is 144-148 MHz, a VHF band above HF: [`crates/hfnode/src/keyer.rs:37-55`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L37-L55).

**In this project.** The node listens on one fixed HF frequency: [`README.md:3-6`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/README.md#L3-L6). With the keyer box, `hfnode` checks that `station.frequency_hz` is inside a US amateur band from its table, whose band edges are "from memory, not checked against the eCFR": [`crates/hfnode/src/keyer.rs:37-55`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L37-L55).

**Covered in.** [01 System overview](01-system-overview.md).

### hfnode

**Definition.** The project's one host binary: the node and all of its tools. The same binary runs the node, drives the bench tests and runs the self-test, on Linux, macOS or Windows.

**In this project.** The package and binary are named `hfnode`: [`crates/hfnode/Cargo.toml:2`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/Cargo.toml#L2), [`crates/hfnode/src/main.rs:1`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L1). It has 17 subcommands: `keygen`, `codes`, `sim`, `decode`, `synth`, `listen`, `record`, `devices`, `radio`, `keyer`, `handheld`, `run`, `storm`, `messages`, `filter`, `selftest` and `testvectors`: [`crates/hfnode/src/main.rs:29-221`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L29-L221). `hfnode run --config C` runs the node: [`crates/hfnode/src/main.rs:116-120`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L116-L120).

**Covered in.** [15 Config, CLI and deployment](15-config-cli-deployment.md), [02 Codebase, build and CI](02-codebase-build-ci.md).

## I

### IC-7300

**Definition.** The ICOM IC-7300, a 100 W HF transceiver. It has an internal antenna tuner, a built-in CW keyer, SWR and power meters, and one USB port that carries both CI-V control and audio.

**In this project.** It is the node's default radio (`station.rig = "ic7300"`): [`crates/hfnode/src/config.rs:85-93`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L85-L93). The driver is `civ::ic7300`: [`crates/civ/src/ic7300.rs:1-13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L1-L13). The design calls for 30-50 W: [`crates/hfnode/src/config.rs:64-66`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L64-L66). Until bring-up stage `keying` has passed, power is capped at `BENCH_MAX_WATTS = 10`: [`crates/hfnode/src/commissioning.rs:60-61`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L60-L61). **Never loosen** (rule 4.6 in [13 Safety rules (never loosen)](13-safety-rules.md)). `hfnode` has never yet been connected to a real IC-7300 (bench notes, project files).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

### ISO latch

**Definition.** On the RP2350, each pad has an isolation latch (`ISO`). While it is set, the pad keeps its last output enable, output level and pulls, whatever its registers say: [`firmware/pico2/src/watchdog.rs:162-164`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/watchdog.rs#L162-L164) (rustos master). Pads come out of reset isolated, and software must clear `ISO` before the pad follows its registers: [`firmware/pico2/src/gpio/gpio.rs:347-361`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/gpio/gpio.rs#L347-L361) (rustos master).

**In this project.** rustos's output constructor writes the output low and sets up the pad first, then clears `ISO` last, so the pin goes from its latched state straight to driven-low: [`firmware/pico2/src/gpio/gpio.rs:418-461`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/gpio/gpio.rs#L418-L461). The keyer firmware configures the key and the PTT as low outputs first thing at boot: [`firmware/pico2-keyer/src/main.rs:177-182`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L177-L182). Whether a pin driven high stays latched across a watchdog reset is read from the datasheet and has not been checked on hardware: [`firmware/pico2/src/watchdog.rs:151-183`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/watchdog.rs#L151-L183).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos pages 05 (Drivers: GPIO, USB, PWM, ADC, DMA) and, for the watchdog-reset case, 04 (Drivers: board, clocks, time, watchdog).

## K

### K

**Definition.** A Morse procedure sign (prosign) that means "go ahead": the sender has finished and hands the frequency over. Others in the same family are `KN` ("go ahead, only the station I am working"), `AR` ("end of message") and `SK` ("end of contact"). A prosign that ends one side's turn is called an over.

**In this project.** Every field transmission ends with an over. The parser accepts `K`, `KN`, `AR` and `SK`, plus `(` and `+`, which are what `KN` and `AR` decode as when keyed run together: [`crates/protocol/src/lib.rs:131-138`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L131-L138). Every node transmission ends `DE <call> K`: [`crates/protocol/src/reply.rs:68`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L68). A letter `K` after an `AGN` code can also be a chunk letter (see [AGN](#agn)).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### K-plug

**Definition.** A two-pin headset jack used on many FM handhelds: a 3.5 mm and a 2.5 mm jack side by side. It carries the speaker, the microphone and the PTT. The UV-K1 has one.

**In this project.** The keyer box drives a UV-K1 through its K-plug: by MCW (stock firmware, the box holds the PTT and keys a tone into the microphone), or as a straight key on NR7Y's CW firmware: [`docs/keyer.md:444-452`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L444-L452), [`docs/keyer.md:840-847`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L840-L847). It also carries the radio's serial (programming) contact and a +V contact. On the MCW route the box uses only ground, PTT, microphone and speaker; the serial contact and +V are never connected, because the serial port can rewrite the radio's memory, calibration included: [`docs/keyer.md:546-549`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L546-L549). **Never loosen** (rule 6.4 in [13 Safety rules (never loosen)](13-safety-rules.md)). The bench tool's stop instructions tell the operator to pull the K-plug: [`crates/hfnode/src/keyer/bench.rs:487-493`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/bench.rs#L487-L493).

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

### Keep-alive

**Definition.** A message sent regularly only to show the sender is still there. If keep-alives stop, the receiver assumes the sender has died and acts on its own.

**In this project.** While a keyer-box run lasts, and only then, `hfnode` sends `STATUS` every `KEEP_ALIVE = 250` ms, well inside the box's [link timeout](#link-timeout), so that a run stops soon after the node dies: [`crates/hfnode/src/keyer/rig.rs:40-42`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/rig.rs#L40-L42).

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### KEY jack

**Definition.** A radio's input for a Morse key or paddle. The radio holds the key line at a few volts, and shorting it to the radio's ground keys the transmitter. On the IC-7300 it is a 6.35 mm (1/4 inch) stereo jack.

**In this project.** The keyer box plugs into it with a stereo plug, tip and sleeve wired and the ring left open. A mono plug shorts the ring, which on a radio set to Paddle is a dash paddle held down: [`docs/keyer.md:39-41`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L39-L41), [`docs/keyer.md:72-76`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L72-L76). The IC-7300's own Time-Out Timer does not cover keying from the KEY jack: [`docs/keyer.md:193-195`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L193-L195).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [09 Keyer box: host side](09-keyer-host-side.md).

### Key line and keying

**Definition.** A radio's key line is the contact of its key input that the radio pulls up to a few volts; shorting it to the radio's ground keys the transmitter. Keying a radio means making it transmit, through its key line or its [PTT](#ptt).

**In this project.** The keyer box keys a radio by closing its key line through the PC817, collector to the line and emitter to the radio's ground: [`docs/keyer.md:66-71`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L66-L71). The line is measured with a meter before anything is connected ("stage zero"): expect +3 V to +15 V and well under 1 mA when shorted: [`docs/keyer.md:236-251`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L236-L251). On the IC-7300 route the node keys through CI-V instead, with the radio's internal [keyer](#keyer).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [13 Safety rules (never loosen)](13-safety-rules.md).

### Key-down limit

**Definition.** In this project, the longest the keyer box lets its key (or its tone) stay on at once.

**In this project.** `KEY_DOWN_MS = 1_000` ms. A key-down past it opens the key and trips the box until it is power-cycled: [`crates/keyer-core/src/lib.rs:45-48`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L45-L48). **Never loosen** (rule 1.1 in [13 Safety rules (never loosen)](13-safety-rules.md)). See [Element](#element).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Keyer

**Definition.** A device that turns key or paddle movements, or text, into correctly timed Morse. Most modern radios have one built in.

**In this project.** The word has several meanings (see the table at the top). The IC-7300's internal keyer sends the node's text through CI-V command `17`, at most 30 characters at a time: [`crates/civ/src/ic7300.rs:135-138`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L135-L138), [`crates/civ/src/lib.rs:24`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L24). On the keyer box, the radio's own keyer is not used. The radio is set to Straight and the box times every element: [`docs/keyer.md:172-178`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L172-L178).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Keyer box

**Definition.** In this project, a small USB box built from a Raspberry Pi Pico 2 that closes a radio's key line through an optocoupler. It lets the node drive any CW radio, not only the IC-7300.

**In this project.** The repo doc is [`docs/keyer.md`](../keyer.md): [`docs/keyer.md:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1-L12). The box's rules and every limit live in the `keyer-core` crate, so that the firmware only moves pins: [`crates/keyer-core/src/lib.rs:1-15`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L1-L15), [`crates/keyer-core/src/keyer.rs:1-2`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L1-L2). The firmware is `firmware/pico2-keyer`: [`firmware/pico2-keyer/src/main.rs:1-41`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L1-L41). On the host, `station.rig = "keyer"` selects `KeyerRig`: [`crates/hfnode/src/keyer/rig.rs:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/rig.rs#L1-L12), [`crates/hfnode/src/keyer/rig.rs:165`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/rig.rs#L165). Nothing has keyed a real radio through the box yet: it has been flashed and greeted over USB on the Quansheng bench, and no keyed stage has run (`bench/quansheng.md`, `audit/final-grades.md`, project files).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [09 Keyer box: host side](09-keyer-host-side.md).

### KN

**Definition.** A Morse prosign meaning "go ahead, only the station I am working": others should not call. It is keyed as one character with no gap between the K and the N.

**In this project.** `KN` is an over, and run together it decodes as `(`. The parser accepts both: [`crates/protocol/src/lib.rs:131-133`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L131-L133). See [K](#k-1).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

## L

### last_seq

**Definition.** In this project, the highest code-sheet sequence number the node has used. A code counts only if its sequence number is greater than `last_seq`. That stops anyone replaying a code they overheard, and still lets the operator skip lines.

**In this project.** `auth::Verifier` applies the rule: [`crates/auth/src/lib.rs:169-175`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L169-L175). `commit` never moves it backwards: [`crates/auth/src/lib.rs:208-210`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L208-L210). `auth::SeqStore` keeps it in the file `last_seq` in the state directory ([`crates/hfnode/src/main.rs:554`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L554), [`crates/hfnode/src/config.rs:15-16`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L15-L16)). The file is written (temporary file, sync, replace) before the node acts on a code, and a missing file reads as 0: [`crates/auth/src/store.rs:1-17`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/store.rs#L1-L17), [`crates/auth/src/store.rs:29-54`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/store.rs#L29-L54). The open code is spent as soon as the transaction opens: [`crates/hfnode/src/session.rs:9-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L9-L12).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Link timeout

**Definition.** In this project, how long the keyer box goes on with a run without hearing from the computer.

**In this project.** `LINK_TIMEOUT_MS = 2_000` ms: a run stops when no valid line has arrived for that long, because the computer crashed, `hfnode` was killed or the cable came out: [`crates/keyer-core/src/lib.rs:58-60`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L58-L60). The host's [keep-alive](#keep-alive) keeps it from expiring during a healthy run, and `hfnode keyer linktest` checks it on the bench. **Never loosen** (see [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [09 Keyer box: host side](09-keyer-host-side.md).

### Listening window

**Definition.** In this project, a stretch of time in which the node listens and may reply. Outside a window it neither decodes nor transmits.

**In this project.** By default the node listens all the time (`schedule.always = true`). With `always = false` it listens only in windows of `window_minutes` every `every_minutes`, aligned to the top of the UTC hour and shifted by `offset_minutes`: [`crates/hfnode/src/config.rs:203-229`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L203-L229). At a window start the node sets the radio up and checks it, and tunes it if it has a tuner; in `hfnode run` a tune that matched is followed by `DE <call>`. In the always-listening mode this happens once, at start-up: [`crates/hfnode/src/station.rs:42-49`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L42-L49), [`crates/hfnode/src/station.rs:58-60`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L58-L60). Because the node may be listening at any time, stop it before using the radio by hand: [`docs/raspberry-pi-setup.md:383-387`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/raspberry-pi-setup.md#L383-L387).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md), [15 Config, CLI and deployment](15-config-cli-deployment.md).

### Lockout

**Definition.** In this project, the station refusing to transmit after a fault, until the next tune.

**In this project.** `TxError::SwrLockout`: since the last tune, SWR was too high, the output missing or too high, the radio stuck on transmit, a transmission failed while keying, the tuner could not match, or the tune failed, so transmitting is suspended until the next one: [`crates/hfnode/src/station.rs:224-230`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L224-L230). It lasts until the next tune: before a reply once the last tune is older than `schedule.retune_minutes`, or at the next window: [`crates/hfnode/src/config.rs:224-228`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L224-L228). Two faults in a row latch the [transmit inhibit](#transmit-inhibit) instead. **Never loosen.**

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

## M

### Maidenhead grid square

**Definition.** A worldwide location code used by radio amateurs: two letters and two digits, such as `DL89`, optionally followed by two more letters for a smaller square. Operators use it to say roughly where they are without giving an address.

**In this project.** A `WX` request names the forecast's place as a grid (`Place::Grid`) or a numbered preset: [`crates/protocol/src/lib.rs:46-52`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L46-L52). `protocol::is_grid` accepts 4- or 6-character locators: [`crates/protocol/src/lib.rs:368-369`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L368-L369).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md), [06 Gateways and messages](06-gateways-messages.md).

### MCW

**Definition.** "Modulated CW": Morse sent as an audio tone through a voice transmitter, usually FM. The carrier stays on the whole time and the tone is keyed. Any FM radio can receive it.

**In this project.** With `[keyer] output = "ptt"`, the keyer box holds an FM handheld's PTT and keys a 700 Hz tone into its microphone: [`docs/keyer.md:444-452`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L444-L452), [`crates/hfnode/src/keyer.rs:70-78`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L70-L78). The box's `MCW <wpm> <text>` command holds the PTT for 500 ms before the first element and 200 ms after the last: [`crates/keyer-core/src/lib.rs:103-115`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L103-L115). `hfnode` refuses `output = "ptt"` outside the bands Part 97 allows MCW in (50.1-54, 144.1-148, 222-225 and 420-450 MHz; band edges from memory, not checked against the eCFR): [`crates/hfnode/src/keyer.rs:57-67`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L57-L67), [`docs/keyer.md:470-473`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L470-L473).

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Mock radio

**Definition.** A program that stands in for a real radio in tests. It answers commands the way the radio would, with no hardware.

**In this project.** There are four, and a stand-in for the outside services:

- `civ::sim::SimRig`, an in-memory `Rig` for tests and `hfnode sim`: [`crates/civ/src/lib.rs:3-4`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L3-L4), [`crates/civ/src/sim.rs:13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/sim.rs#L13).
- `civ::mock`, a byte-level mock IC-7300 (`MockRadio`) behind a fake serial port (`MockPort`): [`crates/civ/src/mock.rs:1-10`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/mock.rs#L1-L10), [`crates/civ/src/mock.rs:485-488`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/mock.rs#L485-L488).
- `hfnode::keyer::mock`, the keyer box (`MockBox`), running the firmware's own `keyer_core`, plus the radio it keys (`MockRadio`): [`crates/hfnode/src/keyer/mock.rs:1-10`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/mock.rs#L1-L10), [`crates/hfnode/src/keyer/mock.rs:401`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/mock.rs#L401), [`crates/hfnode/src/keyer/mock.rs:676`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/mock.rs#L676).
- `hfnode::handheld::mock::MockFirmware`, the UV-K1 firmware's side of the parked handheld protocol: [`crates/hfnode/src/handheld/mock.rs:1-6`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld/mock.rs#L1-L6), [`crates/hfnode/src/handheld/mock.rs:270`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld/mock.rs#L270).
- `selftest::FakeServices` stands in for email, texting and weather: it records what was sent, serves a fixed inbox and canned weather: [`crates/hfnode/src/selftest.rs:515-518`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest.rs#L515-L518).

The [self-test](#selftest) runs the whole node against them.

**Covered in.** [12 Testing](12-testing.md).

### Morse unit

**Definition.** The time unit of Morse code, the length of one dot. A dash is 3 units, the gap inside a character 1, the gap between characters 3 and the gap between words 7.

**In this project.** `cw::units` measures text in Morse units with exactly those rules: [`crates/cw/src/morse.rs:98-101`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/morse.rs#L98-L101). The duration in milliseconds is `units(text) * 1200 / wpm`: [`crates/cw/src/morse.rs:131-134`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/morse.rs#L131-L134). The keyer box uses the same rules, with the dot rounded down to whole milliseconds: [`crates/keyer-core/src/morse.rs:1-6`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/morse.rs#L1-L6), [`crates/keyer-core/src/morse.rs:89-95`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/morse.rs#L89-L95).

**Covered in.** [04 CW decoder and audio](04-cw-decoder-audio.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### MPU

**Definition.** The memory protection unit, a part of each Arm core that limits which addresses the running code may read, write or execute. An access outside its regions faults. Each RP2350 core has its own, with eight Secure regions.

**In this project.** Used only in the one-Pico image (PR #31, draft, not on main, untested on hardware). Before anything else, core 1 turns its MPU on with the default memory map off and seven regions: its flash, its stack, the shared link, the audio ring, the ADC, and (read-only) the DMA channel and the timer. Every other address faults, the GPIO blocks included: [`firmware/pico2-keyer/src/core1.rs:16-22`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L16-L22), [`firmware/pico2-keyer/src/core1.rs:266-287`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L266-L287). Core 1 then drops to [unprivileged](#secure-and-non-secure-privileged-and-unprivileged) mode, so it cannot change the MPU: [`firmware/pico2-keyer/src/core1.rs:310-332`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L310-L332).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### MSPLIM

**Definition.** The Main Stack Pointer Limit register of an Armv8-M core. A push below that address faults instead of overwriting the memory under the stack, so a stack overflow is caught when it happens.

**In this project.** rustos's multicore driver sets core 1's `MSPLIM` to the bottom of its stack before core 1's first push: [`firmware/pico2/src/multicore.rs:44-56`](https://github.com/robinonsay/rustos/blob/410dffa091e247e6e7546c7f2ff465f0af499d5a/firmware/pico2/src/multicore.rs#L44-L56) (rustos PR #6, open, not on master, untested on hardware). In the one-Pico image (PR #31, draft, not on main, untested on hardware), core 0's `MSPLIM` is the end of the memory the statics take: [`firmware/pico2-keyer/src/main.rs:42-43`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/main.rs#L42-L43).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 06 (Multicore).

## N

### NR7Y firmware

**Definition.** The NR7Y CW firmware is a third-party firmware for the Quansheng UV-K1 and UV-K5 v3 handhelds. It gives the radio a real CW mode. With NR7Y's paddle rework, a key on the headset jack keys it.

**In this project.** Two routes use it. The current one keys a reworked UV-K1 on NR7Y v1.3 or v1.3.1, as released and unmodified, through the keyer box's key output, with `[keyer] output = "key"`: [`docs/keyer.md:840-847`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L840-L847). The older route, `firmware/uv-k1`, is NR7Y's firmware with `hfnode` commands added. It is parked, and nothing in it has run on a radio: [`firmware/uv-k1/README.md:1-13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/uv-k1/README.md#L1-L13), [`docs/handheld.md:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/handheld.md#L1-L12).

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

## O

### One-Pico image

**Definition.** In this project, the keyer box firmware that also decodes CW: core 0 runs the keyer and is the only core that keys, and core 1 runs the decoder on the radio's audio, so one Pico 2 does both jobs.

**In this project.** It exists only on PR #31 (draft, not on main, stacked on PR #30, untested on hardware): [`firmware/pico2-keyer/src/core1.rs:1-8`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L1-L8). It pins rustos at PR #6's head (see [rustos](#rustos)). The audit has cleared its image to flash only after PR #31 merges and `hfnode` is rebuilt from main (`audit/final-grades.md`, project files). See [Core 0 and core 1](#core-0-and-core-1), [MPU](#mpu), [CP0](#cp0), [SECCFG](#seccfg) and [Heartbeat](#heartbeat).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 06 (Multicore).

### Over

See [K](#k-1).

## P

### Pad

**Definition.** The physical pin circuit of a chip: its output driver, input buffer and pull resistors. On the RP2350, `PADS_BANK0` sets how each pad behaves electrically (drive strength, pulls, input enable, isolation), separately from which function drives it.

**In this project.** rustos's GPIO driver sets the pad up for each pin: [`firmware/pico2/src/gpio/gpio.rs:1-20`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/gpio/gpio.rs#L1-L20) (rustos master). Two pad behaviours matter for safety: the [ISO latch](#iso-latch) and [erratum RP2350-E9](#erratum-rp2350-e9).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

### Part 97

**Definition.** Part 97 of Title 47 of the US Code of Federal Regulations, the FCC's rules for the amateur radio service. It sets the bands, which modes may be used where, how stations identify, and what content may be sent.

**In this project.** The code cites §97.119(a) for identification ([Station ID](#station-id-and-47-cfr-97119)), §97.301 and §97.305(a) for the band table, and §97.305(c) for the MCW bands (50.1-54, 144.1-148, 222-225 and 420-450 MHz). Those band edges are marked "from memory, not checked against the eCFR": [`crates/hfnode/src/keyer.rs:37-67`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L37-L67). It cites §97.113 for the content of inbound messages, which the reply filter screens: [`crates/hfnode/src/gateway/filter.rs:1-21`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/gateway/filter.rs#L1-L21). The audit grades hardware safety only; Part 97 questions, including [automatic control](#automatic-control) for unattended operation (§97.109(d)), are outside it and are Robin's call as licensee (`audit/rubric.md` §G and §H; `audit/final-grades.md`, project files). Separately, unattended operation is not cleared on any route until Robin's own bench results are in ([H1](#h1)). An earlier IC-7300 grading (`audit/report.md` §4, at `2e39996`, 2026-10-07) called unattended use out of scope until the Part 97 automatic-control question was answered. Rubric section H (added 2026-10-08) replaced that, and `audit/final-grades.md` now grades IC-7300 unattended "not yet": it needs Robin's bench results (H1), the hardware timer of test-plan step 10 and the audit's link-loss test.

**Covered in.** [06 Gateways and messages](06-gateways-messages.md), [13 Safety rules (never loosen)](13-safety-rules.md), [16 Work left and known bugs](16-work-left-known-bugs.md) (WL15).

### PC817 optocoupler

**Definition.** An optocoupler is an LED and a light-sensitive transistor in one package. Current through the LED turns the transistor on, and the two sides share no wire. The PC817 is a common, cheap one.

**In this project.** It is the keyer box's key switch. GP16 drives the LED through 470 Ω (about 4.5 mA). The collector goes to the key plug's tip and the emitter to its sleeve, and polarity matters: [`docs/keyer.md:32-37`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L32-L37), [`docs/keyer.md:50-71`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L50-L71). It keeps the computer's ground off the key line, but it does not isolate the station, because the audio cable ties the grounds together anyway: [`docs/keyer.md:66-71`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L66-L71). Other optocouplers have not been checked against this circuit.

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Pending transaction

**Definition.** In this project, a request the node has read back and is waiting to have confirmed. It is not acted on until the field operator sends `OK` with the next code.

**In this project.** `Session` holds at most one. It is forgotten after `pending_timeout_secs` (default 600) without an `OK` or `NO`: [`crates/hfnode/src/config.rs:17-19`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L17-L19), [`crates/hfnode/src/config.rs:731-733`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L731-L733), [`crates/hfnode/src/session.rs:268-272`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L268-L272). See [Read-back](#read-back) and [Session](#session).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### PHONES jack

**Definition.** A radio's headphone output. It carries the received audio and, while the radio sends CW, its [sidetone](#sidetone).

**In this project.** A radio on the keyer box is heard through its PHONES (or EXT SP) jack and a sound card, and the node decodes and monitors that audio: [`docs/keyer.md:9-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L9-L12), [`docs/keyer.md:42-45`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L42-L45). Headphone level can be far more than a mic input wants, so start with the volume low or use a line input.

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md), [04 CW decoder and audio](04-cw-decoder-audio.md).

### Pico 2

**Definition.** The Raspberry Pi Pico 2, a small, cheap board built around the [RP2350](#rp2350) microcontroller, with USB, 4 MB of flash and a BOOTSEL button.

**In this project.** The keyer box, and the decoder box of PR #30, are each a Pico 2. It must not be a Pico 2 W, whose GP25 belongs to its wireless chip: [`docs/keyer.md:30-31`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L30-L31). The firmware is built for 4 MB of flash and 520 kB of SRAM: [`firmware/pico2-keyer/uf2.py:30-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/uf2.py#L30-L35).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### Piece

**Definition.** In this project, the text one keying command sends: at most 30 characters, because that is the most CI-V command `17` takes. The station splits every transmission into pieces and keys them one at a time, with checks around each.

**In this project.** `civ::MAX_CW_CHARS = 30`: [`crates/civ/src/lib.rs:24`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L24). The station sends text in pieces, with a pause between them. A piece counts as finished only once its full keying time has passed and the radio reads receive. On the IC-7300, SWR is sampled for the whole time each piece keys: [`crates/hfnode/src/station.rs:6-10`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L6-L10), [`crates/hfnode/src/station.rs:25-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L25-L35).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

### Po meter

**Definition.** The IC-7300's RF output-power meter. It shows how much power the transmitter is putting out, as a share of full output.

**In this project.** CI-V `15 11` reads it, 0 to 100 %: [`crates/civ/src/ic7300.rs:125-126`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L125-L126). The station counts an SWR sample only while the Po meter shows output, stops when it shows no output for several samples in a row while keying, and inhibits transmitting if the radio reads receive while Po shows output: [`crates/hfnode/src/station.rs:25-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L25-L35). With no wattmeter on the bench, the radio's own Po meter is the only power reading (`audit/req-on-air-bench-ruling.md` §4.3, project files). **Never loosen.**

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md), [07 CI-V and the IC-7300](07-civ-ic7300.md).

### Preflight

**Definition.** In this project, the read-only check of the IC-7300 that runs before the node writes anything to it. Because it only reads, a radio in an unsafe state is refused before anything has been written to it.

**In this project.** `civ::preflight` only reads. It fails if the radio is not an IC-7300 at the configured address, is transmitting, could be keyed by the USB serial lines, VOX or the tuner's PTT Start, or has split or [∂TX](#delta-tx-tx) on. For a command that can transmit, it also requires the radio's Time-Out Timer at 3 minutes and TX Inhibit off: [`crates/civ/src/preflight.rs:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/preflight.rs#L1-L12). **Never loosen.** PR #27 (open, ready, audit-cleared, not on main) also requires that no front-panel or microphone key is set to send a keyer memory, and that the microphone's keys are not set up as a CW key (MIC Up/Down Keyer, `1A 05 01 65`, OFF); `tune`, `cw` and `run` refuse to start otherwise: [`crates/civ/src/preflight.rs:12-13`](https://github.com/robinonsay/cw-message-gateway/blob/3c97427bcf59fcb8223af7a3676eeebdec529f52/crates/civ/src/preflight.rs#L12-L13), [`crates/civ/src/preflight.rs:370-426`](https://github.com/robinonsay/cw-message-gateway/blob/3c97427bcf59fcb8223af7a3676eeebdec529f52/crates/civ/src/preflight.rs#L370-L426).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

### PTT

**Definition.** Push-to-talk: the switch or line that puts a radio on transmit while it is held. On a handheld it is the side button, and also a contact on the headset jack.

**In this project.** The keyer box has a PTT output, GP17, for an FM handheld, and an input, GP19, that reads the PTT line: [`firmware/pico2-keyer/src/main.rs:68-73`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L68-L73). The PTT may stay down at most `PTT_MS = 60_000` ms. Past that the box opens it and trips: [`crates/keyer-core/src/lib.rs:52-57`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L52-L57). **Never loosen.** On the IC-7300, `hfnode` never forces transmit on; `set_transmit` is only ever called with `false`: [`crates/civ/src/lib.rs:105-109`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L105-L109).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

### Pull-down

**Definition.** A resistor from a pin to ground that holds the pin at 0 V while nothing drives it. The RP2350's pads have weak internal pull-downs, enabled at reset: [`firmware/pico2/src/gpio/gpio.rs:393-403`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/gpio/gpio.rs#L393-L403) (rustos master).

**In this project.** Because of [erratum RP2350-E9](#erratum-rp2350-e9), the internal pull-down is not trusted. The keyer box needs an external 4.7 kΩ from GP16 (the key) to ground, and another from GP17 (the PTT) to ground whenever the PTT output is wired. They keep the key and the PTT open while the chip starts, resets or is unplugged: [`docs/keyer.md:57-65`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L57-L65), [`docs/keyer.md:576-578`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L576-L578), [`docs/keyer.md:591-593`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L591-L593), [`firmware/pico2-keyer/src/main.rs:14-18`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L14-L18). **Never loosen** (rule 2.3 in [13 Safety rules (never loosen)](13-safety-rules.md)). The 2N3904 switch keeps the same pull-down on the pin's side of the base resistor: [`docs/keyer.md:1049-1056`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1049-L1056).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [13 Safety rules (never loosen)](13-safety-rules.md).

### PWM

**Definition.** Pulse-width modulation: a hardware counter that switches a pin on and off at a set frequency and duty cycle. The RP2350's PWM block has slices, each with two channels: [`firmware/pico2/src/pwm.rs:1-25`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/pwm.rs#L1-L25) (rustos master).

**In this project.** The keyer box makes the MCW tone with PWM on GP18 (slice 1, channel A) at 700 Hz, filtered into the handheld's microphone: [`firmware/pico2-keyer/src/main.rs:70-77`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L70-L77). A panic or fault turns every PWM output off and holds the tone pin low: [`firmware/pico2-keyer/src/main.rs:246-252`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L246-L252).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 05 (Drivers: GPIO, USB, PWM, ADC, DMA).

## Q

### QRL?

**Definition.** A Morse Q-code question: "Is this frequency in use?" Operators send it, then listen, before transmitting on a frequency, so they do not interfere with a contact already there.

**In this project.** Not found in the code at `44185fa`. On NR7Y CW, the on-air routine before every keyed bench step sends `QRL? DE <call>` and listens again before going on: [`docs/keyer.md:1400-1414`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1400-L1414). On the IC-7300 the first `QRL? DE <call>` follows the tune and is that tune's identification, because stage `setup` cannot key CW yet (`audit/req-on-air-bench-ruling.md` §4.2, project files): [`docs/hardware-test-plan.md:1047-1058`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/hardware-test-plan.md#L1047-L1058).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### QRP

**Definition.** Low-power operation, by convention 5 W or less for CW. QRP radios are small and run on batteries, which makes them good for the field.

**In this project.** The node relays texts and email "over QRP Morse code": [`README.md:3`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/README.md#L3). The field operator's QRP signal is the weak direction of the link (design spec, project files). The keyer box can drive a QRP rig, and most QRP rigs have no time-out timer, so the box's limits are what stop a stuck key: [`docs/keyer.md:1-7`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1-L7), [`docs/keyer.md:193-195`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L193-L195).

**Covered in.** [01 System overview](01-system-overview.md).

## R

### Read-back

**Definition.** In this project, the node's reply to an open request. It repeats what it understood, ending with `?`, and does nothing until the operator confirms with a second code.

**In this project.** A `TX` read-back is `R <seq> TX <dest> <text> ? DE <call> K`. `RX` and `WX` have their own forms (`Reply::ReadBackTx`, `ReadBackRx` and `ReadBackWx`: [`crates/protocol/src/reply.rs:6-22`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L6-L22)): [`crates/protocol/src/reply.rs:43-68`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L43-L68). The two-code exchange is: open, read-back, `OK` with the next code, then the node acts: [`crates/hfnode/src/session.rs:1-13`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L1-L13).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Rest

**Definition.** In this project, a time the keyer box keeps the key up after a run, before it takes another. It stops a stream of runs sent back to back from adding up to one long key-down.

**In this project.** `REST_MS = 1_000`. However a run ends, the key stays up at least 1 s, so runs sent back to back cannot hold the key down between them: [`crates/keyer-core/src/lib.rs:63-66`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L63-L66). A run started too soon is refused with `ERR CW REST`: [`crates/keyer-core/src/keyer.rs:929-931`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L929-L931). **Never loosen.** On the host, `Rig::rest_needed` is the wider wait before each run (the box's rest, its duty budget, a busy channel): [`crates/civ/src/lib.rs:121-129`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L121-L129).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [09 Keyer box: host side](09-keyer-host-side.md).

### Rig

**Definition.** Radio operators' word for a transceiver. A transceiver both transmits and receives; the node's code says rig for whatever radio it drives, whether an IC-7300 over CI-V, any radio through the keyer box, or a handheld.

**In this project.** `civ::Rig` is the trait for everything the node needs from a radio. The station's safety layer drives every radio through it: [`crates/civ/src/lib.rs:57-58`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L57-L58). `station.rig` picks the implementation: `ic7300` (the default), `keyer` or `handheld`: [`crates/hfnode/src/config.rs:41-45`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L41-L45), [`crates/hfnode/src/config.rs:85-93`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L85-L93).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md), [07 CI-V and the IC-7300](07-civ-ic7300.md).

### Route

**Definition.** In this project, one way of connecting the node to a radio: which rig the node drives, and what it keys on that radio. Each route has its own wiring, settings, bring-up stages and audit verdicts.

**In this project.** `station.rig` picks the rig ([`crates/hfnode/src/config.rs:85-93`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L85-L93)) and, with the keyer box, `[keyer] output` picks what it keys ([`crates/hfnode/src/keyer.rs:69-78`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L69-L78)). The routes are:

- **IC-7300 over CI-V** (`station.rig = "ic7300"`): the node keys the radio's internal keyer.
- **Keyer box, key output** (`station.rig = "keyer"`, `output = "key"`): any CW radio's KEY jack.
- **NR7Y straight key** (`station.rig = "keyer"`, `output = "key"`): a paddle-reworked UV-K1 on NR7Y's CW firmware, its PTT line keyed as a straight key ([`docs/keyer.md:840-847`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L840-L847)).
- **MCW** (`station.rig = "keyer"`, `output = "ptt"`): an FM handheld on stock firmware, its PTT held and a tone keyed into its microphone.
- **Parked handheld route** (`station.rig = "handheld"`): hfnode commands added to NR7Y's firmware over USB-C; parked, never run on a radio.

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md), [13 Safety rules (never loosen)](13-safety-rules.md) (group 6, handheld rules).

### RP2350

**Definition.** Raspberry Pi's microcontroller, with two Arm Cortex-M33 cores and 520 kB of SRAM. The program runs from external flash, which the Pico 2 board provides (4 MB). The firmware here runs on the Arm cores in Secure mode.

**In this project.** All the firmware targets it through rustos. The UF2 family is `RP2350_ARM_S`: [`firmware/pico2-keyer/uf2.py:30-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/uf2.py#L30-L35). Chip behaviours that matter for safety are the [ISO latch](#iso-latch), [erratum RP2350-E9](#erratum-rp2350-e9) and the [watchdog](#watchdog).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 01 (Overview).

### Run

**Definition.** In this project, one `CW` or `MCW` command to the keyer box, from the moment the box takes it until it ends. Also `hfnode run`, the command that runs the node.

**In this project.** A run may last at most `RUN_MS = 60_000` ms. Text whose Morse length is over that is refused: [`crates/keyer-core/src/lib.rs:49-51`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L49-L51). A run stops when no valid line has arrived for `LINK_TIMEOUT_MS = 2_000` ms: [`crates/keyer-core/src/lib.rs:58-60`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L58-L60). **Never loosen either.** On the IC-7300, `hfnode run` needs bring-up stage `done` ([`crates/hfnode/src/commissioning.rs:43-44`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L43-L44)), a preflight that finds the radio's TOT at 3 minutes ([`crates/hfnode/src/commissioning.rs:190-195`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L190-L195)) and `station.max_key_seconds` of at most `RUN_MAX_KEY_SECONDS = 55` ([`crates/hfnode/src/commissioning.rs:121-125`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L121-L125)). With the keyer box, `run` needs `[keyer] commissioned = "done"` ([`crates/hfnode/src/keyer.rs:136-137`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L136-L137)); with the parked handheld route, `[handheld] commissioned = "done"` ([`crates/hfnode/src/handheld.rs:137-139`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L137-L139)). **Never loosen.**

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [15 Config, CLI and deployment](15-config-cli-deployment.md).

### rustos

**Definition.** Robin's bare-metal Rust OS for the Pico 2, in its own repository (`github.com/robinonsay/rustos`). It has an `api` crate of portable hardware traits, with no register addresses, and a `pico2` crate of RP2350 drivers: [`api/src/lib.rs:1-7`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/api/src/lib.rs#L1-L7), [`Cargo.toml:1-7`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/Cargo.toml#L1-L7) (rustos master). This project added its clocks, timer, watchdog, USB CDC-ACM, PWM, ADC, DMA and multicore drivers.

**In this project.** The keyer firmware on `main` pins `api` and `pico2` by git `rev` at `8c915079996f9d6537a1ec1f56dc5e3ea5cccc4c`, master's merge of rustos #4: [`firmware/pico2-keyer/Cargo.toml:21-31`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/Cargo.toml#L21-L31). The comment there says that moving this pin can change the UF2 with no source change. The driver files this page cites (`watchdog.rs`, `gpio/`, `usb/`, `pwm.rs`) are the same at `8c91507` and at master `80bf599`; master adds the ADC and DMA drivers and changes the clock and board code. The [one-Pico image](#one-pico-image) (PR #31, draft, not on main) pins rustos PR #6 (open, untested on hardware) at `410dffa`: [`firmware/pico2-keyer/Cargo.toml:24-31`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/Cargo.toml#L24-L31). Any re-pin is a new build id, a new hash and a new audit. **Never loosen** (rule 7.3 in [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** The rustos handover set, <https://github.com/robinonsay/rustos/blob/claude/handover-docs-us00cn/docs/handover/README.md>, in particular rustos pages 03 (The api crate) and 08 (Changes made for cw-message-gateway); and [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

## S

### Scratch registers

**Definition.** Eight words in the RP2350's watchdog block, `SCRATCH0` to `SCRATCH7`, that keep their contents through a watchdog reset but are cleared by power-on and every other chip-level reset. The boot ROM uses `SCRATCH2` to `SCRATCH7`, so only `SCRATCH0` and `SCRATCH1` are free for the application: [`firmware/pico2/src/watchdog.rs:129-149`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/watchdog.rs#L129-L149) (rustos master).

**In this project.** The keyer box saves its trip, whether the key was down, and its duty budget in those two words (`Saved`, checked with a magic number), so a trip survives a watchdog reset but not unplugging: [`crates/keyer-core/src/keyer.rs:222-243`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L222-L243), [`firmware/pico2-keyer/src/main.rs:127-130`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L127-L130), [`firmware/pico2-keyer/src/main.rs:198-201`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L198-L201).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), and rustos page 04 (Drivers: board, clocks, time, watchdog).

### SECCFG

**Definition.** The RP2350 DMA's per-channel security register, `SECCFG_CHn`. It sets the lowest security level that may read or write the channel's registers; out of reset every channel is Secure Privileged, the highest. Bit 1 `S` marks the channel Secure, and bit 2 `LOCK` makes the register read-only until the DMA block is reset.

**In this project.** In the one-Pico image (PR #31, draft, not on main, untested on hardware), core 0 sets DMA channel 0 to [Secure Unprivileged](#secure-and-non-secure-privileged-and-unprivileged) and locks it, then reads it back, so that unprivileged core 1 may read the channel's position without a bus fault. If the read-back is wrong, there is no decoder: [`firmware/pico2-keyer/src/core1.rs:74-82`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L74-L82), [`firmware/pico2-keyer/src/core1.rs:104-136`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L104-L136).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md).

### Secure and Non-secure, privileged and unprivileged

**Definition.** The RP2350's Arm Cortex-M33 cores follow Armv8-M, which has two security states, Secure and Non-secure, and in each two privilege levels. Privileged code may configure the core itself, such as its MPU and its stack limit; unprivileged code may not. Some RP2350 blocks, such as each DMA channel, can be limited to a lowest security level that may touch them, and an access from below that level gets a bus fault (RP2350 datasheet §12.6.6.1, as the audit's `audit/report.md` quotes it, project files).

**In this project.** All the firmware runs Secure (the UF2 family is `RP2350_ARM_S`): [`firmware/pico2-keyer/uf2.py:30-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/uf2.py#L30-L35). In the one-Pico image (PR #31, draft, not on main, untested on hardware), core 1 drops to Secure Unprivileged, so it cannot undo its MPU or its `MSPLIM`: [`firmware/pico2-keyer/src/core1.rs:310-315`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/firmware/pico2-keyer/src/core1.rs#L310-L315). For the same reason core 0 opens DMA channel 0 to Secure Unprivileged access (see [SECCFG](#seccfg)).

**Covered in.** [11 Decoder box and the one-Pico image (in flight)](11-decoder-box-one-pico.md), and rustos page 06 (Multicore).

### Selftest

**Definition.** In this project, `hfnode selftest`: closed-loop scenarios that run the whole node against a mock radio and a scripted field operator, with no radio, sound card or network. It is how the whole node is tested without hardware, on a developer's machine and in CI.

**In this project.** The design is in [`crates/hfnode/src/selftest.rs:1-32`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest.rs#L1-L32), and the command is [`crates/hfnode/src/main.rs:143-149`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L143-L149). Time runs `scale` times faster than real time, by default 100, at most 200, and the `keyer-` scenarios at most 5 (`KEYER_MAX_SCALE`): [`crates/hfnode/src/main.rs:150-153`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L150-L153), [`crates/hfnode/src/selftest.rs:72-77`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest.rs#L72-L77), [`crates/hfnode/src/selftest.rs:36`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest.rs#L36), [`crates/hfnode/src/selftest/any_radio.rs:48`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/selftest/any_radio.rs#L48). CI runs every scenario through `cargo test` at `HFNODE_E2E_SCALE=20` on Linux, macOS and Windows ([`.github/workflows/ci.yml:14-16`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L14-L16), [`.github/workflows/ci.yml:58-70`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L58-L70)), and the `hfnode selftest --scale 20` binary on macOS and Windows only ([`.github/workflows/ci.yml:89-90`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L89-L90)). **Never loosen the 20x CI scale.**

**Covered in.** [12 Testing](12-testing.md).

### Sequence number

**Definition.** In this project, the number of one line of the code sheet. Each line's code is computed from its sequence number with the node's secret key.

**In this project.** `CodeBook::code(seq)` is the HMAC-SHA256 of the 8-byte big-endian sequence number, encoded as 8 letters: [`crates/auth/src/lib.rs:7-11`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L7-L11), [`crates/auth/src/lib.rs:105-110`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/auth/src/lib.rs#L105-L110). Every field message carries `seq code`: [`crates/protocol/src/lib.rs:3-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/lib.rs#L3-L12). A sequence number must be greater than [last_seq](#last_seq).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Session

**Definition.** In this project, the transaction state machine that takes decoded text from the field and decides what to transmit and when to act. It keeps at most one [pending transaction](#pending-transaction) and acts on nothing without a second code.

**In this project.** `hfnode::session::Session`, with `handle(decoded, now, svc) -> Outcome`: [`crates/hfnode/src/session.rs:206-218`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L206-L218), [`crates/hfnode/src/session.rs:267`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L267). Its rules: two codes, read back then commit, burn the open code, silence is the NACK, save `last_seq` before acting: [`crates/hfnode/src/session.rs:1-27`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/session.rs#L1-L27). In the bench docs, "session" also means one sitting at the radio, as in "Setting up the radio, every session": [`docs/keyer.md:644`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L644).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Sidetone

**Definition.** The tone a radio plays in its own headphones or speaker while it sends CW, so the operator hears what they key. It is generated in the radio and is not the received signal.

**In this project.** A radio on the keyer box must have its sidetone on and heard in the headphone output, because the node listens for it to confirm the radio keyed: [`docs/keyer.md:181-185`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L181-L185). `[keyer] sidetone_hz` sets its pitch if it differs from `audio.pitch_hz` (default 600 Hz): [`crates/hfnode/src/config.rs:102-104`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L102-L104), [`crates/hfnode/src/config.rs:769-771`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L769-L771). `hfnode keyer sidetone` keys `DE <call>` and measures the sidetone's delay, level and pitch: [`crates/hfnode/src/main.rs:309-311`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/main.rs#L309-L311).

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md).

### Sidetone monitor

**Definition.** In this project, the code that listens to a keyer-box radio's audio and works out from it what the radio's key is doing. With the keyer box there is no CI-V to read the radio's state, so the radio's own audio is the second, independent witness of whether it is transmitting.

**In this project.** `hfnode::keyer::monitor::Monitor` cuts the audio into 10 ms slices at the sidetone pitch. `judge` checks the radio keyed a run element by element; `key_state` finds a key held at the radio when the box's is open; `band` checks audio is arriving and no carrier sits on the pitch. For an MCW handheld it listens to receive noise instead: [`crates/hfnode/src/keyer/monitor.rs:1-45`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/monitor.rs#L1-L45), [`crates/hfnode/src/keyer/monitor.rs:352`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/monitor.rs#L352). `KeyerRig` treats the box's `STATUS` and the monitor as two independent sources, and the radio counts as transmitting if either says so: [`crates/hfnode/src/keyer/rig.rs:6-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer/rig.rs#L6-L12).

**Covered in.** [09 Keyer box: host side](09-keyer-host-side.md).

### Snap

**Definition.** In this project, matching a garbled decoded word to the nearest word the parser expects, by how many Morse elements differ rather than how many letters differ.

**In this project.** `protocol::fuzzy::snap` returns the vocabulary entry nearest to a token if it is within a tolerance and strictly nearer than every other entry; ties are refused: [`crates/protocol/src/fuzzy.rs:40-43`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/fuzzy.rs#L40-L43). The distance is `morse_distance`, an edit distance over dot and dash patterns: [`crates/protocol/src/fuzzy.rs:10-11`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/fuzzy.rs#L10-L11).

**Covered in.** [03 Field protocol, codes and sessions](03-field-protocol-auth-session.md).

### Squelch

**Definition.** A receiver function that silences the audio until a signal rises above the noise. On an FM handheld, "squelch open" means the speaker carries the receive noise; "squelch closed" means silence.

**In this project.** The CW decoder has a squelch of its own: a tone must rise `squelch_sigmas` (default 4) noise deviations above the floor to count: [`crates/cw/src/decoder.rs:57-58`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/decoder.rs#L57-L58), [`crates/cw/src/decoder.rs:73`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/decoder.rs#L73). On the parked handheld route the node keys only once the radio's squelch has been closed for `busy_quiet_ms`: [`crates/hfnode/src/config.rs:160-163`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L160-L163). On the MCW route the radio's squelch is left open so the node can hear receive noise: [`docs/keyer.md:450-452`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L450-L452).

**Covered in.** [04 CW decoder and audio](04-cw-decoder-audio.md), [09 Keyer box: host side](09-keyer-host-side.md), [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

### Stage

**Definition.** In this project, a bring-up stage: a step of the hardware bring-up that, once passed and written into the config, unlocks more commands. The stages make the node refuse anything riskier than what has already been shown to work on that radio.

**In this project.** The config key `commissioned` names the last stage passed. Commands that need a later stage are refused: [`crates/hfnode/src/config.rs:59-63`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L59-L63). There are three enums:

- **IC-7300** (`station.commissioned`): `none`, `link`, `setup`, `tune`, `keying`, `done`. Power above 10 W (`BENCH_MAX_WATTS`) needs `keying`, and `run` needs `done`: [`crates/hfnode/src/commissioning.rs:24-45`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L24-L45), [`crates/hfnode/src/commissioning.rs:60-61`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L60-L61). **Never loosen** (rules 4.5 and 4.6 in [13 Safety rules (never loosen)](13-safety-rules.md)).
- **Keyer box** (`keyer.commissioned`): `none`, `listen`, `keying`, `done`: [`crates/hfnode/src/keyer.rs:121-138`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/keyer.rs#L121-L138).
- **Handheld** (`handheld.commissioned`): `none`, `listen`, `keying`, `done`: [`crates/hfnode/src/handheld.rs:123-140`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L123-L140).

"Stage zero" in `docs/keyer.md` is different: a meter measurement of the radio's key line, with no computer involved, before stage `none`: [`docs/keyer.md:236-251`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L236-L251). The bench files number their own [bench stages](#bench-stage).

**Covered in.** [15 Config, CLI and deployment](15-config-cli-deployment.md), [14 Audit, bench and flashing](14-audit-bench-flashing.md).

### Station

**Definition.** In amateur radio, a station is the radio equipment and antenna at one place, operated under one callsign. Here it is the home end of the link: the computer running `hfnode` and the radio it drives.

**In this project.** `[station]` in the config is the radio's settings: rig, callsigns, frequency, serial port, power, keyer speed and limits: [`crates/hfnode/src/config.rs:40-83`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L40-L83). `hfnode::station::Station` is the transmit-side safety layer and "the only code that keys the radio": [`crates/hfnode/src/station.rs:1-4`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L1-L4), [`crates/hfnode/src/station.rs:887`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L887).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

### Station ID and 47 CFR 97.119

**Definition.** US amateur rules (47 CFR 97.119(a)) require a station to send its callsign at the end of each communication and at least every 10 minutes during one. In CW that is usually `DE <call>`.

**In this project.** Every transmission ends `DE <call> K`: [`crates/protocol/src/reply.rs:35-41`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L35-L41). Inside a long transmission, the station keys `DE <call>` between chunks, so that no more than `ID_INTERVAL` (8 minutes) passes between IDs: [`crates/hfnode/src/station.rs:50-55`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L50-L55), [`crates/hfnode/src/station.rs:143-147`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L143-L147). **Never loosen.** In `hfnode run`, a tune at window start that matched is followed by `DE <call>`, so its carrier is identified; a tune before a reply is identified by the reply: [`crates/hfnode/src/station.rs:42-49`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L42-L49).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

### Storm stand-down

**Definition.** In this project, the node refusing to tune or transmit while thunder is forecast or warned at the station. The IC-7300's manual says never to operate it during a lightning storm.

**In this project.** A thread asks the US National Weather Service every `check_minutes` (default 5) about the next `lookahead_hours` (default 2). It fails closed: the station is held until the first check succeeds and after any failed check. A hold ends only after `clear_minutes` (default 30) without thunder: [`crates/hfnode/src/storm.rs:1-26`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/storm.rs#L1-L26), [`crates/hfnode/src/config.rs:722-730`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L722-L730). `hfnode run` refuses to start without a `[storm]` section: [`crates/hfnode/src/config.rs:27-29`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L27-L29). The station side is `StormHold`: [`crates/hfnode/src/storm.rs:37-46`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/storm.rs#L37-L46). **Never loosen** the fail-closed hold or `run`'s refusal without `[storm]` (rule 5.16 in [13 Safety rules (never loosen)](13-safety-rules.md)).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

### Straight key

**Definition.** The simplest Morse key: one switch, closed for as long as each dot or dash lasts. The operator, not the radio, times every element.

**In this project.** The keyer box behaves as a straight key, so the radio must be set to Straight. In Paddle or Bug mode a closed contact sends dits by itself, the box's key-down limit never sees one long key-down, and the sidetone check cannot tell the radio's keyer from the node's text: [`docs/keyer.md:172-178`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L172-L178).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

### SWD

**Definition.** Serial Wire Debug, Arm's two-wire debug port. A probe on it can halt the processor, step it and rewrite any memory or register.

**In this project.** Never attach an SWD probe or debugger while the keyer box is plugged into the radio. A debugger can rewrite the watchdog's registers, and then nothing is left to open the key: [`docs/keyer.md:87-91`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L87-L91).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [13 Safety rules (never loosen)](13-safety-rules.md).

### SWR

**Definition.** Standing-wave ratio: how well the antenna system takes the transmitter's power. 1.0 is a perfect match. A high SWR means power comes back into the radio, which can damage it, and a sudden rise means a fault in the coax or antenna.

**In this project.** The IC-7300 reads SWR with CI-V `15 12`: [`crates/civ/src/ic7300.rs:127-129`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L127-L129). The station samples SWR for the whole time every piece keys, and above `station.swr_limit` (default 2.0, and it must be 1.1-3.0: [`crates/hfnode/src/config.rs:953-955`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L953-L955)) it stops at once: [`crates/hfnode/src/station.rs:25-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L25-L35), [`crates/hfnode/src/config.rs:74-76`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L74-L76), [`crates/hfnode/src/config.rs:749-751`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L749-L751). **Never loosen.** Rigs without meters (the keyer box and the handheld) cannot check SWR: [`crates/civ/src/lib.rs:115-120`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L115-L120).

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md), [07 CI-V and the IC-7300](07-civ-ic7300.md).

## T

### TOT

**Definition.** A radio's own Time-Out Timer: it ends a transmission after a set time, whatever is keying it. It is a backstop that works even if the controlling software has stopped.

**In this project.** Before anything transmits, the IC-7300's TOT must be at its shortest setting, 3 minutes (`REQUIRED_TOT`), read with CI-V `1A 05 00 29`: [`crates/hfnode/src/station.rs:350-353`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L350-L353), [`crates/civ/src/ic7300.rs:166-168`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L166-L168). **Never loosen.** The IC-7300's TOT does not cover keying from the KEY jack, so on the keyer box the box's own limits stop a stuck key: [`docs/keyer.md:193-195`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L193-L195). On NR7Y's CW firmware the UV-K1's own time-out timer does not work in CW, because the firmware clears it on every key-down (the stock UV-K1 has no CW mode): [`firmware/uv-k1/README.md:31-33`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/uv-k1/README.md#L31-L33).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md), [13 Safety rules (never loosen)](13-safety-rules.md).

### Transmit inhibit

**Definition.** In this project, a latch that stops all transmitting until a person has checked the radio. It is kept in a file, so a crash or a restart does not clear it.

**In this project.** When the node cannot confirm the radio is back on receive, or a fault repeats, it latches the inhibit and writes the file `tx-inhibited` in the state directory. While that file exists nothing is transmitted, across restarts. Only removing it, once the radio has been checked, clears it: [`crates/hfnode/src/station.rs:11-20`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L11-L20), [`crates/hfnode/src/station.rs:528-532`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L528-L532). `InhibitLatch` covers the stop paths outside a `Station`: [`crates/hfnode/src/station.rs:440-472`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L440-L472). Two faults in a row with no good transmission between latch it: [`crates/hfnode/src/station.rs:324-329`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L324-L329). When it latches, the IC-7300 also gets break-in off and its own TX Inhibit function (`16 66`) on: [`crates/civ/src/lib.rs:177-185`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/lib.rs#L177-L185), [`crates/civ/src/ic7300.rs:145-152`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/ic7300.rs#L145-L152). **Never loosen.**

**Covered in.** [05 Node loop and station safety](05-node-loop-station-safety.md).

### Trip

**Definition.** In this project, a latched fault in the keyer box. A tripped box keys nothing until it is power-cycled.

**In this project.** `keyer_core::keyer::Trip` lists the trips: `Down` (the key or tone stayed on past the key-down limit), `Pin` (the firmware saw a pin on past its limit), `Slow` (a slow control-loop pass with the key on), `Clock` (the loop's clock disagreed with the processor's count), `Watchdog` (the box restarted because its watchdog fired), `Ptt` (PTT limit) and `Line` (something else is holding the PTT line). A trip survives a restart that is not a power-up: [`crates/keyer-core/src/keyer.rs:164-190`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L164-L190). PR #31 (draft, not on main, untested on hardware) adds `Core1`, for core 1 faulting or not moving its [heartbeat](#heartbeat) within `CORE1_MS = 100` ms: [`crates/keyer-core/src/keyer.rs:194-197`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/crates/keyer-core/src/keyer.rs#L194-L197), [`crates/keyer-core/src/lib.rs:104-110`](https://github.com/robinonsay/cw-message-gateway/blob/bc5062ae4f56ae1f9949b3b067218c7e675d88f8/crates/keyer-core/src/lib.rs#L104-L110). **Never loosen** (rule 1.19 in [13 Safety rules (never loosen)](13-safety-rules.md)). A tripped box answers `CW` and `MCW` with the error code `TRIP`: [`crates/keyer-core/src/keyer.rs:34-36`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L34-L36).

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

## U

### UF2

**Definition.** A file format for flashing microcontrollers over USB mass storage. The file is a series of 512-byte blocks, each carrying 256 bytes of data for one flash address and the ID of the chip family it is for, so the boot ROM can write it to flash as it is copied.

**In this project.** `firmware/pico2-keyer/uf2.py` turns the built ELF into `pico2-keyer.uf2` for family `RP2350_ARM_S`: [`firmware/pico2-keyer/uf2.py:2-19`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/uf2.py#L2-L19), [`firmware/pico2-keyer/uf2.py:30-35`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/uf2.py#L30-L35). CI builds and uploads it: [`.github/workflows/ci.yml:211-216`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/.github/workflows/ci.yml#L211-L216). See [BOOTSEL](#bootsel) and [Flash file](#flash-file).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [02 Codebase, build and CI](02-codebase-build-ci.md).

### UV-K1

**Definition.** The Quansheng UV-K1, a cheap VHF/UHF FM handheld. Like other FM handhelds it has no CW mode of its own: it transmits while its PTT is held and sends whatever reaches its microphone.

**In this project.** It is the test radio for trying the system on 2 m before the IC-7300. It is driven through its [K-plug](#k-plug) by the keyer box, either by MCW on the stock firmware or as a straight key on [NR7Y's CW firmware](#nr7y-firmware): [`docs/keyer.md:444-452`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L444-L452), [`docs/keyer.md:840-847`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L840-L847). The `station.rig = "handheld"` route over USB-C is parked: [`crates/hfnode/src/handheld.rs:1-6`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/handheld.rs#L1-L6), [`docs/handheld.md:1-12`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/handheld.md#L1-L12).

**Covered in.** [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

## V

### Verdict

**Definition.** A decision with a fixed set of outcomes. The project uses the word for two unrelated things.

**In this project.**

- **The audit's verdicts** (project files, `audit/rubric.md`): for each action (flash, connect, key at the bench, run unattended), GO only if every criterion is PASS, or BENCH or RISK with its step done; one FAIL is NO-GO. `audit/final-grades.md`, the file Robin acts on, writes them as GO, GO with steps (safe only if Robin does the listed steps herself, in order, at the radio), NO-GO, not yet (unattended use, graded only after her bench results, [H1](#h1)) or n/a. Not found in the code at `44185fa`.
- **The reply filter's verdict**: `gateway::filter::Verdict`, the decision on one inbound message before the node keys it, with an action of `Keep`, `Redact` or `Drop`: [`crates/hfnode/src/gateway/filter.rs:65-78`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/gateway/filter.rs#L65-L78).

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [06 Gateways and messages](06-gateways-messages.md).

### VFO

**Definition.** Variable-frequency oscillator: on a modern radio, one of its frequency memories that the tuning knob changes. Most HF radios have two, A and B. With "split" on, the radio receives on one and transmits on the other.

**In this project.** The node must transmit only on its listening frequency. The preflight fails if split is on, because the radio "would transmit on the other VFO": [`crates/civ/src/preflight.rs:293-302`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/civ/src/preflight.rs#L293-L302). The station checks split, ∂TX and the transmit frequency again before every transmission: [`crates/hfnode/src/station.rs:36-39`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L36-L39).

**Covered in.** [07 CI-V and the IC-7300](07-civ-ic7300.md).

## W

### Watchdog

**Definition.** A hardware countdown timer that resets the chip when it reaches zero. Working software reloads ("feeds") it regularly. Software that hangs stops feeding it, and the reset puts the system back into a known state: [`firmware/pico2/src/watchdog.rs:1-15`](https://github.com/robinonsay/rustos/blob/80bf5998a60a41b3741041062ff7e9c59caf95b2/firmware/pico2/src/watchdog.rs#L1-L15) (rustos master).

**In this project.** The keyer box's control loop must feed the watchdog every `WATCHDOG_MS = 500` ms: [`crates/keyer-core/src/lib.rs:61-62`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L61-L62), [`firmware/pico2-keyer/src/main.rs:216`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/firmware/pico2-keyer/src/main.rs#L216). **Never loosen.** A box that comes back from a watchdog reset starts tripped (`Trip::Watchdog`): [`crates/keyer-core/src/keyer.rs:406-409`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/keyer.rs#L406-L409). The host also has a software watchdog, a thread that forces the radio to receive if a keying run lasts longer than `station.max_key_seconds`: [`crates/hfnode/src/station.rs:21-24`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/station.rs#L21-L24), [`crates/hfnode/src/config.rs:70-73`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L70-L73). It defaults to 45 s and must be 1-120 s ([`crates/hfnode/src/config.rs:746-748`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L746-L748), [`crates/hfnode/src/config.rs:945-946`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L945-L946)); `run` accepts at most `RUN_MAX_KEY_SECONDS = 55`, below the hardware transmit timer's 60 s ([`crates/hfnode/src/commissioning.rs:121-125`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/commissioning.rs#L121-L125)). **Never loosen** (rules 5.1 and 5.2 in [13 Safety rules (never loosen)](13-safety-rules.md)). The table at the top lists the other meanings of the word.

**Covered in.** [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md), [05 Node loop and station safety](05-node-loop-station-safety.md), and rustos page 04 (Drivers: board, clocks, time, watchdog).

### WPM

**Definition.** Words per minute, the speed of Morse code, measured with the standard word "PARIS" (50 units). At `wpm` words per minute one dot lasts `1200 / wpm` ms, so 20 wpm is a 60 ms dot.

**In this project.** The decoder estimates the sender's speed as `1200 / dit_ms`, starting at 15 and tracking 5 to 35 wpm: [`crates/cw/src/decoder.rs:65-76`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/decoder.rs#L65-L76), [`crates/cw/src/decoder.rs:327-329`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/cw/src/decoder.rs#L327-L329). The node keys at `station.key_speed_wpm` (default 18): [`crates/hfnode/src/config.rs:67-69`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L67-L69), [`crates/hfnode/src/config.rs:743-745`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/hfnode/src/config.rs#L743-L745). The keyer box accepts 5 to 50 wpm: [`crates/keyer-core/src/lib.rs:38-40`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/keyer-core/src/lib.rs#L38-L40).

**Covered in.** [04 CW decoder and audio](04-cw-decoder-audio.md), [08 Keyer box: firmware and protocol](08-keyer-box-firmware.md).

## Z

### Zero-RF

**Definition.** In this project, a bench stage in which nothing can be transmitted: no radio-frequency power leaves the radio. Zero-RF stages prove the wiring and the software before any keyed stage.

**In this project.** On a UV-K1 on NR7Y CW, the radio's break-in (`CWbkin`) is OFF for every zero-RF stage, so a key-down plays the sidetone and transmits nothing: [`docs/keyer.md:1168-1171`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1168-L1171). The radio's own antenna (or a dummy load) stays on even then, in case a fault keys the radio: [`docs/keyer.md:1304-1308`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/docs/keyer.md#L1304-L1308). The audit grades "Connect, zero RF" as its own action (`audit/final-grades.md`, project files). Not found in the code at `44185fa`.

**Covered in.** [14 Audit, bench and flashing](14-audit-bench-flashing.md), [10 Quansheng UV-K1 handheld](10-uv-k1-handheld.md).

## Changing it

The glossary changes whenever a term is added or a pull request merges. To change it:

1. Add a term in alphabetical order under its letter, with the three parts (definition, in this project, covered in) and a permalink to the line that defines it. Add a new letter to "Jump to" if needed.
2. Add any word with more than one meaning to the table at the top.
3. Any safety number added must carry **Never loosen** and agree with [13 Safety rules (never loosen)](13-safety-rules.md); cite the code, never a memory note.
4. Code only on an open pull request is cited at that PR's head and labelled "(PR #N, not on main)", with "untested on hardware" where that is true.
5. Re-run `python3 -I /home/claude/tools/citecheck.py docs/handover/17-glossary.md` from the repository root until it reports 0 broken.

## Known bugs, limits and flakes

This page has no code of its own, so it has no bugs. Four naming hazards trip up new readers:

- **"Stage 0" is ambiguous.** `bench/quansheng.md`'s stage 0 (code on main matches what the audit passed) and `bench/ic7300.md`'s stage 0 (the code on the host) are not `docs/keyer.md`'s "stage zero" (a meter on the key line), and none of them is a config bring-up [stage](#stage). Always say which list you mean.
- **"Verdict" in the code is not the audit's verdict.** `gateway::filter::Verdict` is the reply filter's decision. The audit's verdicts (GO, GO with steps, NO-GO, not yet, n/a) exist only in the project files.
- **"Piece" and "chunk" are different units.** A piece is at most 30 characters, the radio keyer's limit. A chunk is a lettered part of a reply, by default at most 60 characters, sent as one or more pieces. Beware: `protocol::reply::Chunk`'s doc comment and its `pieces()` helper say "piece" for a chunk ([`crates/protocol/src/reply.rs:72-75`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L72-L75), [`crates/protocol/src/reply.rs:93-94`](https://github.com/robinonsay/cw-message-gateway/blob/44185fa9a00369c379d9d8af40ca307e3867e860/crates/protocol/src/reply.rs#L93-L94)); on these pages "piece" always means the ≤30-character keying unit (`civ::MAX_CW_CHARS`, `keyer_core::MAX_TEXT`).
- **"Watchdog" has three meanings** (the box's hardware watchdog, the host's keying watchdog, the parked UV-K1 firmware's watchdog), and none of them is the hardware transmit timer of test-plan step 10.

Some code comments and docs on `main` are older than the audit's on-air ruling: the bring-up stage comments and the keyer box's parts list still say "dummy load" (see [Dummy load](#dummy-load)).

For the project's real bugs and limits, see [16 Work left and known bugs](16-work-left-known-bugs.md).

## Work left

- When PR #30 and PR #31 merge, re-cite the entries labelled with them (ADC, ADC_VREF, AGND, core 0 and core 1, CP0, decoder box, DMA, flash file, Goertzel, GP26, heartbeat, MPU, MSPLIM, one-Pico image, Secure and Non-secure, SECCFG, trip `Core1`) at the merge commit, and remove the "not on main" labels.
- When PR #27 merges, re-cite the [Preflight](#preflight) entry's front-key and MIC Up/Down Keyer checks at the merge commit.
- When rustos PR #6 merges and the firmware is re-pinned at master's merge, re-cite the multicore entries and the [rustos](#rustos) pin at the new commits.
- The project-wide list is [16 Work left and known bugs](16-work-left-known-bugs.md).

## Sources

- cw-message-gateway at `main` `44185fa`: `crates/keyer-core`, `crates/cw`, `crates/protocol`, `crates/auth`, `crates/civ` (`lib.rs`, `ic7300.rs`, `preflight.rs`, `mock.rs`, `sim.rs`, `frame.rs`), `crates/hfnode` (`main.rs`, `config.rs`, `commissioning.rs`, `station.rs`, `session.rs`, `storm.rs`, `selftest.rs`, `selftest/any_radio.rs`, `keyer.rs`, `keyer/`, `handheld.rs`, `handheld/mock.rs`, `gateway/filter.rs`), `firmware/pico2-keyer`, `firmware/uv-k1/README.md`, `.github/workflows/ci.yml`, `README.md`, [`docs/keyer.md`](../keyer.md), [`docs/keyer-protocol.md`](../keyer-protocol.md), [`docs/handheld.md`](../handheld.md), [`docs/hardware-test-plan.md`](../hardware-test-plan.md), [`docs/raspberry-pi-setup.md`](../raspberry-pi-setup.md).
- cw-message-gateway PR #27 at `3c97427` (`crates/civ/src/preflight.rs`), PR #30 at `3efaf84` (`docs/decoder.md`, `firmware/pico2-decoder/src/main.rs`, `crates/cw/src/level.rs`) and PR #31 at `bc5062a` (`firmware/pico2-keyer/src/core1.rs`, `firmware/pico2-keyer/src/main.rs`, `firmware/pico2-keyer/Cargo.toml`, `crates/keyer-core/src/lib.rs`, `crates/keyer-core/src/keyer.rs`).
- rustos master at `80bf599` (`Cargo.toml`, `api/src/lib.rs`, `firmware/pico2/src/`: `watchdog.rs`, `gpio/gpio.rs`, `adc.rs`, `dma.rs`, `pwm.rs`, `usb/mod.rs`), the firmware's pin `8c91507` (compared with master), and rustos PR #6 at `410dffa` (`firmware/pico2/src/multicore.rs`).
- Project files (not public, named only): `design/spec.md`, `design/spec-pico-decoder.md`, `audit/rubric.md`, `audit/final-grades.md`, `audit/report.md`, `audit/req-on-air-bench-ruling.md`, `audit/req-handheld-nr7y-cw-ruling.md`, `bench/quansheng.md`, `bench/ic7300.md`, `reference/IC-7300_ENG_FM_12b.txt` (∂TX, p. 4-11).
- The project's memory notes, checked against the code above.
