# Cantrip

Local-first dictation for Linux. Press a key, speak, let go — your words appear
where your cursor is. A cantrip is a small spell you can always cast; this is
that, for text.

## Features

- **Local transcription by default.** Speech never leaves the machine: an
  int8 ONNX [Parakeet TDT 0.6B v3](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx)
  model via [transcribe-rs](https://github.com/cjpais/transcribe-rs) runs on
  CPU in ~250 ms for short dictations.
- **Optional transcript cleanup.** Cleanup is disabled by default. When enabled,
  the raw transcript can pass through a small language model—such as
  `qwen3:8b` on local Ollama—to remove spoken disfluencies and add punctuation
  and capitalization before delivery.
- **Optional cloud lanes.** Any OpenAI-compatible STT or chat endpoint can
  replace the local models (see `docs/CONFIGURATION.md`). API keys live in
  the OS keyring, never in files.
- **Long dictation without oversized uploads.** Local and cloud STT split long
  recordings into bounded chunks and deliver one finished transcript. Cloud
  failure, partial text, or empty recognition automatically gets one whole-take
  retry with installed local Parakeet. No new cloud provider or model download.
  If neither backend completes, available text and original audio remain saved.
- **Passive status HUD.** A bottom-anchored, input-transparent track shows measured
  microphone activity and real multi-chunk progress. It never takes focus or
  invents progress. Silence, partial text, deferred delivery, storage failures,
  and lost connections have distinct notices. Labels and reduced motion are
  configurable; HUD and native windows follow the desktop palette.
- **Guarded paste-first delivery.** Finished text is normally copied with
  `wl-copy`, then pasted with one native Wayland `Ctrl+Shift+V` chord.
  The destination and uninterrupted session history must remain verifiable.
  Changed focus, locks, suspend, or ambiguous desktop state defer delivery
  instead of sending text elsewhere. Strict `type` mode never touches the
  clipboard; `clipboard` mode only copies. No `wtype`/`ydotool` subprocesses.
- **Per-recording recovery.** `cantrip actions` opens a deliberate native window
  for selected-recording copy, local or configured-provider recovery, confirmed
  deletion, cancellation, and setup. Metadata never exposes transcript text.
  A new failure or unrelated success cannot replace another pending recording.

```
trigger ──> capture (pw-record) ──> STT (local, default) ──> postproc (Ollama) ──> inject
             16 kHz mono s16          parakeet | cloud    qwen3:8b | ...   paste | type | clip
```

## Requirements

- Linux with PipeWire (`pw-record`) and a Wayland compositor.
- `wl-clipboard` for paste/copy delivery (usually preinstalled).
- Native keyboard delivery currently requires a direct Hyprland desktop with
  the Lua focus/layer APIs (verified on 0.56.2), the virtual-keyboard protocol,
  and a verifiable active logind session. Authenticated UWSM-managed desktops
  are supported; nested or unverified desktops fail closed.
  On other compositors, choose `injection = "clipboard"` or explicitly copy
  from `cantrip actions`; automatic delivery does not guess.
- The HUD needs a compositor with the Wayland layer-shell protocol (COSMIC,
  Sway, Hyprland, wlroots-based).

```sh
sudo apt install libdbus-1-dev libssl-dev pkg-config wl-clipboard  # source-build prerequisites
```

## Quickstart

### Download and install

Use the [latest verified Linux x86-64 release](https://github.com/misty-step/cantrip/releases/latest)
for a CPU-only executable that needs no source checkout or Rust installation.
The runtime baseline is Ubuntu 24.04 / glibc 2.39. Follow the bundled
[`INSTALLATION.md`](docs/INSTALLATION.md) for checksums, signed provenance,
runtime packages, first use, and data-preserving update/rollback/uninstall.
The installer changes only the executable; service and shortcut setup remain
explicit. Models are a separate, explicitly requested download. The same GitHub
Release publishes `release.json` and Landmark's `releases.json` for the website.

### Build from source

```sh
cargo build --release --locked

# 1. Create the annotated default config.
./target/release/cantrip config init

# 2. Inspect the effective capture, STT, cleanup, injection, HUD, and daemon
# paths. Follow each reported action; the local default will request the model.
./target/release/cantrip doctor
./target/release/cantrip models pull   # when doctor requests it

# 3. Run the daemon in a dedicated terminal for this first session.
./target/release/cantrip daemon

# 4. In another terminal, dictate once and inspect the delivery outcome.
./target/release/cantrip toggle        # start
./target/release/cantrip toggle        # stop, transcribe, and deliver
```

Bind one custom shortcut to the absolute path of `cantrip toggle`; press it once
to start and once to stop. Compositors that support separate key-down and
key-up commands can bind `cantrip start` and `cantrip stop` instead. Unsupported
desktop safety checks require clipboard mode or explicit recovery, as above.

Run `cantrip doctor` again after changing config or installing a prerequisite.
If cleanup is enabled, ensure its configured endpoint is running and its
keyring credential id, when needed, was stored with `cantrip key set`.
`cantrip cancel` stops capture or processing without injecting or deleting audio.
Cancellation prevents later chunks, local fallback, cleanup requests, and delivery;
an already-running provider request may need to return before the worker settles.

## User service (graphical session)

[`contrib/cantrip.service`](contrib/cantrip.service) provides optional systemd
startup. `cantrip actions` uses an installed service, even when disabled; it
does not enable it or start a competing daemon. Without a service, Actions can
start a direct process. Neither path changes hotkeys.

**Supported:** systemd 246 or newer, one Wayland session per Unix user, and a
session manager that refreshes its environment before starting
`graphical-session.target` and stops that target on logout. From the attended,
unlocked graphical session, check:

```sh
systemctl --user is-active graphical-session.target
systemctl --user show cantrip.service \
  --property=LoadState,FragmentPath,DropInPaths,UnitFileState,ActiveState
```

If the target is inactive or the desktop does not manage its login/logout
lifecycle, use Actions or the first-session terminal instead. Do not manually
start the target or enable lingering to bypass missing session integration.
If a personal/package service or compositor autostart already owns Cantrip,
keep that owner or deliberately migrate it; do not install a second one.

### Fresh installation

Complete the first-session setup above. Finish or cancel the current take and
stop its daemon through its existing owner; Ctrl+C stops a foreground daemon.
`cantrip stop` ends recording, not the daemon. For an Actions-started process,
identify its executable and PID before sending that specific process SIGTERM
and waiting for exit; do not use a broad `pkill` or delete its socket.

From the repository root, this block refuses existing binaries, units, masks,
symlinks, and personal drop-in directories:

```sh
(
  set -eu
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  if [ "$(systemctl --user show cantrip.service --property=LoadState --value)" != not-found ]; then
    printf '%s\n' 'Existing or unknown service owner; inspect it before replacing it.' >&2
    exit 1
  fi
  for path in "$HOME/.local/bin/cantrip" "$unit_dir/cantrip.service" "$unit_dir/cantrip.service.d"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      printf 'Refusing to replace %s\n' "$path" >&2
      exit 1
    fi
  done
  mkdir -p "$HOME/.local/bin" "$unit_dir"
  install -m755 ./target/release/cantrip "$HOME/.local/bin/cantrip"
  install -m644 contrib/cantrip.service "$unit_dir/cantrip.service"
  systemctl --user daemon-reload
  systemctl --user cat cantrip.service
)
```

Review the effective unit and any inherited drop-ins. It runs
`%h/.local/bin/cantrip`, not a login-shell `PATH` lookup. To retain another
executable location, skip the binary copy and use
`systemctl --user edit cantrip.service` to set:

```ini
[Service]
ExecStart=
ExecStart=/absolute/path/to/cantrip daemon
```

### Environment and readiness

Services inherit the user manager's environment, not the invoking terminal's.
The session manager must supply the current `WAYLAND_DISPLAY` and, on Hyprland,
`HYPRLAND_INSTANCE_SIGNATURE` before startup. Do not hard-code or guess them.
To repair this login from its graphical terminal, before starting Cantrip:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
# On Hyprland:
systemctl --user import-environment HYPRLAND_INSTANCE_SIGNATURE
```

A one-time import does not configure future logins; the session manager must
refresh these values each login and retire them on logout. Import
`XDG_CURRENT_DESKTOP` and `XDG_SESSION_TYPE` if supplied by that session.
`WAYLAND_SOCKET` must be absent. Keep the manager's runtime directory and D-Bus
address; never import an entire shell environment or put keys in a unit.
Custom XDG config/data/state paths and tool `PATH` must agree with the existing
installation so models and recordings do not appear missing. Environment
changes affect newly started processes, not an already-running daemon.

After reviewing the environment:

```sh
systemctl --user enable cantrip.service
systemctl --user start cantrip.service
"$HOME/.local/bin/cantrip" ping
"$HOME/.local/bin/cantrip" status --json
"$HOME/.local/bin/cantrip" doctor
```

Use the overridden executable path if applicable. An active service is not
dictation readiness: wait for successful IPC, inspect prerequisites, and make
an attended trial in a safe destination. Diagnose failures with
`journalctl --user -u cantrip.service -b --no-pager` and Actions, not a second
daemon. After repairing repeated startup failures, run
`systemctl --user reset-failed cantrip.service` before starting again.

### Stop, update, and remove

Finish/cancel a take and wait for Idle before routine maintenance:

```sh
systemctl --user stop cantrip.service          # stop now
systemctl --user disable --now cantrip.service # also remove login enablement
```

The unit follows the [graphical-session lifecycle](https://www.freedesktop.org/software/systemd/man/latest/systemd.special.html#graphical-session.target).
Explicit stops are not failure-restarted. `KillMode=mixed` lets the daemon stop
`pw-record` with SIGINT and retain audio before terminating remaining children.
A stuck shutdown is force-killed after 90 seconds; inspect failures rather than
assuming runtime-only audio became durable. No unit action deletes retained data.

For an update, build first, record the current enabled/running state, and make
a private backup of the binary, unit, and drop-ins. Stop the existing owner
and require a clean inactive result before replacement. For a regular binary
at the default location, stage beside it and rename atomically:

```sh
(
  set -eu
  binary="$HOME/.local/bin/cantrip"
  [ -f "$binary" ] && [ ! -L "$binary" ]
  staged="$(mktemp "$HOME/.local/bin/.cantrip.XXXXXX")"
  trap 'rm -f -- "$staged"' EXIT
  install -m755 ./target/release/cantrip "$staged"
  mv -T -- "$staged" "$binary"
)
```

Use the package owner's procedure for package-managed installations. Leave
unchanged units and personal drop-ins alone. When deliberately replacing a
unit, disable its old enablement first, review/merge overrides, install the
replacement, reload the manager, and restore the intended enablement. Recheck
environment, IPC, and dictation. Roll back using the backed-up binary and only
the changed unit settings; never restore whole directories over later edits.
Binary rollback does not itself prove history-schema compatibility.

To remove only the unit installed by this guide, disable it, remove its
reviewed file at `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/cantrip.service`,
and run `systemctl --user daemon-reload`. Inspect whether a lower-priority
packaged unit becomes visible. Remove only drop-ins you deliberately created;
keep the binary if bindings still use it. Configuration, models, keyring
entries, runtime leftovers, and transcript history remain untouched.

## CLI

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon |
| `cantrip hud [--screenshot PATH]` | Run the layer-shell status HUD (or dump one frame to a PNG and exit). The daemon spawns and watches it; run manually only to override |
| `cantrip settings [--screenshot PATH]` | Open the configuration window (view, edit, reload; or dump a frame) |
| `cantrip actions [--doctor] [--screenshot PATH]` | Open the recording recovery and setup window |
| `cantrip toggle` / `start` / `stop` / `cancel` | Dictation transitions |
| `cantrip status [--json]` / `ping` | Current state, measured progress, outcome, and capabilities / daemon liveness |
| `cantrip transcribe [--local] <wav>` | One-shot file transcription; transcript-only stdout, diagnostics on stderr |
| `cantrip models pull` / `status` | Manage local STT models |
| `cantrip config show` / `edit` / `init` / `path` | Inspect and edit configuration |
| `cantrip key set` / `rm` / `status <id>` | Store and manage keyring credential ids |
| `cantrip doctor` | Environment report |
| `cantrip recordings [--json]` | List canonical recording IDs, durations, and available artifacts; no transcript text |
| `cantrip copy <id>` | Copy this exact recording's saved transcript; never send keys |
| `cantrip last` | Re-deliver the latest saved transcript, selected once when accepted |
| `cantrip recover [--id ID] [--local] [--clipboard]` | Retry this recording; omitted ID selects the newest unresolved take with retained audio |
| `cantrip dismiss [--event-id ID]` | Acknowledge a notice without deleting recordings |
| `cantrip forget <id> --yes` | Delete retained audio and incomplete text; keep complete archived text |
| `cantrip reload` | Re-read configuration in the running daemon |

Two hotkeys, one with cleanup and one without: `toggle` and `start` take
`--postproc clean|raw` to force transcript cleanup on or off for that
dictation, overriding `[postproc].enabled`. Bind one key to
`cantrip toggle --postproc clean` and the other to
`cantrip toggle --postproc raw`; each key starts and stops its own dictation
mode (cleanup runs only when the capture was started with `clean`). Without
the flag, `[postproc].enabled` decides.

## Recovery

Open `cantrip actions` to select a recording by its capture time and ID. Copy,
recover, and confirmed Forget always target that selection, even if newer takes
arrive. Keyboard navigation supports arrows, Page Up/Down, and Home/End.
Escape closes the window or confirmation without deleting a recording.

For the CLI, list metadata and choose an exact ID:

```sh
cantrip recordings
cantrip recover --id RECORDING_ID --clipboard
cantrip copy RECORDING_ID           # copy already-saved text without re-transcribing
```

For local recovery without changing your usual configuration:

```sh
cantrip models pull                 # once, if Parakeet is not installed
cantrip recover --id RECORDING_ID --local --clipboard
```

Paste with Ctrl+V in a GUI or Ctrl+Shift+V in a terminal. `--local` uses installed
Parakeet and disables cleanup for that operation; it does not change separately
opted-in count-only telemetry. A partial result is visibly marked incomplete
and leaves the audio available. Retrying processes the whole recording again.
Automatic local fallback keeps your configured cleanup policy; explicit
`--local` disables cleanup. `cantrip doctor` reports local fallback readiness.

Dismissal only acknowledges feedback. Successful delivery marks a take resolved
but keeps its audio. Only confirmed Forget removes that take's audio and incomplete
text, retaining any complete archived transcript. Retained audio costs about
1.92 MB per recorded minute (115 MB/hour), with no silent expiry.
Partial or uncertain delivery is not a successful retry: inspect the destination
before trying again to avoid duplicates.

For an audio file outside the managed history:

```sh
(umask 077; cantrip transcribe --local recording.wav > recovered.txt)
```

The CLI exits unsuccessfully if only a partial transcript was produced, while
still printing the available text. Keep the original file until satisfied.

## Omarchy integration

The repository includes a status badge and menu route. Deploy only while attending
the unlocked graphical session; review the dry run first:

```sh
python3 integrations/omarchy/install.py
python3 integrations/omarchy/install.py --apply
omarchy menu summon cantrip
```

Live `--apply` fails closed unless bounded, read-only probes identify the same
Hyprland/Omarchy session and both explicitly report unlocked, with no requested
or pending lock. Locked, unavailable, ambiguous, malformed, or timed-out state
refuses installation before staging or backups. Run from a terminal in that
graphical session: the installer will not guess a display from SSH/TTY, disable
locking, unlock automatically, or provide a live bypass. Passing `--config-dir`
for the live configuration does not skip these checks.

The installer checks again before publishing the staged plugin and each changed
configuration file. If safety changes during staging/publication, existing
rollback preserves installed content (and may retain private backups). These
checks are defense in depth, **not a guarantee against a check-to-lock race**:
only the shell can coordinate hot reload with locking and fully close that race.
Do not use unattended live deployment. Dry runs and already-current no-ops remain
non-mutating without requiring a session probe; a distinct offline `--config-dir`
fixture can still be installed without a running desktop.

If replacing an existing personal badge, add `--replace-widget OLD_PLUGIN_ID`
to both installer commands. Left-click keeps raw dictation; right-click opens
`cantrip actions`. Existing hotkeys are not redefined. Status failures show
unknown state, not Ready; the badge retains only the last confirmed pending count.

The installer preserves unrelated shell/menu content, stages complete plugin
updates, and reports private rollback backups. To roll back, disable
`cantrip.dictation`, restore the previous bar widget, and remove only the managed
Cantrip menu block. Move the plugin directory outside `omarchy/plugins` rather
than deleting personal extras. Never restore whole backups over subsequent edits.

## Configuration

Everything lives in `~/.config/cantrip/config.toml` (or `cantrip config path`);
`cantrip config show` prints the active file, `config edit` opens it, and
`cantrip settings` opens a window you can keep open to view and adjust it
(Save reloads the daemon). The gauntlet-informed recommended setup and every
knob (STT model, cloud STT, postproc model + instructions, cloud postproc) are
documented in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md).

## Privacy

- Audio and transcripts never leave the machine in the default local lanes.
- In-flight recordings live in `$XDG_RUNTIME_DIR/cantrip` (tmpfs, per-user
  `0700`). Before transcription, each stopped take is retained under its own ID
  in `$XDG_STATE_HOME/cantrip/transcripts` as owner-only recovery audio.
  Every stopped take, including successful, cancelled, empty, and undelivered
  takes, remains independently available until explicit confirmed Forget.
  Graceful shutdown stops and retains live capture; startup imports trusted,
  finalized runtime leftovers under their original IDs. Runtime originals are
  consumed only after matching durable audio is confirmed.
  Storage failures are surfaced. Active or runtime-only audio can be lost on
  reboot or power failure; no recognition or storage guarantee overrides that.
  Legacy `last-failed.wav` and `last-transcript.txt` migrate independently and
  idempotently, with originals consumed only after durable publication.
- Every successful STT result is saved locally as an owner-only JSON record in
  `$XDG_STATE_HOME/cantrip/transcripts` (normally
  `~/.local/state/cantrip/transcripts`). This history contains sensitive text;
  it is never uploaded or committed automatically.
- Operational logs contain character counts only—never transcript content.
  The single stdout exemption is `cantrip transcribe`.
- Clipboard mode overwrites the clipboard and does not restore the previous
  contents (restoring is racy on Wayland).

## Evaluation gauntlet

`examples/eval` is a reproducible harness that scores any configured STT and
post-proc lane over a 5-clip reference set (WER/CER, latency, cost) and ranks
arrangements. Reproduction procedures, accepted evaluation contracts, and clearly
separated historical findings/design proposals: [`docs/EVALUATION.md`](docs/EVALUATION.md).

## Development

The exact Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml).
With rustup installed, enter the checkout and install that toolchain with:

```sh
rustup install
```

```sh
cargo build --locked
./scripts/check
```

`scripts/check` is the clone-to-green command: it runs `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`,
the offline `cargo test --locked --example eval` suite, and installer/release
safety tests (Python 3.11+), stopping at the first red gate. The example tests
do not contact providers.

- **Local git hooks** (format + clippy on commit, tests + secret scan on push):
  `.githooks/install.sh`. After installing hooks, `gitleaks` and `trufflehog`
  must be on `PATH`; the pre-commit and pre-push hooks fail closed when either
  scanner is missing instead of skipping the scan.
- **CI** (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, tests, secret
  scan, and on `master` the Landmark-prepared verified Linux release.
- Architecture decisions: [`docs/adr/`](docs/adr/). Log tags: `[Daemon]`
  `[Capture]` `[STT]` `[Postproc]` `[Inject]` `[Models]` `[HUD]`.

## Work and documentation ownership

Work starts from the user's current request, checked against live code and
overlapping work. Linear owns current work, prioritization, and selected unresolved
opportunities; neither an old issue nor a document authorizes automatic intake.
Powder is retired. [ADR 0017](docs/adr/0017-powder-board-of-record.md) preserves
the earlier migration and its rejection of duplicate boards, not live routing.
GitHub issue history remains context rather than a second backlog.

The repository owns version-bound product/system contracts, accepted technical
decisions, portable procedures, and curated public/synthetic eval inputs and
baselines. Linear holds safe work summaries and links to proof; raw, large, or
sensitive run output belongs in approved retained artifact storage, subject to
the existing privacy boundary. Owner-private recording history stays local and
never becomes a repository fixture or work attachment automatically.
[VISION.md](VISION.md) is optional product rationale, not mandatory reading or
a higher authority than the request.

## Prior art & credits

Design informed by [Handy](https://github.com/cjpais/Handy) (and its
`transcribe-rs` engine, which Cantrip uses directly) and by
[Vox](https://github.com/misty-step/vox), our macOS predecessor whose pipeline
architecture and privacy rules carry over.

## License

[MIT](LICENSE) © Misty Step.

## Docs

- [Configuration](docs/CONFIGURATION.md)
- [Evaluation gauntlet](docs/EVALUATION.md)
- [Architecture decisions](docs/adr/) — chronological rationale, not a flat list
  of current implementation requirements. Older decisions retain their original
  evidence and link forward where superseded.
  - [Per-take recovery and verified delivery](docs/adr/0019-per-take-recovery-and-verified-delivery.md),
    refined by [local completion and explicit audio deletion](docs/adr/0020-local-completion-and-explicit-audio-deletion.md)
    and the [signed waveform contract](docs/adr/0021-signed-pixel-waveform.md).
  - [Private history and fixture promotion](docs/adr/0013-local-transcript-history.md)
    and [eval-driven post-processing](docs/adr/0012-eval-driven-postprocessing.md).
- Marketing/docs site scaffold: [`site/`](site/)
