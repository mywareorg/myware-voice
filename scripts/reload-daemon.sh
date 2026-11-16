#!/usr/bin/env bash
# Rebuild the daemon and restart the one that is running.
#
# A change to `config.toml` alone does not need this: the daemon reloads the
# file on its idle tick when the stamp changes. A change to the code does, and
# forgetting it means the old binary is what hears the microphone.
#
# The restart is detached on purpose. Run from a shell that is about to exit,
# a plain `&` would take the daemon with it, and the next keypress would find
# nothing listening.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --quiet --bin myware-voice-daemon

# `quit` is the socket command, so it also waits for a graceful stop; when no
# daemon is running it exits non-zero, which is the state we wanted anyway.
./target/debug/myware-voice quit >/dev/null 2>&1 || true

for _ in $(seq 1 25); do
    if ! pgrep -f 'target/debug/myware-voice-daemon' >/dev/null; then
        break
    fi
    sleep 0.2
done

if pgrep -f 'target/debug/myware-voice-daemon' >/dev/null; then
    echo "reload-daemon: the old daemon did not exit" >&2
    exit 1
fi

setsid nohup ./target/debug/myware-voice-daemon >/dev/null 2>&1 </dev/null &
sleep 1

# The point of the script is that the thing that is listening is the thing that
# was just built, so say so rather than leaving it ambiguous.
exec ./target/debug/myware-voice status
