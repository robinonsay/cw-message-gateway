#!/bin/sh
# Runs the node in a Terminal window, kept going by hfnode-supervise.sh (restarts
# after a failure, at most 3 starts an hour; puts the radio on receive after every
# stop; keeps the Mac awake). Double-click it, or add it to System Settings >
# General > Login Items to start it at log-in. Stop it with Ctrl-C in its window.
# See docs/macos-setup.md.
#
# Running in Terminal means the microphone permission macOS asks for (needed to
# hear the radio's USB sound card) is Terminal's, granted once.
dir="$HOME/Library/Application Support/hfnode"
exec /bin/sh "$dir/hfnode-supervise.sh" "$HOME/.cargo/bin/hfnode" "$dir/hfnode.toml" "$dir/env"
