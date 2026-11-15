# Hippo Dictate

Push-to-talk dictation. Press the keybind, speak, press it again: the transcript
goes to the clipboard and is pasted into the window you were typing in.

Local-only. The audio never leaves the machine; there are no accounts, no
telemetry, and no call leaves the machine after the model is installed. The
optional Ollama client talks to loopback only.

**Status:** works end to end on sway/Wayland, including streaming dictation
(`hippo-dictate stream`). The Windows build compiles and is type-checked but has
not been run.

## What it uses

- **Speech** — NVIDIA Parakeet TDT 0.6B v3, int8, exported for
  [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx). 25 languages, punctuation
  and capitalisation included, no separate punctuation model.
- **Runtime** — sherpa-onnx 1.13.8, loaded at run time from two shared objects.
  Nothing is linked at build time.
- **Audio** — `cpal` for capture, `rubato` to reach the 16 kHz mono the model
  expects.

Parakeet TDT v3 needs sherpa-onnx **>= 1.12.15**; the export landed in that
release. The version is pinned in `sherpa.lock` because the C struct layouts
differ between releases — the bindings assert their sizes against the pinned ABI.

## Build and run

```bash
./scripts/fetch-sherpa.sh     # 27 MB runtime -> vendor/sherpa
./scripts/fetch-model.sh      # 640 MB model    -> vendor/models

cargo build
./target/debug/hippo-dictate doctor
./target/debug/hippo-dictate transcribe some.wav
```

`doctor` prints the resolved runtime, the model, the session type, whether the
focused window can be read, the licence tier, and which of `wl-copy`, `wtype`,
`ydotool` and `notify-send` it found.

### Developing with the keybind live

An installed copy and a checkout do not share a binary, so a rebuild changes
nothing until the keybind stops pointing at the installed one:

```bash
./scripts/dev-link.sh          # ~/.local/bin -> target/debug
cargo build --workspace
hippo-dictate quit             # replace the daemon that is still running
```

`toggle` spawns a daemon when none is listening, so the next press starts the new
build — `quit` is the entire restart, and there is no daemon to start by hand.
`scripts/install.sh` points the same symlinks back at a release install.

Two consequences worth knowing: a rebuild on Linux can replace the binary while
the old daemon keeps running, so `quit` is what makes the new code take effect
rather than an error; and on Windows `cargo build` fails while the daemon holds
the executable, so quit first there.

`transcribe` applies the same text rules before printing, so a file and a
dictation read alike.

`transcribe --json` reports `t_prepare`, `t_load`, `t_decode`, RTF and peak RSS,
which is how the warm-model decision is measured rather than guessed. `--repeat N`
adds the spread: every run, and a summary of min, median, p90 and max.

`model status | verify | download` owns the 640 MB package. `verify` hashes every
file; `download` resumes a partial file, re-downloads one whose checksum is
wrong, and refuses to install a bad copy. Add `--streaming` to any of them for
the optional 73 MB word-by-word package; `scripts/fetch-streaming-model.ps1`
does the same on Windows without the CLI.

`log` prints where the daemon writes and the last lines of it; the file is capped
at 1 MiB so a resident daemon cannot fill a disk. `history` prints the opt-in
transcript history — off by default, pruned after `history_days`, and deleted by
`history --clear`. `history --search <text>` keeps only the entries containing it,
case-insensitively, which is the same filter the window's history pane has.

Environment overrides: `HIPPO_DICTATE_SHERPA_DIR`, `HIPPO_DICTATE_MODEL_DIR`.

## Install (Linux)

The release tarball needs no root and no build tools:

```bash
tar -xzf hippo-dictate-0.2.0-linux-x64.tar.gz
cd hippo-dictate-0.2.0-linux-x64
./install.sh
hippo-dictate model download        # 640 MB, resumable, checksummed
```

`install.sh` puts the four binaries and the sherpa runtime in
`~/.local/libexec/hippo-dictate`, with symlinks in `~/.local/bin`, the hicolor icons
under `~/.local/share/icons` and a launcher for the settings window in
`~/.local/share/applications`, with its `Exec=` written as an absolute path
because a display-manager session does not have `~/.local/bin` on `PATH`. The library lives beside the binaries because the
daemon looks for it next to itself before anywhere else.

```bash
./install.sh --prefix /some/place   # not the default prefix
./install.sh --systemd              # also write a user unit; read the caveat below
./install.sh --help
```

Then bind it, in `~/.config/sway/config` — see "Keybind and autostart" for why
`--release` and `--no-repeat` matter:

```
# `exec` runs through `sh` with sway's own PATH. On a session started by a
# display manager that is /usr/local/bin:/usr/bin, which has no ~/.local/bin, so
# a bare `hippo-dictate` is a "command not found" in the sway log and nothing
# happens when you press the chord. An absolute path always works.
set $hippo ~/.local/bin/hippo-dictate

exec $hippo daemon
bindsym --release --no-repeat ctrl+space exec $hippo toggle
bindsym --release --no-repeat ctrl+shift+space exec $hippo stream
```

`--systemd` writes `~/.config/systemd/user/hippo-dictate.service` with the installed
path substituted in. A user unit does **not** inherit the compositor's
environment on Wayland, so without importing it the daemon comes up with no
`WAYLAND_DISPLAY`, no `SWAYSOCK` and no session bus, and every transcript is
copied into nothing:

```
exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK
exec systemctl --user enable --now hippo-dictate
```

Running the daemon straight from the sway config avoids the whole problem; the
unit buys restarts and journal history, and costs that line.

`uninstall.sh` removes the binaries, the launcher and the icons. The model, the
config and the history stay unless `--purge` is given, because 640 MB is the
expensive part of a setup.

`packaging/PKGBUILD` is the AUR recipe: the tag's archive and the sherpa runtime
as two hashed sources, `prepare()` refusing a tree whose `Cargo.toml` disagrees
with `pkgver`, and `check()` running the test suite in the build directory. It
builds and packages today (`makepkg --nodeps`), with `packaging/.SRCINFO`
generated beside it. Submitting it to the AUR needs the maintainer's account and a
key; the recipe itself is ready.

## Configuration

`config.toml` is read at startup and re-read whenever the file changes: one `stat`
per tick, and the file again only when its size or timestamp moved. Edits are
applied immediately when idle, or at the start of the next capture if one is
already running, so a change never swaps the microphone or the tones under a
recording in progress. Deleting the file restores the defaults.

A file that will not parse leaves the last good config in force and is reported
rather than reset, in `hippo-dictate status` and in the log:

```
config: parse /home/me/.config/hippo-dictate/config.toml: unknown variant `nonsense`
```

`hippo-dictate status` also reports the applied paste method and the revision
number, which is the fastest way to confirm an edit landed:

```
idle 0.0s
hotkey: compositor binding
paste: ydotool
config: revision 3
timings: decode 380 ms · copy 12 ms · paste 9 ms · total 1180 ms
```

Two keys set how the recognizer runs: `chunk_seconds` (20 s) is the silence-split
length, and `threads` (4, clamped to 1..16) is ONNX Runtime's thread count. Both
are settled with `transcribe --repeat` rather than by feel — see "Measured".

Every capture logs its length and RMS — `capture 2.0s rms 0.1435` — so a
transcript that came from near-silence is a measurement rather than a mystery.
That is the data a silence threshold would be tuned against; on this machine the
model returns an empty transcript for a room's noise floor, so there is nothing to
gate yet.

`clipboard` decides what the clipboard holds afterwards: `"keep"` (the default)
puts back whatever was there before, and `"replace"` leaves the transcript. The
restore happens at the **start of the next capture**, not straight after the
paste — restoring immediately races the target application's read of the
selection — and only if the clipboard still holds our text, so anything you copied
in between is left alone.

### Text before it is pasted

A transcript goes through a rules pass between the decoder and the clipboard. No
model, no network: a rule either fires or it does not, which is why these are
testable and why they cannot invent a word you did not say.

Every rule is a row in the config file — you can read the whole table and change
any of it — and the window's **Text** tab edits the same rows.

- `cleanup = true` — remove the words in `fillers`, collapse an immediately
  repeated run ("the the"), tidy spacing and stray punctuation.
- `[[fillers]]` — the words cleanup removes. Shipped: um, uh, uhm, erm, hmm, mhm.
- `formatting = true` — spoken addresses, from `[[formatting_rules]]`: "at sign"
  becomes `@`, "dot com" becomes `.com`, and so on for ten common domains.
- `punctuation_commands = false` — off by default, because people say "comma" and
  "period" in prose. On, each row of `[[punctuation]]` turns a spoken name into
  characters: `comma` → `, `, `question mark` → `? `, `new paragraph` → a blank
  line. A trailing space is part of the replacement, and `\n` in the window's
  field is a line break.
- `commands = true` — a whole utterance from `[[command_rules]]` is an
  instruction: action `discard` ("scratch that") or `quit` ("quit hippo"). A
  trailing phrase from `[[shape_rules]]` reshapes instead: action `bullets` ("as a
  list", one bullet per sentence) or `one line` ("on one line"). Both only match
  the whole utterance, or a suffix of it, so a sentence that contains the words is
  still a sentence.
- `[[snippets]]` — a trigger phrase that expands to fixed text, on an exact
  whole-utterance match.
- `[[replacements]]` — whole-word, case-insensitive substitutions, applied last.

Every matching field — `say`, `trigger`, `from` — takes a comma-separated list of
alternatives, all of which produce the same result:

```toml
[[punctuation]]
say = "exclamation mark, exclamation point"
becomes = "! "
```

That is the shipped shape: one row per mark, one row per action, rather than one
row per spelling. A comma is therefore a separator and not a literal, which is
why the mark is spelled `comma`; the output side (`becomes`, `text`, `to`) is
inserted as written and is never split.

The punctuation characters are **pasted, not typed**: a newline inside a
transcript is a newline inside the clipboard payload, and for a terminal the
payload is flattened to one line, so a paragraph break can never execute the
next word there.

```toml
punctuation_commands = true

# The shipped table is long; a config only needs the rows it changes when it
# replaces the whole table, so copy the defaults before editing them.
[[punctuation]]
say = "semi, semicolon"
becomes = "; "

[[command_rules]]
say = "belay that"
action = "discard"

[[shape_rules]]
say = "as points"
action = "bullets"

[[snippets]]
trigger = "my calendar link"
text = "https://cal.example/sean"

[[replacements]]
from = "hippo"
to = "Hippo Dictate"
```

The order is fixed: cleanup, then a command, then a snippet, then punctuation and
formatting, then replacements — so a snippet is never rewritten by a replacement
and an address is never broken by one. Capitalisation is left alone: Parakeet
already sentence-cases its output, and a rule that re-cases would only be able to
make it worse.

`hippo-dictate text "um, mail sean at sign example dot com"` runs the pass on a
string and prints what would be pasted, which is the fastest way to check a rule.
The settings window's **Text** tab has the same thing live: the switches, a phrase
box that shows the result as you type, and a row editor for every table above —
fillers, punctuation, formatting, commands, shapes, snippets and replacements.

### Local Ollama interface

The daemon can talk to an Ollama server on this machine. The one feature built on
it is **suggested rules**: the settings window reads your dictation history, asks
the local model which corrections you keep making by hand, and offers them as
replacement and snippet rules. You accept or dismiss each row; nothing is applied
without a click, and the model is never in the path of a paste.

Turn on **Keep history** first — the suggestions are drawn from `history.tsv`.

A local server also serves models from `ollama.com`. The base URL being loopback
says nothing about where the prompt goes, so the client reads `remote_host` from
`/api/tags`. With `ollama_model` empty, a suggestion run only ever picks a model
the server reads from this machine's disk; with `ollama_model` set, the named
model is used whatever it is. Bringing your own model means it is yours to point,
and the app does not overrule a line you wrote.

- `ollama_url = "http://127.0.0.1:11434"` — the base URL. The client accepts only
  `127.0.0.1`, `localhost` or `::1`; a LAN or cloud host is an error, so there is
  no configuration that turns this into a network client.
- `ollama_model = ""` — the default model. Empty means the suggestion run takes
  the first model **on disk**, and the `run` command must be given a name; nothing
  is chosen silently.
- `ollama_timeout_seconds = 30` — how long a request waits before it is abandoned.

Four commands exercise the interface against the live server:

```bash
hippo-dictate ollama version
hippo-dictate ollama models
hippo-dictate ollama run llama3.2 "Rewrite this sentence more plainly."
hippo-dictate ollama doctor          # add --json for one object
```

`doctor` is the one to run first. It prints the server's version, every installed
model marked `local` or `CLOUD <host>`, which model a suggestion run would pick
and why not if it cannot, and one completion's speed from the server's own
counters — `tok/s`, load time, total. It does not probe a cloud model: the
measurement would leave the machine, and the number would describe a network hop
rather than a local inference.

`curl` is the transport, for the same reason `model download` uses it: it is
already a runtime dependency, and Ollama's API is plain JSON over HTTP. Requests
ask for `"stream": false`, so a call returns one document. The suggestion run
also sends `"format": "json"`, which the server enforces as a grammar; the reply
is still parsed defensively, and a malformed row is dropped rather than failing
the batch. The client covers `GET /api/version`, `GET /api/tags`,
`POST /api/generate` and `POST /api/chat`.

`hippo-dictate ollama run` takes a prompt you typed yourself and sends it to
whatever model you name, cloud or not. It is a debugging tool, and it never sees
a transcript.

Hotkeys are the exception: the Windows daemon registers its chords at startup, so
a new chord needs a restart. Nothing else does.

A failed dictation is reported the same way: the reason is in `status` as
`last error`, it is in the log, and it is a desktop notification, because the
user's attention is in the window they were typing in. A transcript that lands
clears it. A missing model is caught at startup rather than at the first press:

```
last error: no usable model in /home/me/.local/share/hippo-dictate/models (missing encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx, tokens.txt); run `hippo-dictate model download`
```

The decoder runs on its own thread behind a panic boundary: a panic costs one
dictation — the model is dropped, the reason is reported, and the next press
loads a fresh one — instead of wedging the daemon in `decoding` for the session.

## Testing

Seven environment variables let the whole pipeline run with no microphone and no
model, which is how `tests/daemon.rs` drives a real daemon over its real socket
on CI:

- `HIPPO_DICTATE_FAKE_AUDIO` — a WAV replayed instead of the device.
- `HIPPO_DICTATE_FAKE_MODEL` — the transcript to return instead of decoding.
- `HIPPO_DICTATE_FAKE_DELIVERY` — skip the clipboard and paste, so no external tool
  or focused window is needed.
- `HIPPO_DICTATE_CONFIG` — the config file, named outright rather than derived from
  `XDG_CONFIG_HOME`, so a test can move just the config and rewrite it mid-run.
- `HIPPO_DICTATE_FAKE_DECODE` — break the fake decoder on purpose: `panic` fails the
  first decode, which is how the supervision around it is tested; `slow` delays
  every decode so a second press arrives mid-decode.
- `HIPPO_DICTATE_FAKE_FOCUS` — the focused window as `app:window`, so a test does
  not need a compositor that can answer.
- `HIPPO_DICTATE_LICENCE` — the licence file, named outright.

## Measured

On a Ryzen 7 PRO 8840U, 4 threads, int8:

| | 5 s clip | 45 s clip |
| --- | --- | --- |
| decode | 0.23 s (RTF 0.046) | 2.24 s (RTF 0.050) |
| model load | ~1.5 s | ~1.5 s |
| peak RSS | 1.07 GB | 1.48 GB |

Peak RSS above 1 GB is the warm model. The daemon loads it once and keeps it
warm, so the load time is paid on the first utterance rather than on every one.

These are single samples, which is why nothing here is a tuning decision on its
own. `transcribe --repeat 5 --json` decodes the same audio five times and reports
`runs` plus a `summary` of min, median, p90 and max: nearest rank, so every number
is a run that actually happened. Change `chunk_seconds` or `threads` with that in
front of you, not from one warm cache.

The daemon measures itself too. Every delivery records the stop press to the paste
chord, and `hippo-dictate status` prints it:

```
timings: decode 380 ms · copy 12 ms · paste 9 ms · total 1180 ms
```

`total` includes the queueing, the resample and the normalise, so it is the number
the user actually feels. `copy` and `paste` are `-` when the delivery was skipped,
which is a different thing from a copy that took no time.

## Keybind and autostart (sway)

In `~/.config/sway/config`:

```
# `exec` runs through `sh` with sway's own PATH. On a session started by a
# display manager that is /usr/local/bin:/usr/bin, which has no ~/.local/bin, so
# a bare `hippo-dictate` is a "command not found" in the sway log and nothing
# happens when you press the chord. An absolute path always works.
set $hippo ~/.local/bin/hippo-dictate

exec $hippo daemon
bindsym --release --no-repeat ctrl+space exec $hippo toggle
bindsym --release --no-repeat ctrl+shift+space exec $hippo stream
```

`stream` is the same capture as `toggle`, except it pastes phrases as you speak
rather than one transcript at the end: a phrase is committed at a pause near
`stream_seconds`, and the session stops on the second press. It is a separate
command rather than a modifier because the compositor owns the binding — there is
no chord for the daemon to register, which is what the Windows hotkey does.

`--release` and `--no-repeat` matter: without them a held key toggles in a
storm.

The durable fix for the PATH is a session environment file rather than a path in
every command — `~/.config/environment.d/10-local-bin.conf` containing
`PATH=${HOME}/.local/bin:${PATH}` — but that only applies to the next session, and
the absolute path in the bindings works now. The same reason is why a `.desktop`
entry that says `Exec=hippo-dictate-settings` can do nothing on a minimal session. Starting the daemon from the sway config also gives it `WAYLAND_DISPLAY`,
`XDG_RUNTIME_DIR` and the session bus, which a `systemd --user` unit does not
inherit on Wayland.

## Delivery

Paste uses `wtype` when it is installed, otherwise `ydotool`. `ydotool` needs
`ydotoold` running and access to `/dev/uinput`; this is a safe connectivity
check that injects nothing:

```bash
ydotool type ''
```

Pin one with `paste = "wtype" | "ydotool" | "none"` in
`$XDG_CONFIG_HOME/hippo-dictate/config.toml`. To check injection by hand, focus a
text field and run:

```bash
wl-copy --trim-newline <<< "hello"; ydotool key 29:1 47:1 47:0 29:0
```

Terminals get `ctrl+shift+v` rather than `ctrl+v`, and a trailing newline is
never copied, because a newline pasted into a terminal executes a command.

`wl-copy` is not optional: every delivery path starts with it, including the
copy-only one. Without it the daemon refuses the capture and says why, instead of
recording into a microphone that cannot produce anything.

The paste only happens if the daemon can tell it is still in the window the
capture started in. Two things are compared: the application and the window, not
the title. A title is rewritten constantly for reasons the user did not cause —
a shell prompt after every command, an editor's dirty marker on the first
keystroke — and comparing it refused pastes that should have landed. Two windows
of the same application are still a change of focus.

Which window is focused comes from the compositor, chosen by the variable it
exports: `swaymsg` under `SWAYSOCK`, `hyprctl` under
`HYPRLAND_INSTANCE_SIGNATURE`. Anywhere else the daemon cannot see the focused
window, so it copies the transcript and tells you: pasting blind is how a
transcript lands in a chat window instead of the one you were typing in. That is
the cost of not supporting X11 yet, and it is visible rather than silent.

## Listening popup

While a capture is running the daemon shows a small panel in the top right of the
focused screen: a microphone that **fills from the bottom with your voice**, base
first and capsule last. How much of it is lit is the level, with a fast rise and a
slow fall, so it reads at a glance in peripheral vision — which a row of bars,
moving two pixels per column, did not.

The mapping is in decibels, not linear: −40 dB (a quiet room, RMS 0.01) is empty
and −12 dB (RMS 0.25) is full. Linear put ordinary speech at a fifth of the glyph,
which read as a meter that never moved.

Its colour is the **mode**: green is ordinary dictation, where the transcript is
pasted when you stop, and blue is streaming, where phrases are committed as you
speak. The cradle, stem and base stay dim so the shape is recognisable at zero.

On Linux it is a `wlr-layer-shell` surface drawn by `hippo-dictate-overlay`, a
small separate binary the daemon starts when the capture starts and stops when it
ends. It has to be a layer surface and not a window: `keyboard_interactivity =
none` is what makes it impossible for the popup to take focus, and focus is the
one thing this application cannot afford to lose — the paste would land in the
popup. Naming no output puts it on the compositor's focused one, so it follows the
monitor you are typing on.

Being a separate process is deliberate: it links a Wayland client and a software
renderer, and none of that belongs in the daemon's memory while it is idle over a
lunch break. The daemon writes one JSON line per tick on the child's stdin and
closes the pipe to say "done", so the popup's lifetime is exactly the capture's.

On Windows the same panel is a GDI popup positioned at the top right of the work
area of the monitor holding the foreground window, drawing the same microphone
with the same fill, plus the label and elapsed timer that the Linux panel has no
font stack for.

Nothing about it is load-bearing. A compositor without `wlr-layer-shell`, a
missing overlay binary, or a pipe that breaks costs the popup and nothing else;
the tone cue still plays and the transcript still lands.

Known gap: the Linux panel has no label or timer, where the Windows one shows
"Listening" and elapsed seconds. Drawing text needs a font stack the overlay
deliberately does not carry.

## Tray

The daemon publishes a tray icon: on Linux a StatusNotifierItem, which waybar's
`tray` module and every GTK shell host; on Windows a notification-area icon. It
carries the same menu on both — **Open settings…**, **Recent transcripts**,
**Toggle dictation**, **Quit** — and a left click opens the settings window.

The icon is looked up by name in the theme, so it appears once `install.sh` has
put the hicolor tree in place; a checkout that has installed nothing shows the
host's fallback icon instead. The menu is the point, not the picture.

Nothing about it is load-bearing. A session with no tray host, a bus that is not
there, or a bar that starts after the daemon all mean "no icon", not "no
dictation": the daemon retries for a few seconds in the background and then
gives up quietly, because a daemon that refused to start without a tray would be
a worse bug than a missing menu.

A second click does not stack a window: the settings binary claims an abstract
socket named `hippo-dictate-settings`, and a second invocation hands it the pane it
was asked for, raises the open window and exits. The pane travels through a small
request file the window polls, which is the same mechanism the tray has always
used. Raising is `swaymsg`, so on another compositor the pane still switches but
the window is not brought forward.

## Settings window

`hippo-dictate-settings` is a separate binary, built against `hippo-ui`, that edits
`config.toml` and exits. It never talks to the running daemon over its socket: the
daemon watches the file, so there is no reload protocol to get wrong.

```bash
cargo run -p hippo-dictate-settings
```

It wears the original hippo in the header and the microphone mark in the title
bar. `scripts/make-app-icons.py` derives every shipped size from
`icons/microphone-icon-1024.png`: the `.ico` frames, the taskbar bitmap and the
Linux hicolor tree under `packaging/`.

Three tabs: **Dictation** (input, delivery, activation), **Text** (every rule table
and the live preview), and **History**. It reads `config.toml` and `history.tsv`
and writes only the former, editing the document in place so a key it does not
recognise and the comments around it survive a save.
The Windows hotkey is editable there; a chord it cannot parse is reported instead
of saved. Everything except the hotkeys takes effect without a restart — see
"Configuration" above.

It is the second consumer of `hippo-ui` and the reason that library has a
gallery and a style snapshot.

## The socket

`hippo-dictate toggle` is a client of the daemon, over a Unix socket in
`$XDG_RUNTIME_DIR/hippo-dictate/` (a loopback port with a per-run token on Windows).
Every request carries the wire version and every reply reports the daemon's, so a
client and a daemon from different builds refuse each other with a sentence
instead of reading a field that has changed meaning. Adding a field does not
change the version; changing one does.

## Licensing

Everything that dictates is free: the daemon, both models, the settings window,
streaming with Parakeet phrase chunks, the history, the tray. Pro is a signed file
that unlocks the extras — today that is the word-by-word zipformer
(`stream_online`), which is otherwise a 73 MB download for a worse transcript.

The licence is a short document with an Ed25519 signature, checked offline against
a public key compiled into the binary. Nothing is fetched and nothing phones home:

```
subject = "someone@example.com"
tier = "pro"
issued = 1794692085
---
<128 hex characters>
```

The signature covers the document byte for byte, so an edited `tier` line stops
verifying rather than quietly granting itself Pro. `expires` is optional; without
it the licence is perpetual, and an expired one drops to the free tier with a
sentence in `status` and `doctor` rather than refusing to run.

A source build has no public key and therefore verifies nothing — that is
deliberate, so a rebuilt or forked binary cannot manufacture licences. A release
build names one:

```bash
HIPPO_DICTATE_LICENCE_PUBKEY=<64 hex characters> cargo build --release
```

Issuing is `licence keygen`, then `licence sign`, both hidden from `--help`
because they are not things a user does:

```bash
hippo-dictate licence keygen > keys.txt        # keep the private line off this machine
hippo-dictate licence sign --subject someone@example.com --key private.key > licence
hippo-dictate licence show                    # what is in force here, and why not
```

`HIPPO_DICTATE_LICENCE` names the file for a test; the installed one is
`$XDG_DATA_HOME/hippo-dictate/licence`. The daemon reads it at startup, so a
licence dropped in while it runs applies after a restart.

## Windows

Install for the current user from the release folder — no administrator rights:

```powershell
.\install.ps1
hippo-dictate model download
```

`install.ps1` copies all three executables and the runtime DLLs to
`%LOCALAPPDATA%\Programs\hippo-dictate`, adds that folder to the user `PATH`,
registers `hippo-dictate-daemon.exe` under `HKCU\...\Run` so it starts at login
without a console window, starts it, and prints `doctor`. The daemon also adds a
**notification-area icon**: left click opens the settings window, right click
opens a menu with settings, recent transcripts, **Toggle dictation** and Quit.
The settings window is single-instance, so a second click focuses it instead of
opening another. While recording, a toast appears above the clock with the word
"Listening", a timer and a live level meter. It is created with
`WS_EX_NOACTIVATE`, so showing it never takes focus from the window being typed
into. `-NoStartup` and
`-NoPath` make it portable. `uninstall.ps1` reverses every step and asks before
deleting the 640 MB model.

Unsigned executables make SmartScreen say "Windows protected your PC" until you
choose More info → Run anyway. That is a signing certificate decision, not a bug.

### Signed MSIX, for sideloading

```powershell
.\scripts\package-msix.ps1            # build, self-sign, pack
.\scripts\package-msix.ps1 -Install   # and install it
```

Three pieces, in order: `build-windows.ps1` stages the executables and runtime,
`make-test-cert.ps1` creates or reuses a self-signed code-signing certificate whose
subject matches `packaging\identity.json`, and `make-msix.ps1` lays the package
out, renders `packaging\AppxManifest.xml.template`, indexes the tile assets with
`makepri`, packs with `makeappx`, unpacks it again to prove nothing was lost, and
signs it with `signtool`.

The certificate is per-user and does not need elevation: the private half stays in
`Cert:\CurrentUser\My`, the `.cer` goes into `Cert:\CurrentUser\TrustedPeople`,
and the package installs with `Add-AppxPackage` on this machine. Anywhere else it
will not install until that certificate is trusted there, which is what
"self-signed" means. `-NoCertificate` packs without signing when you only want to
inspect the contents, and `-SkipBuild` reuses an existing `dist` folder.

The daemon runs at login through the manifest's `windows.startupTask`, because the
settings window is the app's face and the daemon has no window of its own. The
`microphone` device capability is declared: a packaged app without it is silently
given silence rather than an error.

CI has a `windows-msix` job behind `tags: [windows]`, manual, because the runners
this project has are Linux and the cross-build is all they can do.

Or build the release folder in one command from a checkout:

```powershell
.\scripts\build-windows.ps1
```

It enters the Visual Studio developer environment when `link.exe` is not already
on `PATH`, fetches the runtime if absent, runs the tests, builds the three
executables,
stages the Visual C++ runtime so a clean machine can start the exe, and writes
`dist\hippo-dictate-<version>-win-x64` plus a zip of it. `-SkipTests`, `-Dev`, `-NoZip` and `-WithModel` are the useful switches; the
640 MB model is not bundled unless you ask.

Delivery uses `SendInput`. A window running elevated — an administrator console,
say — refuses injected input by design: the transcript stays on the clipboard and a
tray balloon says so. Nothing at normal privilege can paste into it.

The release folder needs the Visual C++ runtime beside the exes:
`sherpa-onnx-c-api.dll` imports `vcruntime140`/`msvcp140` and `onnxruntime.dll`
also imports `msvcp140_1`. `build-windows.ps1` stages them from the Visual Studio
redistributable; the CI artifact does not, so on a machine without the
redistributable installed, `doctor` will report the runtime as unloadable.

**Status: compiles and type-checks for `x86_64-pc-windows-msvc`, has not been
run.** What is wired:

- The daemon registers `ctrl+space` itself with `RegisterHotKey` on a thread with
  a message loop, because a shortcut cannot launch a command line the way sway
  launches `hippo-dictate toggle`. While it runs, that chord is owned system-wide,
  so the input-method switch on some layouts and editor autocomplete stop
  reaching their applications; `hotkey = "ctrl+alt+space"` in `config.toml`
  trades the shorter chord for that. The settings window has the same field.
- **Streaming mode** (`ctrl+shift+space`) records continuously and pastes text
  while you speak. Both modes use Parakeet by default; streaming commits phrases
  at pauses near `stream_seconds` (3 s, editable in settings). The optional
  word-by-word engine is a **Pro** switch in settings (`stream_online`): a 73 MB
  online zipformer with 0.6 s lookahead, English-only and less accurate than
  Parakeet, which pastes only words that survive one revision. A phrase handoff
  (endpoint, or `stream_segment_seconds` without a pause, 8 s by default) flushes
  the text and resets the decoder, so one head cannot grow without bound. If the
  package is missing or fails to load, streaming falls back to Parakeet chunks.
  Either way the session stops if focus changes, because continuing would paste
  into the wrong window. `stream_hotkey`, `stream_seconds`,
  `stream_segment_seconds`, `stream_online` and
  `HIPPO_DICTATE_STREAMING_MODEL_DIR` are config knobs.
- The transport is loopback TCP on an ephemeral port with a per-run token in the
  state directory: an open port is not authority to open a microphone.
- Delivery is `arboard` for the clipboard and `enigo` (`SendInput`) for the chord.
- There is no toast equivalent of `notify-send` yet; failures go to the log.

Files follow the platform: `%APPDATA%\hippo-dictate` for configuration and
`%LOCALAPPDATA%\hippo-dictate` for logs, the endpoint file, models and cache.
(`HOME` is usually unset on Windows, which is why nothing resolved there before.)

Fetch the native pieces with the PowerShell equivalents:

```powershell
.\scripts\fetch-sherpa.ps1
.\scripts\fetch-model.ps1
```

`fetch-sherpa.ps1` pulls the 6.6 MB `MD-Release` archive — the only Windows build
published — and extracts it with an installed 7-Zip, Git for Windows' `bzip2`, or
a standalone 7-Zip it downloads to `%LOCALAPPDATA%\hippo-dictate\cache`. The
built-in `tar` has no bzip2, so it cannot do this alone.
Both DLLs and `onnxruntime_providers_shared.dll` must sit beside the executable:
Windows resolves a DLL's imports from the application directory, not from the DLL.

For development, fetch the native pieces into the checkout and start the tray
daemon with one command:

```powershell
.\scripts\run-windows.ps1
```

It enters the Visual Studio environment when `link.exe` is missing, fetches the
runtime if it is absent, warns when the model is missing (`-WithModel` fetches
it), builds the whole workspace so `hippo-dictate-settings.exe` exists beside the
daemon, and then runs `cargo run --bin hippo-dictate-daemon`. The daemon is a
GUI-subsystem binary, so no console window survives the launch; the terminal
stays with cargo until you Quit from the tray. `-Release` uses the release
profile. `-WithStreamingModel` fetches the optional word-by-word package.
`-Force` stops a running daemon or Settings window before building; without it,
the script refuses with the process list because Windows cannot replace a
running executable. The equivalent without the preflight checks is
`cargo build --workspace` followed by `cargo run --bin hippo-dictate-daemon`; the
workspace build matters because the tray launches the settings executable from
the daemon's own directory.

A startup failure is written to `%LOCALAPPDATA%\hippo-dictate\log`; the script
prints the tail when cargo exits non-zero. Errors from the dev build are also
appended to `hippo-dictate-error.log` in the checkout root — that file is
gitignored, capped at 256 KB, and is the one to send back. Closing the terminal
does not necessarily stop a GUI-subsystem daemon, so Quit from the tray before
starting another one - the script detects a live endpoint and says so rather than
failing with a bare exit code.

Check it from another host with `cargo check --target x86_64-pc-windows-msvc`.

## Licence

Code: LGPL-2.1-only.

Model: Parakeet TDT 0.6B v3 is **CC BY 4.0** — NVIDIA trained it, the
sherpa-onnx project produced the ONNX export. Attribution is required and is
recorded in `models.lock`. The optional streaming zipformer is **Apache-2.0**
(k2-fsa/icefall, exported by sherpa-onnx) and is recorded in
`models-streaming.lock`. sherpa-onnx is Apache-2.0; ONNX Runtime is MIT.
