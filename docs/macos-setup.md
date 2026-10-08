# macOS setup

This sets up a Mac as the node instead of a Raspberry Pi: building `hfnode`, the
radio's serial port and USB audio, the files it keeps, secrets, and starting it at
log-in. Apple Silicon and Intel Macs both work.

What has been checked, and what has not: `hfnode` builds, passes its tests and runs
the self-test against the mock radio on macOS in CI (GitHub's macOS runner, Apple
Silicon), and the Intel build is compile-checked there. Nothing in this guide has
been run on a Mac with an IC-7300 connected yet; the port names, the microphone
prompt and the start-up items below come from Apple's and Silicon Labs'
documentation and source, not from a test. The [hardware test
plan](hardware-test-plan.md) is how they get checked.

Before putting the node on the air, work through the hardware test plan. Set it to
start at log-in only after it passes.

## 1. Build and install `hfnode`

Install Apple's command line tools (compiler and linker; full Xcode is not needed)
and Rust:

```sh
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # accept the defaults
source "$HOME/.cargo/env"
```

Get the code and build it. `-b main` matters while the repository's default branch
is still the old one:

```sh
mkdir -p ~/rust && cd ~/rust
git clone -b main https://github.com/robinonsay/ic-7300-hf-server.git ic7300-hf-server
cd ic7300-hf-server
cargo test --workspace
cargo install --locked --path crates/hfnode   # installs ~/.cargo/bin/hfnode
hfnode selftest                               # the whole node against a mock radio
```

`cargo install` puts `hfnode` on your `PATH` (rustup added `~/.cargo/bin` to it).
`hfnode selftest` takes a minute or so and should end with every scenario `PASS`.
It runs the radio 100 times faster than real time, so a Mac busy with other work can
fail a scenario on timing; then run it slower, `hfnode selftest --scale 20` (and
`HFNODE_E2E_SCALE=20 cargo test --workspace` for the tests). Check that the build
is for your Mac's processor: `file ~/.cargo/bin/hfnode` says `arm64` on Apple
Silicon and `x86_64` on Intel.

Use `git clone` as above rather than a ZIP download from GitHub, so that `git pull`
updates it later.

## 2. The node's folder

Everything the node keeps goes in one folder:

```sh
D="$HOME/Library/Application Support/hfnode"
mkdir -p "$D/state" && chmod 700 "$D"
cp hfnode.example.toml "$D/hfnode.toml"
cp deploy/hfnode-supervise.sh deploy/macos/hfnode.command "$D/"
hfnode keygen --out "$D/node.key"        # created with mode 0600
```

| File | What it is |
|---|---|
| `hfnode.toml` | The config. |
| `node.key` | The secret key. Back it up somewhere offline (a USB stick kept at home). Anyone with it can print valid codes; never carry it in the field, only the printed table. |
| `env` | Passwords and API keys (section 6). |
| `state/` | `last_seq`, `inbox.json`, `rx.log`, `health.csv`, `wx_last.json`, and `tx-inhibited` if the node ever stops transmitting (section 8). With texting set up, also `google_voice.json`, `imessage.json` and `imessage_windows.json` ([texting.md](texting.md#files-in-state_dir)). |
| `hfnode-supervise.sh`, `hfnode.command` | The start-up scripts (section 7). |

In `hfnode.toml` point the node at that folder (`~` is your home folder):

```toml
state_dir = "~/Library/Application Support/hfnode/state"

[auth]
key_file = "~/Library/Application Support/hfnode/node.key"
```

## 3. Find the radio's serial port

Connect the IC-7300's USB port to the Mac, turn the radio on, and list the ports:

```sh
hfnode devices
```

`hfnode devices` only lists; it opens nothing, so it cannot affect the radio. The
radio's port looks like `/dev/cu.usbserial-...` and is marked `<- the IC-7300` (its
USB serial number contains "IC-7300") or `<- a CP210x bridge, as in the IC-7300`.
Put that path in `station.serial_port`:

- Use the **`/dev/cu.`** name, not `/dev/tty.`; `hfnode devices` lists only `cu`
  names, and `hfnode` warns if the config has a `tty` one.
- The suffix comes from the radio's USB serial number or the USB socket it is in,
  so plug the radio into the same socket each time and re-run `hfnode devices` if
  the port is not found.

macOS has its own driver for the radio's Silicon Labs CP210x USB-serial chip, so
normally nothing needs installing. If no port shows up with the radio on, install
Silicon Labs' "CP210x USB to UART Bridge VCP" driver for macOS and approve it under
System Settings > Privacy & Security; the port is then `/dev/cu.SLAB_USBtoUART`.

**DTR and RTS.** macOS raises the port's DTR and RTS lines whenever a program
opens it (read in Apple's IOSerialFamily source; not yet measured on this radio),
and the IC-7300 can be set to transmit on either. `hfnode` lowers them straight
after opening and will not use the port if it cannot; before writing anything to
the radio (but for `radio rx`, which only stops the keyer and switches to receive)
it reads USB SEND and both USB Keying items and refuses unless they are OFF, which
is what makes the lines harmless. (The radio's Inhibit Timer at USB
Connection, left ON, also holds off a signal for a few seconds when a port opens.)
Other programs do not lower the lines, so keep the radio's port to `hfnode` alone:
quit WSJT-X, fldigi, flrig and similar programs before starting it.

Opening the port within two seconds of the last program closing it can take up to
two seconds: macOS holds DTR down that long between uses. That is normal.

## 4. Find the radio's USB audio

`hfnode devices` also lists audio inputs. The IC-7300's shows as `USB Audio CODEC`
(marked `<- the IC-7300's USB codec`), which is the default `audio.device` on a Mac,
so normally you leave it. If you have two such devices, use the full name
`hfnode devices` shows. `hfnode` reads the device at its own sample rate and
converts to `audio.sample_rate`; nothing needs setting in Audio MIDI Setup.

**Microphone permission.** macOS treats every audio input, the radio's included,
as a microphone, and asks once per app. Run the first recording from Terminal (tune
the radio to a CW signal or band noise first):

```sh
hfnode record --config "$D/hfnode.toml" --out ~/radio.wav --seconds 10
hfnode decode ~/radio.wav --pitch 600
```

Answer **Allow** to "Terminal would like to access the microphone". The grant is
Terminal's, so it covers every `hfnode` run in Terminal, including the start-up
item in section 7. If you said no, or the prompt never appeared, macOS gives
`hfnode` silence instead of an error; `hfnode record` says "silence, no audio is
reaching hfnode", and the node logs a warning after 3 s. Turn Terminal on under
System Settings > Privacy & Security > Microphone and try again.

Set the Mac's sound output (System Settings > Sound) to its own speakers, not the
USB Audio CODEC, so system sounds do not go to the radio, and do not use the codec
in other audio programs while the node runs.

## 5. IC-7300 settings and the config

Set the radio's menu items exactly as in [raspberry-pi-setup.md, section
6](raspberry-pi-setup.md#6-ic-7300-settings); they do not depend on the computer.
Then edit `hfnode.toml` as in [section 7](raspberry-pi-setup.md#7-configuration)
there, with the serial port, audio device and folder paths from above. Keep
`station.commissioned = "none"` until the hardware test plan sets it.

To reach contacts by text or iMessage as well as email, set up `[google_voice]`
and `[imessage]` as in [texting.md](texting.md).

Fill in `[storm]` with your home's latitude and longitude (`run` refuses to start
without it), then check the node can read the forecast. It prints `clear:` or
`storm:`; an error means the node would never transmit:

```sh
hfnode storm --config "$D/hfnode.toml"
```

## 6. Secrets

Passwords and API keys never go in the TOML file. Put them in `env` in the node's
folder, readable only by you:

```sh
touch "$D/env" && chmod 600 "$D/env"
open -e "$D/env"
```

```
HFNODE_EMAIL_PASSWORD=app-password-for-the-node-mailbox
ANTHROPIC_API_KEY=sk-ant-...
```

One `KEY=value` per line, no quotes and no `export`. The start-up script reads the
file without running it. To load it the same way for a command run by hand (for
example `sim` without `--offline`):

```sh
while IFS= read -r l; do case $l in ''|'#'*) ;; *=*) export "$l" ;; esac; done < "$D/env"
```

With `filter.provider = "ollama"` (a model run by Ollama on this Mac or another
computer) leave out `ANTHROPIC_API_KEY`; see [reply-filter.md](reply-filter.md).
To check the filter before the node goes live, run
`hfnode filter --config "$D/hfnode.toml" test` (with Claude, load `env` first as
above).

What happens when a secret is missing is in [raspberry-pi-setup.md, section
8](raspberry-pi-setup.md#8-secrets).

## 7. Starting the node

`hfnode run` refuses to start until `station.commissioned = "done"`. Both ways
below use `hfnode-supervise.sh`, which does what the Pi's systemd unit does:
restarts the node 30 s after a failure and gives up after 3 starts in an hour (so a
radio that is off or unplugged does not get an endless loop of start-up tunes),
does not restart it after a clean stop, runs `hfnode radio ... rx` after every stop
or crash, loads `env`, and keeps the Mac awake while it runs.

**In Terminal (recommended).** Double-click `hfnode.command` in the node's folder
(Finder: Go > Go to Folder, `~/Library/Application Support/hfnode`). It opens a
Terminal window and runs the node there, with the log in the window. To start it
at log-in, add `hfnode.command` under System Settings > General > Login Items >
Open at Login. Running in Terminal is what makes the microphone permission
dependable, and it is the only way the node can use iMessage: the permissions for
Messages are Terminal's too ([texting.md](texting.md#imessage-mac-only)).

On start you should see `last_seq is N`, `preflight:` lines, `read-back:` lines, a
`health: tune ...` line and `N0CALL listening on 7030000 Hz`.

**As a launchd agent (no window).** `deploy/macos/io.github.robinonsay.hfnode.plist`
runs the same script in the background and logs to `~/Library/Logs/hfnode.log`:

```sh
mkdir -p ~/Library/LaunchAgents
sed "s|__HOME__|$HOME|g" deploy/macos/io.github.robinonsay.hfnode.plist \
  > ~/Library/LaunchAgents/io.github.robinonsay.hfnode.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.robinonsay.hfnode.plist
```

The microphone permission is the weak point here: the agent asks for itself, not
through Terminal, and macOS may never show the prompt for a program that is not an
app, or may forget the grant when `hfnode` is rebuilt. Check the log for the
silence warning and that `hfnode` decodes what it hears. The agent cannot use
iMessage: it sets `HFNODE_LAUNCHD=1`, and the node then logs that iMessage is not
available and sends by Google Voice or email. Stop it with
`launchctl bootout gui/$(id -u)/io.github.robinonsay.hfnode`; macOS 13 and later
also list it under Login Items > Allow in the Background.

Do not run it as a LaunchDaemon (in `/Library/LaunchDaemons`): a daemon cannot be
given microphone access, so it would hear nothing.

**Staying awake and logged in.** Both ways need you logged in. The script keeps
the Mac from sleeping on its own while it runs (`caffeinate`), but closing a
MacBook's lid still sleeps it unless an external display is connected, and a sleep
takes the radio's USB devices away. Keep it on the power adapter. After a power
cut the Mac only comes back by itself with "Start up automatically after a power
failure" on (Energy settings, desktop Macs) and automatic log-in, which FileVault
does not allow. Turn off "Install macOS updates" under System Settings > General >
Software Update > Automatic Updates, so that an update does not restart the Mac
and leave the node stopped.

**Clock.** If you set the node to listen in windows (`schedule.always = false`),
they are computed from UTC, so keep "Set time and date automatically" on (System
Settings > General > Date & Time). Listening all the time, the default, does not
depend on the clock.

## 8. Stopping, updating, and the transmit inhibit

**Stopping.** Ctrl-C in the Terminal window (or `launchctl bootout` for the
agent). The node stops the radio's keyer and confirms receive before it exits; the
radio may first finish the text already in its keyer (at most 30 characters). The
script then checks receive again. Closing the window sends the same stop, but
you cannot see whether receive was confirmed, so prefer Ctrl-C.

**Using the radio yourself.** Stop the node first, and start it again when you
are done. While it runs, it puts its frequency, mode, power and keyer settings back
every `schedule.check_minutes` (10) and before every transmission, and it reads the
radio's transmit status every second: a transmission it did not start (you keying
the radio) stops it transmitting until you clear `tx-inhibited` as below.

**If it stops transmitting.** When the node cannot confirm the radio is back on
receive, it stops transmitting and writes
`~/Library/Application Support/hfnode/state/tx-inhibited` with the time and reason.
It keeps running and logging but transmits nothing, also after a restart, until
that file is removed. Check the radio first, then run `hfnode radio --config <config> setup`: the
node also turns the radio's TX Inhibit on (and its semi break-in off) when it
stops transmitting, `setup` turns TX Inhibit off once the file is gone, and `run`
refuses to start while it is on. With `[email] alert_to` set, the node
emails you the reason and the steps to clear it when this happens, and at each
start while the file is there.

**Updating.** Stop the node, then in `~/rust/ic7300-hf-server`: `git pull`,
`cargo install --locked --path crates/hfnode`, copy the start-up scripts again
(`cp deploy/hfnode-supervise.sh deploy/macos/hfnode.command "$D/"`), and start it
again. `last_seq` and the inbox stay in the node's folder. With iMessage set up,
start it with `hfnode.command` and check its log says `iMessage ready`: after an
update macOS may ask again for Terminal to control Messages, so run `hfnode
messages --config "$D/hfnode.toml" check` in Terminal once if it does not.

**Moving the node to or from another computer.** First stop the old node and turn
off its start-up (on a Pi `sudo systemctl disable --now hfnode`, on a Mac its Login
Item or launchd agent, on Windows `Unregister-ScheduledTask -TaskName hfnode`).
Then copy the whole node folder from it, `state/last_seq` above all: without it the
node starts again from sequence 0 and would accept codes from the table that were
already used. Copy it at that moment, not from a backup, Migration Assistant or an
earlier copy, which would hold an older `last_seq`; before the first `hfnode run`,
check that `last_seq` is at least the last line crossed off the printed table. Then
set `station.commissioned = "none"` and redo the hardware test plan from step 1,
since the computer changed. `state/imessage.json` belongs to the old Mac's Messages:
on a new Mac expect one `rescanning` warning, then run `hfnode messages --config
"$D/hfnode.toml" check`.
