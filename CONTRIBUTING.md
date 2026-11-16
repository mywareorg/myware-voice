# Contributing

## Licence and sign-off

The project is `LGPL-2.1-only` (`Cargo.toml`), and patches are accepted under the
same licence. Sign your commits off to certify you wrote the code or have the
right to submit it:

```bash
git commit -s -m "Fix focus detection when the title changes"
```

The `Signed-off-by` trailer is the whole of the process; there is no CLA form to
fill in.

The licence is deliberately `-only` rather than `-or-later`. Moving to a later
LGPL therefore needs permission from every copyright holder, so provenance is
what makes that possible or impossible. If you are contributing on behalf of an
employer, make sure they are content with that before you send the patch.

## Workflow

One developer, one branch. Work on `main`, do not open a merge request, and
commit once the change is ready:

- Imperative subject, no conventional-commit prefix, no trailing period, under
  about 72 characters.
- The body says what changed and why. Name known gaps explicitly; do not let the
  message imply more than the patch delivers.
- Push to `main` in the same sitting.

## What a patch must not break

These are load-bearing, and each of them has already cost time (see `AGENTS.md`):

- **Never type the transcript.** Copy, then send a paste chord. Typing is seconds
  of visible keystrokes and races the user.
- **Never inject Enter.** A trailing newline pasted into a terminal runs a
  command. Strip it before copying.
- **Start capture before loading the model.** The user is already talking.
- **Do not take focus.** The daemon and its overlay must never steal it, or the
  paste lands in the wrong window.
- **Measure, then decide.** Latency and memory claims come from
  `transcribe --json`, not from intuition.

## Tests

`cargo test` runs the unit tests and the integration tests in `tests/daemon.rs`.
The integration tests drive the real binary through the seams it exposes:

- `MYWARE_DICTATE_FAKE_AUDIO` / `MYWARE_DICTATE_FAKE_AUDIO_FAILS` — replay a WAV
  instead of opening a microphone.
- `MYWARE_DICTATE_FAKE_MODEL` — a canned transcript, no model on disk.
- `MYWARE_DICTATE_FAKE_DELIVERY` — skip the clipboard and the paste.
- `MYWARE_DICTATE_CONFIG` — name the config file outright.
- `XDG_*` variables — isolate state, history and the socket.

New behaviour needs a test at the seam where it can run headlessly. If it can
only be checked with a compositor or a real Windows session, say so in the commit
body and in the README instead of adding a test that will never run.

`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all --check` are required; CI runs both.

## Scope

Linux on sway/Wayland is the target. Windows compiles and is type-checked but has
not been run, so treat its runtime behaviour as unproven. X11 is not a goal until
someone needs it.
