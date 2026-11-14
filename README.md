# Hippo Voice

Push-to-talk dictation. Press the keybind, speak, press it again: the transcript
goes to the clipboard and is pasted into the window you were typing in.

Local-only. The audio never leaves the machine; there are no accounts, no
telemetry, and no network use after the model is installed.

**Status:** works end to end on sway/Wayland, including streaming dictation
(`hippo-voice stream`). The Windows build compiles and is type-checked but has
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
./target/debug/hippo-voice doctor
./target/debug/hippo-voice transcribe some.wav
```

`doctor` prints the resolved runtime, the model, the session type, whether the
focused window can be read, the licence tier, and which of `wl-copy`, `wtype`,
`ydotool` and `notify-send` it found.

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
`history --clear`.

Environment overrides: `HIPPO_VOICE_SHERPA_DIR`, `HIPPO_VOICE_MODEL_DIR`.

## Configuration

`config.toml` is read at startup and re-read whenever the file changes: one `stat`
per tick, and the file again only when its size or timestamp moved. Edits are
applied immediately when idle, or at the start of the next capture if one is
already running, so a change never swaps the microphone or the tones under a
recording in progress. Deleting the file restores the defaults.

A file that will not parse leaves the last good config in force and is reported
rather than reset, in `hippo-voice status` and in the log:

```
config: parse /home/me/.config/hippo-voice/config.toml: unknown variant `nonsense`
```

`hippo-voice status` also reports the applied paste method and the revision
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

Hotkeys are the exception: the Windows daemon registers its chords at startup, so
a new chord needs a restart. Nothing else does.

A failed dictation is reported the same way: the reason is in `status` as
`last error`, it is in the log, and it is a desktop notification, because the
user's attention is in the window they were typing in. A transcript that lands
clears it. A missing model is caught at startup rather than at the first press:

```
last error: no usable model in /home/me/.local/share/hippo-voice/models (missing encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx, tokens.txt); run `hippo-voice model download`
```

The decoder runs on its own thread behind a panic boundary: a panic costs one
dictation — the model is dropped, the reason is reported, and the next press
loads a fresh one — instead of wedging the daemon in `decoding` for the session.

## Testing

Seven environment variables let the whole pipeline run with no microphone and no
model, which is how `tests/daemon.rs` drives a real daemon over its real socket
on CI:

- `HIPPO_VOICE_FAKE_AUDIO` — a WAV replayed instead of the device.
- `HIPPO_VOICE_FAKE_MODEL` — the transcript to return instead of decoding.
- `HIPPO_VOICE_FAKE_DELIVERY` — skip the clipboard and paste, so no external tool
  or focused window is needed.
- `HIPPO_VOICE_CONFIG` — the config file, named outright rather than derived from
  `XDG_CONFIG_HOME`, so a test can move just the config and rewrite it mid-run.
- `HIPPO_VOICE_FAKE_DECODE` — break the fake decoder on purpose: `panic` fails the
  first decode, which is how the supervision around it is tested; `slow` delays
  every decode so a second press arrives mid-decode.
- `HIPPO_VOICE_FAKE_FOCUS` — the focused window as `app:window`, so a test does
  not need a compositor that can answer.
- `HIPPO_VOICE_LICENCE` — the licence file, named outright.

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
chord, and `hippo-voice status` prints it:

```
timings: decode 380 ms · copy 12 ms · paste 9 ms · total 1180 ms
```

`total` includes the queueing, the resample and the normalise, so it is the number
the user actually feels. `copy` and `paste` are `-` when the delivery was skipped,
which is a different thing from a copy that took no time.

## Keybind and autostart (sway)

In `~/.config/sway/config`:

```
exec hippo-voice daemon
bindsym --release --no-repeat ctrl+space exec hippo-voice toggle
bindsym --release --no-repeat ctrl+shift+space exec hippo-voice stream
```

`stream` is the same capture as `toggle`, except it pastes phrases as you speak
rather than one transcript at the end: a phrase is committed at a pause near
`stream_seconds`, and the session stops on the second press. It is a separate
command rather than a modifier because the compositor owns the binding — there is
no chord for the daemon to register, which is what the Windows hotkey does.

`--release` and `--no-repeat` matter: without them a held key toggles in a
storm. Starting the daemon from the sway config also gives it `WAYLAND_DISPLAY`,
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
`$XDG_CONFIG_HOME/hippo-voice/config.toml`. To check injection by hand, focus a
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

## Settings window

`hippo-voice-settings` is a separate binary, built against `hippo-ui`, that edits
`config.toml` and exits. It never talks to the running daemon over its socket: the
daemon watches the file, so there is no reload protocol to get wrong.

```bash
cargo run -p hippo-voice-settings
```

It wears the original hippo in the header and the microphone mark in the title
bar. `scripts/make-app-icons.py` derives every shipped size from
`icons/microphone-icon-1024.png`: the `.ico` frames, the taskbar bitmap and the
Linux hicolor tree under `packaging/`.

Two panes: settings, and the transcripts kept when "Keep history" is on. It reads
`config.toml` and `history.tsv` and writes only the former, editing the document
in place so a key it does not recognise and the comments around it survive a save.
The Windows hotkey is editable there; a chord it cannot parse is reported instead
of saved. Everything except the hotkeys takes effect without a restart — see
"Configuration" above.

It is the second consumer of `hippo-ui` and the reason that library has a
gallery and a style snapshot.

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
HIPPO_VOICE_LICENCE_PUBKEY=<64 hex characters> cargo build --release
```

Issuing is `licence keygen`, then `licence sign`, both hidden from `--help`
because they are not things a user does:

```bash
hippo-voice licence keygen > keys.txt        # keep the private line off this machine
hippo-voice licence sign --subject someone@example.com --key private.key > licence
hippo-voice licence show                    # what is in force here, and why not
```

`HIPPO_VOICE_LICENCE` names the file for a test; the installed one is
`$XDG_DATA_HOME/hippo-voice/licence`. The daemon reads it at startup, so a
licence dropped in while it runs applies after a restart.

## Windows

Install for the current user from the release folder — no administrator rights:

```powershell
.\install.ps1
hippo-voice model download
```

`install.ps1` copies all three executables and the runtime DLLs to
`%LOCALAPPDATA%\Programs\hippo-voice`, adds that folder to the user `PATH`,
registers `hippo-voice-daemon.exe` under `HKCU\...\Run` so it starts at login
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

Or build the release folder in one command from a checkout:

```powershell
.\scripts\build-windows.ps1
```

It enters the Visual Studio developer environment when `link.exe` is not already
on `PATH`, fetches the runtime if absent, runs the tests, builds both executables,
stages the Visual C++ runtime so a clean machine can start the exe, and writes
`dist\hippo-voice-<version>-win-x64` plus a zip of it. `-SkipTests`, `-Dev`, `-NoZip` and `-WithModel` are the useful switches; the
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
  launches `hippo-voice toggle`. While it runs, that chord is owned system-wide,
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
  `HIPPO_VOICE_STREAMING_MODEL_DIR` are config knobs.
- The transport is loopback TCP on an ephemeral port with a per-run token in the
  state directory: an open port is not authority to open a microphone.
- Delivery is `arboard` for the clipboard and `enigo` (`SendInput`) for the chord.
- There is no toast equivalent of `notify-send` yet; failures go to the log.

Files follow the platform: `%APPDATA%\hippo-voice` for configuration and
`%LOCALAPPDATA%\hippo-voice` for logs, the endpoint file, models and cache.
(`HOME` is usually unset on Windows, which is why nothing resolved there before.)

Fetch the native pieces with the PowerShell equivalents:

```powershell
.\scripts\fetch-sherpa.ps1
.\scripts\fetch-model.ps1
```

`fetch-sherpa.ps1` pulls the 6.6 MB `MD-Release` archive — the only Windows build
published — and extracts it with an installed 7-Zip, Git for Windows' `bzip2`, or
a standalone 7-Zip it downloads to `%LOCALAPPDATA%\hippo-voice\cache`. The
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
it), builds the whole workspace so `hippo-voice-settings.exe` exists beside the
daemon, and then runs `cargo run --bin hippo-voice-daemon`. The daemon is a
GUI-subsystem binary, so no console window survives the launch; the terminal
stays with cargo until you Quit from the tray. `-Release` uses the release
profile. `-WithStreamingModel` fetches the optional word-by-word package.
`-Force` stops a running daemon or Settings window before building; without it,
the script refuses with the process list because Windows cannot replace a
running executable. The equivalent without the preflight checks is
`cargo build --workspace` followed by `cargo run --bin hippo-voice-daemon`; the
workspace build matters because the tray launches the settings executable from
the daemon's own directory.

A startup failure is written to `%LOCALAPPDATA%\hippo-voice\log`; the script
prints the tail when cargo exits non-zero. Errors from the dev build are also
appended to `hippo-voice-error.log` in the checkout root — that file is
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
