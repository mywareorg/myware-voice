# Working agreement

## Branches

- Work on `main`. There is one developer; a tag marks what is releasable, not a
  branch. `dev` is retired.

## Commits

- Imperative subject, no prefix, no trailing period, under ~72 characters.
- The body says what changed and why; call out known gaps explicitly rather than
  letting the commit imply more than it delivers.
- Commit and push to `main` without being asked once a change is ready. There is
  no review step to wait for.

## Running

- `scripts/reload-daemon.sh` rebuilds the daemon and restarts the one that is
  running. Run it after any change to the daemon's code; a change to
  `config.toml` alone is picked up on the next idle tick, because the daemon
  reloads it when the stamp changes.

- `cargo lint` runs clippy over every target, including the settings window and
  the integration tests. Run it before committing; it passes clean today, and
  `cargo lint-strict` is the same command with `-D warnings`.

## This app

A background daemon, not a window. The compositor owns the keybind; the daemon
owns the microphone, the model and the clipboard. The daemon must never take
focus, or paste lands in the wrong place.

Rules that have already cost time elsewhere:

- **Pin the native ABI.** `sherpa.lock` names one version. The struct layouts
  change between releases, and a mismatch is undefined behaviour that looks like
  a random crash. `size_of` assertions live in `src/sherpa/mod.rs`; `doctor`
  prints the version actually loaded.
- **Never type the transcript.** Copy, then send a paste chord. Typing is seconds
  of visible keystrokes and races the user.
- **Never inject Enter.** A trailing newline pasted into a terminal runs a
  command. Strip it before copying.
- **Start capture before loading the model.** The user is already talking by the
  time a 1.5 s load finishes.
- **Measure, then decide.** `transcribe --json` exists so latency and RSS
  decisions are numbers, not guesses.

## Scope

Linux targets sway/Wayland first; X11 is not a goal until someone needs it.
Windows compiles and is type-checked for `x86_64-pc-windows-msvc`, but has never
been run, so treat its runtime behaviour as unproven until someone exercises it
on a real machine. Development stays on Linux; Windows is the test target, and
`scripts/run-windows.ps1` is how a checkout starts the tray daemon there.
