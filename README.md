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

## Licence

Code: LGPL-2.1-only.

Model: Parakeet TDT 0.6B v3 is **CC BY 4.0** — NVIDIA trained it, the
sherpa-onnx project produced the ONNX export. Attribution is required and is
recorded in `models.lock`. sherpa-onnx is Apache-2.0; ONNX Runtime is MIT.
