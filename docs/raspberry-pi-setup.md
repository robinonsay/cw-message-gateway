# Raspberry Pi setup

This sets up a Raspberry Pi as the node: the `hfnode` binary, the radio's serial
port and USB audio, the IC-7300 menu settings, secrets, and a systemd service. Other
Linux machines work the same way. For a Mac see [macos-setup.md](macos-setup.md),
for Windows [windows-setup.md](windows-setup.md); both refer back here for the
radio's menu settings (section 6).

Before putting the node on the air, work through the
[hardware test plan](hardware-test-plan.md). Enable the service only after it passes.

## 1. Operating system

Install **Raspberry Pi OS (64-bit)**, Lite is enough, with Raspberry Pi Imager. In
the imager's settings, set a hostname, a user, SSH, and Wi-Fi or plan on Ethernet.
A Pi 4 or Pi 5 is comfortable; a Pi 3B with the 64-bit OS works but builds slowly.

Set the clock to sync (it is on by default with `systemd-timesyncd`) and check:

```sh
timedatectl
```

If you set the node to listen in windows (`schedule.always = false`), they are
computed from UTC, so a wrong clock means the node listens at the wrong time, and
tunes (a short carrier, then its callsign) at the wrong time too. Listening all the
time, the default, does not depend on the clock. The Pi has no battery-backed clock;
it needs the network at boot to get the time. Make the service wait until the clock
has actually synchronised, not just until the time service has started:

```sh
sudo systemctl enable systemd-time-wait-sync.service
```

Update and install the audio tools (`hfnode` runs `arecord` for capture):

```sh
sudo apt update && sudo apt full-upgrade -y
sudo apt install -y alsa-utils
```

## 2. Get the `hfnode` binary

**Option A: build on the Pi.**

```sh
sudo apt install -y build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # accept the defaults
source ~/.cargo/env
git clone https://github.com/robinonsay/ic-7300-hf-server.git
cd ic-7300-hf-server
cargo test --workspace
cargo build --release -p hfnode
sudo install -m 0755 target/release/hfnode /usr/local/bin/hfnode
```

**Option B: cross-compile and copy.** Build for `aarch64-unknown-linux-gnu` on
another machine as described in the [README](../README.md#build-and-test), then:

```sh
scp target/aarch64-unknown-linux-gnu/release/hfnode pi@hfnode.local:/tmp/
ssh pi@hfnode.local sudo install -m 0755 /tmp/hfnode /usr/local/bin/hfnode
```

Check it runs: `hfnode --version`.

## 3. Service user, directories, key

The node runs as its own system user, in the `dialout` group (serial port) and the
`audio` group (sound card):

```sh
sudo useradd --system --home-dir /var/lib/hfnode --shell /usr/sbin/nologin \
  --groups dialout,audio hfnode
sudo install -d -o hfnode -g hfnode -m 0750 /var/lib/hfnode
sudo install -d -o root -g hfnode -m 0750 /etc/hfnode
```

Also add your own login user to the same groups for bench testing (log out and in
again afterwards):

```sh
sudo usermod -aG dialout,audio "$USER"
```

The bench tests use `/var/lib/hfnode` as their state directory too, so that a
transmit inhibit a test latches also stops the node: the [hardware test
plan](hardware-test-plan.md#before-you-start) has you make it yours for the tests
and give it back to `hfnode` afterwards.

Create the secret key. It must be readable by the `hfnode` user and nobody else:

```sh
sudo hfnode keygen --out /etc/hfnode/node.key     # created with mode 0600
sudo chown hfnode:hfnode /etc/hfnode/node.key
```

Back the key up somewhere offline (for example on a USB stick kept at home).
Anyone with the key can print valid codes. Never carry the key in the field; carry
only the printed table.

## 4. Find the radio's serial port

Connect the IC-7300's USB port to the Pi and turn the radio on. The radio has a
Silicon Labs CP210x USB-to-serial bridge, which Linux drives with the built-in
`cp210x` module:

```sh
sudo dmesg | grep -i cp210x          # "cp210x converter now attached to ttyUSB0"
ls -l /dev/ttyUSB* /dev/serial/by-id/
hfnode devices                       # marks the radio's port, and its by-id name
```

`/dev/ttyUSB0` is fine while it is the only USB serial device, but the number can
change if you add another one. Two stable alternatives:

- **The by-id path**, which the system creates for you, for example
  `/dev/serial/by-id/usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_IC-7300_03001234-if00-port0`.
  Use the exact name `ls` shows.
- **A udev symlink by serial number.** Find the serial number:

  ```sh
  udevadm info -a -n /dev/ttyUSB0 | grep -E 'ATTRS\{(idVendor|idProduct|serial)\}' | head -3
  ```

  Then create `/etc/udev/rules.d/99-ic7300.rules` with your values:

  ```
  SUBSYSTEM=="tty", ATTRS{idVendor}=="10c4", ATTRS{idProduct}=="ea60", ATTRS{serial}=="IC-7300 03001234", SYMLINK+="ic7300", GROUP="dialout", MODE="0660"
  ```

  and reload: `sudo udevadm control --reload && sudo udevadm trigger`. The port is
  then `/dev/ic7300`.

Put whichever path you choose in `station.serial_port`.

Any program that opens a serial port raises its DTR and RTS lines, and the IC-7300
can be set to transmit on either (section 6). `hfnode` lowers them straight after
opening, but other programs do not, so keep the radio's port to `hfnode` alone:
ModemManager probes new serial ports and brltty claims CP210x bridges. Raspberry
Pi OS Lite installs neither; check with `systemctl status ModemManager brltty` and
`sudo apt purge modemmanager brltty` if they are there.

## 5. Find the radio's USB audio device

The IC-7300 also presents a USB sound card. List capture devices:

```sh
arecord -l     # card numbers and names; look for "USB Audio CODEC"
arecord -L     # ALSA device names; look for plughw:CARD=CODEC,DEV=0
```

`audio.device` defaults to `plughw:CARD=CODEC,DEV=0`, which names the card instead
of its number, so it survives reboots and other USB audio devices. If your card has a
different name, use the `plughw:CARD=...,DEV=0` line that `arecord -L` prints for
it. Record a few seconds to check (tune the radio to a CW signal or to band noise
first):

```sh
arecord -D plughw:CARD=CODEC,DEV=0 -f S16_LE -r 8000 -c 1 -d 10 /tmp/test.wav
hfnode decode /tmp/test.wav --pitch 600
```

If it differs from the default, put the device name in `audio.device` (the line
is commented out in the example config). Once the config is in place,
`hfnode record --config <file> --out /tmp/test.wav` records the same way, as the
node hears it, and prints the peak level.

## 6. IC-7300 settings

Set these on the radio and write down what you set. Unless noted they are under
**MENU > SET > Connectors** (IC-7300 Full Manual, pp. 12-10 and 12-11). Item names
can differ slightly between firmware versions; check each against the manual for
yours. The node never changes any of these over CI-V. Before it writes anything to
the radio it reads the transmit-related ones and refuses to go on if one is wrong
(`hfnode radio rx` excepted: it only stops the keyer and switches to receive);
`hfnode radio --config <file> check` prints what it reads, without writing.

**CI-V:**

| Item | Set to | Why |
|---|---|---|
| CI-V Address | **94h** (default) | Must equal `station.civ_address` (02h to DFh). Instructions written for the IC-7300MK2 use B6h; do not copy them. |
| CI-V USB Port | **Unlink from [REMOTE]** (default) | The USB port works independently of the rear REMOTE jack, so no other controller's replies can be mistaken for the radio's. The two settings below only apply in this mode. The node refuses to write to the radio if it is linked. |
| CI-V USB Baud Rate | **115200** | Must equal `station.baud`. Set it explicitly rather than Auto. |
| CI-V USB Echo Back | **OFF** (default) | The driver skips its own echoed frames, so ON also works; OFF is less traffic. |
| CI-V Transceive | **OFF** (default is ON) | Stops the radio sending unsolicited status frames whenever a setting changes. The driver ignores frames not addressed to it, so ON also works. |
| USB Serial Function | **CI-V** (default) | The USB serial port must carry CI-V, not decoded RTTY. |
| CI-V Output (for ANT) | **OFF** (default) | ON sends the radio's status (frequency and so on) unasked for an antenna controller (p. 12-10). The node warns while it is ON. |

**Transmit control over USB, keep OFF:**

| Item | Set to | Why |
|---|---|---|
| USB SEND | **OFF** (default) | Set to DTR or RTS, that serial control line puts the radio on transmit (p. 12-11). Opening a serial port raises both lines (section 4). The node does not use them, and refuses to write to the radio unless this is OFF (but for `radio rx`, which only sends the stop and receive commands). |
| USB Keying (CW) | **OFF** (default) | Same: a DTR or RTS line would hold the CW key down. The node keys with CI-V command 17 instead, and refuses to write to the radio unless this is OFF. |
| USB Keying (RTTY) | **OFF** (default) | Same: a DTR or RTS line would key RTTY (FSK). Checked like the two above. |
| Inhibit Timer at USB Connection | **ON** (default) | When the USB connection is made, delays a SEND or Keying signal by a few seconds (p. 12-11). It only delays it, so the three items above must still be OFF. |

**USB audio:**

| Item | Set to | Why |
|---|---|---|
| ACC/USB Output Select | **AF** (default) | The decoder needs audio, not the 12 kHz IF. |
| ACC/USB AF Output Level | Start at 50% (default) and adjust | Receive audio to the Pi. Adjust so the strongest signals do not clip (test plan, step 2). |
| ACC/USB AF SQL | **OFF (OPEN)** (default) | The decoder needs audio all the time, not gated by squelch. |
| ACC/USB AF Beep/Speech... Output | **OFF** (default) | Keeps beeps and voice announcements out of the decoder. |
| USB MOD Level | Leave as is | The node does not transmit audio. |
| DATA OFF MOD, DATA MOD | **MIC,ACC** and **ACC** (defaults), not USB | With USB in either, sound the computer plays to the radio is transmitted in that mode (p. 12-10). The node sends no audio; it warns if either includes USB. |

**CW** (in CW mode, from the Multi-function menu):

| Item | Set to | Why |
|---|---|---|
| CW PITCH | **600 Hz** | Must equal `audio.pitch_hz`. The decoder looks for the tone here. |
| BKIN D (break-in delay) | Leave to the node | Holds transmit between characters and words. The node sets it to 10.0 dots (fixed, not a config key: 3 dots longer than a word gap, so the radio stays on transmit for a whole keyer message) at start-up and at the start of each listening window, and reads it back at start-up. |
| Break-in | Leave to the node | The node turns semi break-in on with CI-V at start-up (command 17 only transmits with break-in on), and reads it back. It never selects full break-in. |
| Dot/Dash Ratio (MENU > KEYER > EDIT/SET > CW-KEY SET) | **1:1:3.0** (default) | Standard Morse timing for the field operator's ear and decoder; the node times its keying at 1:1:3.0. The node refuses to write to the radio if it is anything else. |
| KEY jack | Nothing plugged in | With break-in on, anything on the KEY jack keys the transmitter. Unplug paddles for unattended use. |

**Transmit backstop and tuner** (MENU > SET > Function, p. 12-5):

| Item | Set to | Why |
|---|---|---|
| PTT Start (Tuner) | **OFF** (default) | ON starts a tuner cycle, which transmits, when PTT is pushed after the frequency has moved more than 1% (p. 12-5, lines 6310-6315). The node never needs it, and refuses to write to the radio unless it is OFF. |
| Time-Out Timer (CI-V) | **3 min** (shortest option) | The radio ends a transmission "initiated by a CI-V command or pushing TRANSMIT" after this long (p. 12-5). The manual does not say whether CW keyed with command 17 counts, so it backs up, and does not replace, the node's watchdog (`max_key_seconds`) and the external hardware PTT timer. `radio tune`, `radio cw` and `hfnode run` refuse to start unless it is 3 min. |
| VOX (VOX/BK-IN key) | **OFF** | With VOX ON, sound at the microphone transmits (p. 4-10). The node refuses to write to the radio unless it is off. |
| TX Inhibit (CI-V `16 66`; no menu item) | **OFF** | While ON the radio "cannot transmit" (p. 13-6). An IC-PW2 amplifier sets it. `radio tune`, `radio cw` and `hfnode run` refuse to start while it is ON; `radio check` and `radio setup` warn. |

**Display** (MENU > SET > Display, p. 12-12):

| Item | Set to | Why |
|---|---|---|
| Meter Peak Hold | **OFF** (default is ON) | The node reads the Po and SWR meters over CI-V to check each transmission. The manual does not say whether those readings are the held peak or the present value; OFF removes the doubt. The node warns while it is ON. |

**Tuner emergency mode** (MENU > SET > Others > Emergency, p. 11-4):

| Item | Set to | Why |
|---|---|---|
| Tuner | **Not ticked** (default) | In emergency mode the internal tuner keeps working into an SWR above 3:1. Normally it gives up and bypasses itself, which the node sees and then stays silent until its next tune. |

**Scope data output** (command `27 11`, p. 19-14; panadapter programs turn it on): OFF.
With it ON the radio streams waveform data to the port the node uses, which slows
the node's stop commands. Close any panadapter program before starting the node;
the node refuses to write to the radio while it is ON.

**On the main screen:** SPLIT off and XIT (∂TX) off. With either on, the radio would
transmit somewhere other than the frequency the node set; the node refuses to
write to the radio unless both read OFF.

**Power and tuner:** the node sets RF power to `station.power_watts` (30-50 W per
the design; start bench tests at 10 W) and runs the internal tuner at start-up, at
the start of each listening window if it uses them, and before a reply once the last
tune is over an hour old. After the tune at start-up or at a window's start, if it
matches, it sends its callsign (`DE <node_call>`) in CW to identify the carrier; a
tune before a reply is identified by the reply. Leave the tuner switched on.

## 7. Configuration

```sh
sudo cp hfnode.example.toml /etc/hfnode/hfnode.toml
sudo chown root:hfnode /etc/hfnode/hfnode.toml && sudo chmod 0640 /etc/hfnode/hfnode.toml
sudo nano /etc/hfnode/hfnode.toml
```

At minimum set `station.node_call`, `station.field_calls`, `station.frequency_hz`,
`station.serial_port`, `audio.device`, the `[[contacts]]`, `[email]` (with
`alert_to`, an address you read often: the node emails it if it stops transmitting)
and `[weather]`, with a `[[weather.presets]]` entry for each place you often key
from (`WX 1`, `WX 2`, ...). The file is checked on load; unknown keys are errors,
and `power_watts` (1-100), `max_key_seconds` (1-120) and `swr_limit` (1.1-3.0) are
range checked.

Keep `state_dir = "/var/lib/hfnode"` and `key_file = "/etc/hfnode/node.key"` to match
the systemd unit. The node creates `last_seq`, `inbox.json`, `rx.log`,
`health.csv` and `wx_last.json` (the last weather place each field callsign
confirmed, used for `WX` alone) in `state_dir`, `google_voice.json` with
`[google_voice]` set, and `tx-inhibited` if it stops transmitting (section 10).

To reach contacts by text from a Google Voice number as well as by email, see
[texting.md](texting.md). (iMessage needs a Mac.)

`max_key_seconds` (default 45) must be shorter than the hardware PTT timer.

`run` also refuses to start without a `[storm]` section giving the station's latitude
and longitude (the storm stand-down; see `hfnode.example.toml`). Check that it can
read the forecast (it prints `clear:` or `storm:`; an error means the node would
never transmit):

```sh
sudo -u hfnode /usr/local/bin/hfnode storm --config /etc/hfnode/hfnode.toml
```

`station.commissioned` starts at `"none"`: `hfnode run`, and so the service, refuses
to start until the [hardware test plan](hardware-test-plan.md#bring-up-stages) has
been worked through on this radio and it is set to `"done"`, and any `power_watts`
above 10 is refused before `"keying"`. Copy the value from the bench config once the
plan has passed.

## 8. Secrets

Passwords and API keys never go in the TOML file. The config names the environment
variables to read (`email.password_env`, default `HFNODE_EMAIL_PASSWORD`, and
`filter.api_key_env`, default `ANTHROPIC_API_KEY`). systemd loads them from
`/etc/hfnode/env`:

```sh
sudo install -m 0600 -o root -g root /dev/null /etc/hfnode/env
sudo nano /etc/hfnode/env
```

```
HFNODE_EMAIL_PASSWORD=app-password-for-the-node-mailbox
ANTHROPIC_API_KEY=sk-ant-...
```

systemd reads this file as root before dropping to the `hfnode` user, so it can
stay root-only. Use a dedicated mailbox for the node and an app password, not your
personal account password.

What happens if a secret is missing:

- With `[email]` configured, `hfnode run` refuses to start without the email password.
  The alert to `email.alert_to` goes out through the same mail server and password.
- Without the API key (or if the API cannot be reached), inbound messages stay
  unscreened and are never transmitted; the node logs a warning and keeps running.
  With `filter.provider = "ollama"` no API key is needed; see
  [reply-filter.md](reply-filter.md).
  Setting `filter.enabled = false` transmits third-party text unscreened, which the
  design advises against.

To run a command by hand as the service user with the secrets loaded (for
example `sim` without `--offline`, which really sends email), use `systemd-run`:

```sh
sudo systemd-run --pty --quiet --uid=hfnode --gid=hfnode \
  -p SupplementaryGroups="dialout audio" -p EnvironmentFile=/etc/hfnode/env \
  /usr/local/bin/hfnode sim --config /etc/hfnode/hfnode.toml
```

## 9. Print the first code table

```sh
sudo -u hfnode hfnode codes --config /etc/hfnode/hfnode.toml --count 100
```

Run it as the `hfnode` user so it reads the real `last_seq`. See
[operating.md](operating.md#the-code-table) for how to carry and use it.

## 10. systemd service

```sh
sudo cp deploy/hfnode.service /etc/systemd/system/hfnode.service
sudo systemctl daemon-reload
sudo systemctl enable --now hfnode
journalctl -u hfnode -f
```

On start you should see `if transmitting is inhibited, you@example.com is emailed`
(or a warning that it is only logged, without `alert_to`), `last_seq is N`,
`preflight:` lines (the read-only radio checks), `read-back:` lines, `N0CALL
listening on 7030000 Hz`, `listening`, a `health: tune NNNms` line and `health:
swr ...` (from the `DE N0CALL` after the tune). With listening windows
(`schedule.always = false`) the node tunes only when a window opens, so
`listening window open`, the tune and the ID follow at once if it started inside a
window, otherwise at the next one (up to 50 minutes later with windows of 10
minutes every hour). The unit:

- runs `/usr/local/bin/hfnode run --config /etc/hfnode/hfnode.toml` as `hfnode`, with
  `dialout` and `audio` as supplementary groups;
- loads `/etc/hfnode/env`;
- restarts on failure after 30 s, and gives up after 3 starts in an hour, so a
  broken radio connection does not turn into an endless loop of start-up tunes
  (`sudo systemctl reset-failed hfnode` before starting it again by hand);
- runs `hfnode radio ... rx` after every stop or crash, to make sure the radio is on
  receive;
- only allows the process to open USB serial (`ttyUSB`) and ALSA devices, and
  write only to `/var/lib/hfnode`.

If you use a udev symlink or by-id path, it still resolves to a `ttyUSB` device,
so the device allow-list covers it.

**If it stops transmitting.** When the node cannot confirm the radio is back on
receive (the radio off or unplugged at the top of a window, or stuck on transmit
after a fault), or the radio reads receive while its Po meter shows output, it
stops transmitting and writes `/var/lib/hfnode/tx-inhibited` with the time and the
reason. It keeps running and decoding, but it does not tune or key, so the field
operator hears nothing, and this lasts across restarts. With `[email] alert_to`
set it emails that address once when this happens (subject `N0CALL: node stopped
transmitting (tx-inhibited)`), and once more each time the service starts while the
file is there; the email gives the reason and these steps. Without `alert_to` it is
only in the journal. To clear it:

```sh
sudo systemctl stop hfnode
sudo -u hfnode hfnode radio --config /etc/hfnode/hfnode.toml check   # read-only; must pass
sudo cat /var/lib/hfnode/tx-inhibited                                # the time and the reason
sudo rm /var/lib/hfnode/tx-inhibited
sudo systemctl reset-failed hfnode                                   # only if systemd gave up
sudo systemctl start hfnode
```

Check the radio before deleting the file. Deleting it while the node runs changes
nothing: the node reads it only when it starts.

**Check the alert.** Make the node start inhibited and see the email arrive.
Nothing is transmitted while the file is there:

```sh
echo "$(date +%s) alert check by hand" | sudo -u hfnode tee /var/lib/hfnode/tx-inhibited
sudo systemctl restart hfnode
journalctl -u hfnode -n 20     # look for: emailed the transmit-inhibit alert
```

Then clear it as above.

**Stopping the node.** `sudo systemctl stop hfnode`. The node stops the radio's
keyer and confirms receive before it exits, and the unit's stop hook then checks
receive again. If the node was keying, the radio may first finish the text already
handed to its keyer (at most 30 characters).

**Using the radio yourself.** Stop the node first, and start it again
(`sudo systemctl start hfnode`) when you are done. While it runs, it puts its
frequency, mode, power and keyer settings back every `schedule.check_minutes` (10)
and before every transmission.

**Updating.** `sudo systemctl stop hfnode`, install the new binary, `sudo systemctl
start hfnode`. `last_seq` and the inbox are kept in `/var/lib/hfnode`.
