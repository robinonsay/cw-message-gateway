#!/bin/sh
# Keeps `hfnode run` going on macOS (started by launchd, see
# deploy/macos/io.github.robinonsay.hfnode.plist and docs/macos-setup.md), or on a
# Unix system without systemd. It does what deploy/hfnode.service has systemd do:
#
# - restarts the node 30 s after it fails, and gives up after 3 starts within an
#   hour, so that a radio that is off, unplugged or misbehaving does not get an
#   endless loop of start-up tunes (each of which transmits);
# - does not restart it after a clean stop (exit status 0);
# - after every stop or crash runs `hfnode radio ... rx`, which stops the keyer and
#   makes sure the radio is on receive (with the keyer box, that its key is open);
# - on a stop signal (launchd sends SIGTERM) passes it on to the node, which puts the
#   radio on receive itself, then runs the receive check above and exits;
# - loads secrets from an environment file (KEY=value lines, no quotes, mode 0600);
# - on macOS keeps the computer from going to sleep while it runs (caffeinate).
#
# Usage: hfnode-supervise.sh HFNODE_BINARY CONFIG_FILE [ENV_FILE]
#
# Do not set this to start automatically until docs/hardware-test-plan.md has been
# worked through; `hfnode run` refuses to start before station.commissioned = "done".

set -u

bin=${1:?usage: hfnode-supervise.sh HFNODE_BINARY CONFIG_FILE [ENV_FILE]}
cfg=${2:?usage: hfnode-supervise.sh HFNODE_BINARY CONFIG_FILE [ENV_FILE]}
env_file=${3:-}

# The same limits as the systemd unit; settable for tests.
restart_sec=${HFNODE_RESTART_SEC:-30}
burst=${HFNODE_START_LIMIT_BURST:-3}
interval=${HFNODE_START_LIMIT_INTERVAL:-3600}
stop_timeout=${HFNODE_STOP_TIMEOUT:-20}

log() {
    echo "$(date '+%Y-%m-%d %H:%M:%S') hfnode-supervise: $*"
}

if [ -n "$env_file" ]; then
    if [ ! -r "$env_file" ]; then
        log "cannot read $env_file"
        exit 1
    fi
    # Read KEY=value lines without running anything in the file. A file saved
    # with Windows line endings has a CR at the end of each line; drop it.
    cr=$(printf '\r')
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line%"$cr"}
        case $line in
            '' | '#'*) continue ;;
            *=*) ;;
            *)
                log "ignoring a line in $env_file that is not KEY=value"
                continue
                ;;
        esac
        key=${line%%=*}
        case $key in
            '' | [0-9]* | *[!A-Za-z0-9_]*)
                log "ignoring a line in $env_file that is not KEY=value"
                continue
                ;;
        esac
        export "$key=${line#*=}"
    done <"$env_file"
fi

# Keep the Mac awake (no idle or system sleep) for as long as this script runs.
if command -v caffeinate >/dev/null 2>&1; then
    caffeinate -i -s -w $$ &
fi

child=
killer=
woken=0
stopping=0
on_stop() {
    woken=1
    stopping=1
    if [ -n "$child" ]; then
        kill -TERM "$child" 2>/dev/null
    fi
}
trap on_stop TERM INT HUP

# Put the radio on receive, whatever state the node left it in. Failing here (radio
# off, port gone) is not an error.
force_receive() {
    if "$bin" radio --config "$cfg" rx; then
        log "radio confirmed on receive"
    else
        log "could not confirm the radio is on receive; check it"
    fi
}

# Wait for the node to exit; its status is left in $status. A trapped signal
# interrupts `wait`, so wait again; once stopping, send SIGKILL if the node has not
# gone within $stop_timeout seconds.
wait_child() {
    while :; do
        if [ "$stopping" = 1 ] && [ -z "$killer" ]; then
            (
                sleep "$stop_timeout"
                log "hfnode did not stop within $stop_timeout s; killing it"
                kill -KILL "$child" 2>/dev/null
            ) &
            killer=$!
        fi
        woken=0
        wait "$child"
        status=$?
        [ "$woken" = 1 ] || break
    done
    if [ -n "$killer" ]; then
        kill "$killer" 2>/dev/null
        killer=
    fi
}

starts=
while :; do
    now=$(date +%s)
    recent=
    count=0
    for t in $starts; do
        if [ $((now - t)) -lt "$interval" ]; then
            recent="$recent $t"
            count=$((count + 1))
        fi
    done
    if [ "$count" -ge "$burst" ]; then
        log "gave up: $count starts within $interval s. Fix the cause (see the log above), then start it again."
        exit 1
    fi
    starts="$recent $now"

    if [ "$stopping" = 1 ]; then
        log "stopped"
        exit 0
    fi
    log "starting: $bin run --config $cfg"
    "$bin" run --config "$cfg" &
    child=$!
    # A stop that came in since the check above found no node to pass the signal
    # to; pass it on now.
    if [ "$stopping" = 1 ]; then
        kill -TERM "$child" 2>/dev/null
    fi
    wait_child
    child=
    log "hfnode exited with status $status"
    force_receive

    if [ "$stopping" = 1 ]; then
        log "stopped"
        exit 0
    fi
    if [ "$status" = 0 ]; then
        log "clean stop; not restarting"
        exit 0
    fi
    log "restarting in $restart_sec s"
    sleep "$restart_sec" &
    sleeper=$!
    wait "$sleeper"
    kill "$sleeper" 2>/dev/null
    if [ "$stopping" = 1 ]; then
        log "stopped"
        exit 0
    fi
done
