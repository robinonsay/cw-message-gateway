# Windows setup

This sets up a Windows 10 or 11 PC as the node: building `hfnode`, the radio's COM
port and USB audio, the files it keeps, secrets, and starting it at log-on.

What has been checked, and what has not: `hfnode` builds, passes its tests and runs
the self-test against the mock radio on Windows in CI (GitHub's Windows runner,
x86-64), and the start-up script is tested there with a stand-in for `hfnode`.
Nothing here has been run on Windows with an IC-7300 connected. The driver, the
device names, the microphone setting, and what Ctrl-C does to the script in a
console window come from Microsoft's and Silicon Labs' documentation, not from a
test. The [hardware test plan](hardware-test-plan.md) is how they get checked.

Before putting the node on the air, work through the hardware test plan. Register
the start-up task only after it passes.

## 1. Build and install `hfnode`

Install, accepting the defaults:

- **Visual Studio Build Tools** (the free "Build Tools for Visual Studio"), with
  the "Desktop development with C++" workload. Rust needs its linker and the
  Windows SDK, and the TLS library compiles some C.
- **Rust**, with `rustup-init.exe` from rustup.rs.
- **Git for Windows**.

Then, in a new PowerShell window. `-b main` matters while the repository's default
branch is still the old one:

```powershell
git clone -b main https://github.com/robinonsay/ic-7300-hf-server.git
cd ic-7300-hf-server
cargo test --workspace
cargo install --path crates/hfnode      # installs %USERPROFILE%\.cargo\bin\hfnode.exe
hfnode selftest                         # the whole node against a mock radio
```

## 2. The node's folder

Everything the node keeps goes in `%LOCALAPPDATA%\hfnode`:

```powershell
$D = "$env:LOCALAPPDATA\hfnode"
New-Item -ItemType Directory -Force "$D\state" | Out-Null
Copy-Item hfnode.example.toml "$D\hfnode.toml"
hfnode keygen --out "$D\node.key"
```

Files under `%LOCALAPPDATA%` are readable only by you, SYSTEM and administrators
(`icacls "$D\node.key"` shows who). Back the key up somewhere offline (a USB stick
kept at home). Anyone with it can print valid codes; never carry it in the field,
only the printed table.

In `hfnode.toml` point the node at that folder. **Use single quotes for Windows
paths**: TOML takes them literally, while in double quotes a backslash starts an
escape and the file will not load. `~` is your user folder:

```toml
state_dir = '~\AppData\Local\hfnode\state'

[auth]
key_file = '~\AppData\Local\hfnode\node.key'
```

## 3. Find the radio's COM port

The IC-7300's USB serial chip is a Silicon Labs CP210x. Windows Update normally
installs its driver when the radio is first connected and turned on; if not,
install ICOM's IC-7300 USB driver or Silicon Labs' "CP210x Universal Windows
Driver". Then:

```powershell
hfnode devices
```

`hfnode devices` only lists; it opens nothing, so it cannot affect the radio. The
radio's port is marked `<- the IC-7300` or `<- a CP210x bridge, as in the IC-7300`.
Device Manager shows the same under Ports (COM & LPT), as "Silicon Labs CP210x USB
to UART Bridge (COM3)". Put the name in `station.serial_port`, for example
`serial_port = "COM3"`. Windows normally keeps that number for the radio; re-run
`hfnode devices` if the port is not found.

**DTR and RTS.** The IC-7300 can be set to transmit on either serial control line.
`hfnode` opens the port with both lines set to off, clears them again straight
after, and will not use the port if it cannot; before writing anything to the radio
it reads USB SEND and both USB Keying items and refuses unless they are OFF; and the
radio's Inhibit Timer at USB Connection covers the moment of opening. Whether the
CP210x driver raises the lines for an instant when the port opens is not
documented. A COM port can be open in one program at a time, so quit WSJT-X,
fldigi, flrig and similar programs before starting `hfnode`, and do not start them
while it runs.

## 4. Find the radio's USB audio

`hfnode devices` also lists audio inputs. The IC-7300's recording device has `USB
Audio CODEC` in its name (Windows usually calls it something like "Microphone (USB
Audio CODEC)" or "Microphone (2- USB Audio CODEC)"), and `audio.device` defaults to
`USB Audio CODEC`, which matches any of those. If there are two such devices, use
the full name `hfnode devices` shows. `hfnode` reads the device in its own format
(usually 48000 Hz) and converts.

Windows treats every recording device as a microphone. Under Settings > Privacy &
security > Microphone, turn on **Microphone access** and **Let desktop apps access
your microphone**. Without them `hfnode` hears silence and logs a warning after 3 s.
Check with a recording (tune the radio to a CW signal or band noise first):

```powershell
hfnode record --config "$D\hfnode.toml" --out "$HOME\Desktop\radio.wav" --seconds 10
hfnode decode "$HOME\Desktop\radio.wav" --pitch 600
```

In Sound settings, keep the PC's output on its own speakers, not on the USB Audio
CODEC, so system sounds do not go to the radio.

## 5. IC-7300 settings and the config

Set the radio's menu items exactly as in [raspberry-pi-setup.md, section
6](raspberry-pi-setup.md#6-ic-7300-settings); they do not depend on the computer.
Then edit `hfnode.toml` as in [section 7](raspberry-pi-setup.md#7-configuration)
there, with the COM port, audio device and folder paths from above. Keep
`station.commissioned = "none"` until the hardware test plan sets it.

## 6. Secrets

Passwords and API keys never go in the TOML file. Put them in `env` in the node's
folder:

```powershell
notepad "$D\env"
```

```
HFNODE_EMAIL_PASSWORD=app-password-for-the-node-mailbox
ANTHROPIC_API_KEY=sk-ant-...
```

One `KEY=value` per line, no quotes. The start-up script reads the file without
running it. To set one for a command run by hand (for example `sim` without
`--offline`): `$env:HFNODE_EMAIL_PASSWORD = "..."`. What happens when a secret is
missing is in [raspberry-pi-setup.md, section 8](raspberry-pi-setup.md#8-secrets).

## 7. Starting the node

`hfnode run` refuses to start until `station.commissioned = "done"`.
`deploy\windows\hfnode-supervise.ps1` does what the Pi's systemd unit does: restarts
the node 30 s after a failure and gives up after 3 starts in an hour (so a radio
that is off or unplugged does not get an endless loop of start-up tunes), does not
restart it after a clean stop, runs `hfnode radio ... rx` after every stop or
crash, loads `env`, and keeps the PC from sleeping while it runs.

To start it at log-on, from the repository folder:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File deploy\windows\install-task.ps1
Start-ScheduledTask -TaskName hfnode     # or log off and on
```

This registers a Task Scheduler task, `hfnode`, that opens a console window when
you log on and runs the node there, with its log in the window. It runs as you, in
your session, the way the microphone setting above expects; running it as a Windows
service is not supported. So the PC must be logged in. To run it once without the
task:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File deploy\windows\hfnode-supervise.ps1 `
    -Hfnode "$env:USERPROFILE\.cargo\bin\hfnode.exe" -Config "$D\hfnode.toml" -EnvFile "$D\env"
```

On start you should see `last_seq is N`, `preflight:` lines, `read-back:` lines, a
`health: tune ...` line and `N0CALL listening on 7030000 Hz`.

**Staying awake.** The script keeps Windows from sleeping on its own while it
runs, but not from a laptop's lid: set "When I close the lid" to "Do nothing"
(Control Panel > Power Options > Choose what closing the lid does), and keep it on
the charger. A sleep takes the radio's USB devices away.

**Clock.** Listening windows are computed from UTC, so keep "Set time
automatically" on (Settings > Time & language > Date & time) and press "Sync now"
once.

## 8. Stopping, updating, and the transmit inhibit

**Stopping.** Press Ctrl-C in the node's window. The node stops the radio's keyer
and confirms receive before it exits; the radio may first finish the text already
in its keyer (at most 30 characters). The script then checks receive again.
Closing the window also stops the node, but Windows ends the script within a few
seconds, before it can check receive again, so use Ctrl-C. Without the window (for example from another session):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File deploy\windows\stop-hfnode.ps1
```

which ends the task and the node and then runs `hfnode radio ... rx`. To remove the
task: `Unregister-ScheduledTask -TaskName hfnode`.

**If it stops transmitting.** When the node cannot confirm the radio is back on
receive, it stops transmitting and writes `tx-inhibited` in
`%LOCALAPPDATA%\hfnode\state` with the time and reason. It keeps running and
logging but transmits nothing, also after a restart, until that file is removed.
Check the radio first.

**Updating.** Stop the node, then in the repository folder: `git pull`,
`cargo install --path crates/hfnode`, and start it again. `last_seq` and the inbox
stay in the node's folder.

**Moving the node to or from another computer.** Copy the whole node folder,
`state\last_seq` above all: without it the node starts again from sequence 0 and
would accept codes from the table that were already used. Then set
`station.commissioned = "none"` and redo the hardware test plan from step 1, since
the computer changed.
