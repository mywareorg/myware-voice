# Hippo Voice

Push-to-talk dictation. Press the keybind, speak, press it again: the transcript
goes to the clipboard and is pasted into the window you were typing in.

Local-only. The audio never leaves the machine; there are no accounts, no
telemetry, and no network use after the model is installed.

**Status: M0.** `doctor` and `transcribe` work. The daemon and the keybind come
next.

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

`doctor` prints the resolved runtime, the model, the session type, and which of
`wl-copy`, `wtype`, `ydotool` and `notify-send` it found.

`transcribe --json` reports `t_prepare`, `t_load`, `t_decode`, RTF and peak RSS,
which is how the warm-model decision is measured rather than guessed.

`model status | verify | download` owns the 640 MB package. `verify` hashes every
file; `download` resumes a partial file, re-downloads one whose checksum is
wrong, and refuses to install a bad copy.

`log` prints where the daemon writes and the last lines of it; the file is capped
at 1 MiB so a resident daemon cannot fill a disk. `history` prints the opt-in
transcript history — off by default, pruned after `history_days`, and deleted by
`history --clear`.

Environment overrides: `HIPPO_VOICE_SHERPA_DIR`, `HIPPO_VOICE_MODEL_DIR`.

## Measured

On a Ryzen 7 PRO 8840U, 4 threads, int8:

| | 5 s clip | 45 s clip |
| --- | --- | --- |
| decode | 0.23 s (RTF 0.046) | 2.24 s (RTF 0.050) |
| model load | ~1.5 s | ~1.5 s |
| peak RSS | 1.07 GB | 1.48 GB |

Peak RSS above 1 GB is the warm model. The daemon loads it once and keeps it
warm, so the load time is paid on the first utterance rather than on every one.

## Keybind and autostart (sway)

In `~/.config/sway/config`:

```
exec hippo-voice daemon
bindsym --release --no-repeat ctrl+space exec hippo-voice toggle
```

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

## Settings window

`hippo-voice-settings` is a separate binary, built against `hippo-ui`, that edits
`config.toml` and exits. It never talks to the running daemon over its socket: the
daemon reads the file on the next capture, so there is no reload protocol to get
wrong.

```bash
cargo run -p hippo-voice-settings
```

It is the second consumer of `hippo-ui` and the reason that library has a
gallery and a style snapshot.

## Windows

Compiles and type-checks for `x86_64-pc-windows-msvc`, but has **not been run**:
there is no Windows machine in this development environment. What is wired:

- The daemon registers `ctrl+space` itself with `RegisterHotKey` on a thread with
  a message loop, because a shortcut cannot launch a command line the way sway
  launches `hippo-voice toggle`.
- The transport is loopback TCP on an ephemeral port with a per-run token in the
  state directory: an open port is not authority to open a microphone.
- Delivery is `arboard` for the clipboard and `enigo` (`SendInput`) for the chord.
- There is no toast equivalent of `notify-send` yet; failures go to the log.

Fetch the native pieces with the PowerShell equivalents:

```powershell
.\scripts\fetch-sherpa.ps1
.\scripts\fetch-model.ps1
```

`fetch-sherpa.ps1` pulls the 6.6 MB `MD-Release` archive — the only Windows build
published — and falls back to 7-Zip when the built-in `tar` cannot handle bzip2.
Both DLLs and `onnxruntime_providers_shared.dll` must sit beside the executable:
Windows resolves a DLL's imports from the application directory, not from the DLL.

Check it from another host with `cargo check --target x86_64-pc-windows-msvc`.

## Licence

Code: LGPL-2.1-only.

Model: Parakeet TDT 0.6B v3 is **CC BY 4.0** — NVIDIA trained it, the
sherpa-onnx project produced the ONNX export. Attribution is required and is
recorded in `models.lock`. sherpa-onnx is Apache-2.0; ONNX Runtime is MIT.
