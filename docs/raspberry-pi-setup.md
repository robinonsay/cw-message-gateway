# Raspberry Pi setup

This sets up a Raspberry Pi as the node: the `hfnode` binary, the radio's serial
port and USB audio, the IC-7300 menu settings, secrets, and a systemd service.

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

Listening windows are computed from UTC, so a wrong clock means the node listens at
the wrong time. The Pi has no battery-backed clock; it needs the network at boot to
get the time.

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

The example config uses `plughw:CARD=CODEC,DEV=0`, which names the card instead of
its number, so it survives reboots and other USB audio devices. If your card has a
different name, use the `plughw:CARD=...,DEV=0` line that `arecord -L` prints for
it. Record a few seconds to check (tune the radio to a CW signal or to band noise
first):

```sh
arecord -D plughw:CARD=CODEC,DEV=0 -f S16_LE -r 8000 -c 1 -d 10 /tmp/test.wav
hfnode decode /tmp/test.wav --pitch 600
```

Put the device name in `audio.device`.

## 6. IC-7300 settings

Set these on the radio and write down what you set. Unless noted they are under
**MENU > SET > Connectors** (IC-7300 Full Manual, pp. 12-10 and 12-11). Item names
can differ slightly between firmware versions; check each against the manual for
yours. The node never changes any of these over CI-V. Before it writes anything to
the radio it reads the transmit-related ones and refuses to go on if one is wrong;
`hfnode radio --config <file> check` prints what it reads, without writing.

**CI-V:**

| Item | Set to | Why |
|---|---|---|
| CI-V Address | **94h** (default) | Must equal `station.civ_address` (02h to DFh). Instructions written for the IC-7300MK2 use B6h; do not copy them. |
| CI-V USB Port | **Unlink from [REMOTE]** (default) | The USB port works independently of the rear REMOTE jack. The two settings below only apply in this mode. |
| CI-V USB Baud Rate | **115200** | Must equal `station.baud`. Set it explicitly rather than Auto. |
| CI-V USB Echo Back | **OFF** (default) | The driver skips its own echoed frames, so ON also works; OFF is less traffic. |
| CI-V Transceive | **OFF** (default is ON) | Stops the radio sending unsolicited status frames whenever a setting changes. The driver ignores frames not addressed to it, so ON also works. |
| USB Serial Function | **CI-V** (default) | The USB serial port must carry CI-V, not decoded RTTY. |

**Transmit control over USB, keep OFF:**

| Item | Set to | Why |
|---|---|---|
| USB SEND | **OFF** (default) | Set to DTR or RTS, that serial control line puts the radio on transmit (p. 12-11). Opening a serial port raises both lines (section 4). The node does not use them, and refuses to write to the radio unless this is OFF. |
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
| USB MOD Level, DATA OFF MOD | Leave as is | The node does not transmit audio. |

**CW** (in CW mode, from the Multi-function menu):

| Item | Set to | Why |
|---|---|---|
| CW PITCH | **600 Hz** | Must equal `audio.pitch_hz`. The decoder looks for the tone here. |
| BKIN D (break-in delay) | Leave to the node | Holds transmit between characters. The node sets it from `station.break_in_delay_dots` (default 10.0) at start-up and reads it back. |
| Break-in | Leave to the node | The node turns semi break-in on with CI-V at start-up (command 17 only transmits with break-in on), and reads it back. It never selects full break-in. |
| Dot/Dash Ratio (MENU > KEYER > EDIT/SET > CW-KEY SET) | **1:1:3.0** (default) | Standard Morse timing for the field operator's ear and decoder. The node warns if it is anything else. |
| KEY jack | Nothing plugged in | With break-in on, anything on the KEY jack keys the transmitter. Unplug paddles for unattended use. |

**Transmit backstop** (MENU > SET > Function, p. 12-5):

| Item | Set to | Why |
|---|---|---|
| Time-Out Timer (CI-V) | **3 min** (shortest option) | The radio ends a transmission "initiated by a CI-V command or pushing TRANSMIT" after this long (p. 12-5). The manual does not say whether CW keyed with command 17 counts, so it backs up, and does not replace, the node's watchdog (`max_key_seconds`) and the external hardware PTT timer. `hfnode run` refuses to start while it is OFF. |

**Display** (MENU > SET > Display, p. 12-12):

| Item | Set to | Why |
|---|---|---|
| Meter Peak Hold | **OFF** (default is ON) | The node reads the Po and SWR meters over CI-V to check each transmission. The manual does not say whether those readings are the held peak or the present value; OFF removes the doubt. The node warns while it is ON. |

**Tuner emergency mode** (MENU > SET > Others > Emergency, p. 11-4):

| Item | Set to | Why |
|---|---|---|
| Tuner | **Not ticked** (default) | In emergency mode the internal tuner keeps working into an SWR above 3:1. Normally it gives up and bypasses itself, which the node sees and then stays silent for that listening window. |

**On the main screen:** SPLIT off and XIT (∂TX) off. With either on, the radio would
transmit somewhere other than the frequency the node set; the node refuses to
write to the radio unless both read OFF.

**Power and tuner:** the node sets RF power to `station.power_watts` (30-50 W per
the design; start bench tests at 10 W) and runs the internal tuner at start-up and
at the start of each listening window. Leave the tuner switched on.

## 7. Configuration

```sh
sudo cp hfnode.example.toml /etc/hfnode/hfnode.toml
sudo chown root:hfnode /etc/hfnode/hfnode.toml && sudo chmod 0640 /etc/hfnode/hfnode.toml
sudo nano /etc/hfnode/hfnode.toml
```

At minimum set `station.node_call`, `station.field_calls`, `station.frequency_hz`,
`station.serial_port`, `audio.device`, the `[[contacts]]`, `[email]` and
`[weather]`. The file is checked on load; unknown keys are errors, and
`power_watts` (1-100), `max_key_seconds` (1-120) and `swr_limit` (1.1-3.0) are range
checked.

Keep `state_dir = "/var/lib/hfnode"` and `key_file = "/etc/hfnode/node.key"` to match
the systemd unit. The node creates `last_seq`, `inbox.json`, `rx.log` and
`health.csv` in `state_dir`.

`max_key_seconds` (default 45) must be shorter than the hardware PTT timer.

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
- Without the API key (or if the API cannot be reached), inbound messages stay
  unscreened and are never transmitted; the node logs a warning and keeps running.
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

On start you should see `last_seq is N`, `preflight:` lines (the read-only radio
checks), `read-back:` lines, a `health: tune ...` line, and
`N0CALL listening on 7030000 Hz`. The unit:

- runs `/usr/local/bin/hfnode run --config /etc/hfnode/hfnode.toml` as `hfnode`, with
  `dialout` and `audio` as supplementary groups;
- loads `/etc/hfnode/env`;
- restarts on failure after 30 s, and gives up after 5 failures in 10 minutes so a
  broken radio connection does not turn into an endless loop of start-up tunes;
- runs `hfnode radio ... rx` after every stop or crash, to make sure the radio is on
  receive;
- only allows the process to open USB serial (`ttyUSB`) and ALSA devices, and
  write only to `/var/lib/hfnode`.

If you use a udev symlink or by-id path, it still resolves to a `ttyUSB` device,
so the device allow-list covers it.

**Stopping the node.** `sudo systemctl stop hfnode`. If the node was keying when it
was stopped, the radio may finish the text already handed to its keyer (at most 30
characters) before the stop hook forces receive.

**Updating.** `sudo systemctl stop hfnode`, install the new binary, `sudo systemctl
start hfnode`. `last_seq` and the inbox are kept in `/var/lib/hfnode`.
