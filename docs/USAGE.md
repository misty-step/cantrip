# Use Cantrip

Press your dictation shortcut once to start, speak, then press it again to stop.
Cantrip finishes transcription before delivering one result. Automatic keyboard
delivery is guarded; when the destination cannot be verified, use deliberate
copy and manual paste instead.

Start with [installation](INSTALLATION.md). The examples below use the default
installed binary, `"$HOME/.local/bin/cantrip"`; replace that path everywhere if
you selected another prefix. In command descriptions, `cantrip` is shorthand
for that executable, not an assumption that installation changed your `PATH`.

## First dictation

Use an attended, unlocked Wayland session and a harmless sentence. **Stopped
audio and plaintext transcript history are retained**, including successful and
cancelled takes. Read [privacy and Forget](PRIVACY.md) before speaking sensitive
material. No cloud account or cleanup service is needed for this first local
trial.

### 1. Prepare configuration in Terminal A

On a **first installation only**, create the annotated default configuration:

```sh
"$HOME/.local/bin/cantrip" config init
"$HOME/.local/bin/cantrip" config edit
```

The second command opens your editor. Save and exit it before continuing.
`settings` opens a native configuration window if you prefer. If configuration
already exists, skip `init`; inspect and edit the existing file rather than
replacing it. Keep its XDG location, model settings, and credential ids intact.

For this first local trial:

- Keep `[stt].model = "parakeet-tdt-0.6b-v3-int8"` and no `[stt].endpoint`.
- Keep `[postproc].enabled = false`. Leave cleanup off until you have explicitly
  configured a usable endpoint and model.
- Leave telemetry disabled.
- Choose **one delivery path** before starting the daemon:
  - **Supported direct Hyprland/logind desktop:** keep the default
    `injection = "auto"` and follow the hotkey/editor path below. The destination
    editor must accept `Ctrl+Shift+V` as Paste; otherwise choose clipboard mode.
  - **Other or unverified desktop, or manual paste by preference:** set the
    top-level `injection = "clipboard"` and follow the clipboard path below.
    Cantrip will not send keyboard input.

These are choices for a fresh local setup, not an instruction to overwrite an
existing operator's cloud or delivery configuration. See
[configuration](CONFIGURATION.md) and [desktop support](DESKTOP.md#supported-desktops)
for the boundaries.

### 2. Download the local model deliberately

Still in Terminal A:

```sh
"$HOME/.local/bin/cantrip" doctor
"$HOME/.local/bin/cantrip" models pull
"$HOME/.local/bin/cantrip" models status
"$HOME/.local/bin/cantrip" doctor
```

`models pull` is the explicit network/download step; the weights are not bundled
with the binary. Once installed, default local transcription runs on CPU without
network access. Read and address every applicable `doctor` finding. Before you
start a daemon, “not running or unreachable” is expected. A successful `doctor`
exit does **not** mean that the microphone, endpoint, HUD, or destination works.

Optionally, while in the extracted release directory, isolate local recognition
using the bundled public sample:

```sh
"$HOME/.local/bin/cantrip" transcribe --local ./sample.wav
```

This prints the recognized text and saves its local history. It does not exercise
your microphone, shortcut, HUD, or desktop delivery.

### 3. Start one daemon in Terminal A

For the automatic-delivery path, first add an unused shortcut using
[the desktop shortcut procedure](DESKTOP.md#dictation-shortcut). Its command is
the absolute installed executable path followed by `toggle`, not a command that
opens a terminal.

If a service or personal autostart already owns Cantrip, keep that owner and skip
the foreground command. Otherwise run:

```sh
"$HOME/.local/bin/cantrip" daemon
```

**Terminal A is now occupied by the daemon.** Leave it open. Do not paste the
next commands into it or start a second daemon. Optional
[login startup](DESKTOP.md#user-service-graphical-session) comes after the first
attended trial, not before it.

Open **Terminal B in the same graphical user session**:

```sh
"$HOME/.local/bin/cantrip" ping
"$HOME/.local/bin/cantrip" status --json
"$HOME/.local/bin/cantrip" doctor
```

Wait for successful IPC and address the prerequisite findings. `ping` proves
liveness; an idle state proves only that no recording is currently in progress.
Neither certifies dictation readiness.

### 4A. Dictate into an editor on supported Hyprland

This path requires the [verified direct Hyprland/logind support conditions](DESKTOP.md#supported-desktops),
not merely an available virtual-keyboard backend.

1. Open a safe text editor, create an empty scratch document, and put the cursor
   in its text area. Close launchers, menus, and other focus-taking overlays.
2. With that editor focused, **press and release your configured shortcut** to
   start. Speak a short sentence, such as “This is my first local dictation.”
   Observe whether the listening HUD responds to your actual microphone.
3. **Press and release the same shortcut while the editor is still focused**
   to stop. Leave the editor focused and the session unlocked throughout
   transcription and delivery. Do not switch to a terminal to issue `stop` or
   poll status: the delivery destination is selected when recording stops.
4. Wait for processing to settle, then inspect the actual text in the editor.
   The HUD's completion mark reports the delivery mechanism's acknowledgement,
   not proof that the editor accepted the sentence.
5. Only after delivery has settled, return to Terminal B to inspect the outcome:

   ```sh
   "$HOME/.local/bin/cantrip" status
   ```

A complete result **and the expected sentence in the intended editor** establish
a successful attended trial. A notice, no text, a partial result, or an uncertain
handoff is not success; use [the outcome guide](#understand-the-outcome) rather
than pressing the shortcut repeatedly.

### 4B. Dictate with explicit clipboard delivery

Choose this path only after setting `injection = "clipboard"` in step 1. If you
changed the file after the daemon started, apply it first with
`"$HOME/.local/bin/cantrip" reload`.

In Terminal B, run one command at a time:

```sh
"$HOME/.local/bin/cantrip" start
```

Speak a short sentence, then run:

```sh
"$HOME/.local/bin/cantrip" stop
"$HOME/.local/bin/cantrip" status
```

`stop` acknowledges processing; it does not wait for completed transcription.
Reissue `status` while processing. Once it reports a complete result with
`delivery: copied`, focus your safe editor and paste using its Paste command
(usually Ctrl+V in a GUI, Ctrl+Shift+V in a terminal). Inspect the sentence you
actually pasted. Do not paste blindly after a failed copy: the clipboard might
still contain something older.

You can later bind the same `toggle` command to an unused compositor shortcut
while keeping clipboard mode. The shortcut changes recording control, not the
delivery policy. Automatic typing is not required to use Cantrip.

## Everyday controls

- `toggle` starts recording when idle and stops the current recording.
- `start` and `stop` are separate commands for desktops that support key-down
  and key-up bindings. Do not use a repeating binding.
- `cancel` stops capture or processing without delivering or deleting audio.
  Cancellation prevents subsequent chunks, local fallback, cleanup requests,
  and delivery; an already-running provider request may need to return before
  the worker settles.
- `stop` ends a recording, **not the daemon**. For a foreground daemon, finish
  or cancel the take, let processing settle, then use Ctrl+C in Terminal A.
  Services and other owners have [their own stop procedure](DESKTOP.md#stop-update-and-remove).

`toggle` and `start` accept `--postproc clean|raw`. The mode chosen at capture
start overrides `[postproc].enabled` for that take; using a different flag to
stop does not change that take's mode. Without a flag, the configured policy
applies. Two shortcuts can run `toggle --postproc raw` and
`toggle --postproc clean`, but configure a working cleanup endpoint/model
[before using the clean shortcut](CONFIGURATION.md#postproc--cleanup).
An explicit clean request does not install or start an LLM service.

## Understand the outcome

`status` prints the last outcome's completeness, delivery, cleanup, event id,
and recording id when available. `status --json` provides the same typed state
and measured progress without transcript text. An idle state alone does not
mean the last take succeeded; command acceptance alone does not mean a queued
operation finished.

| Observation | What to do |
|---|---|
| `completeness: complete` and `delivery: pasted` or `typed` | Inspect the intended application. Mechanism acknowledgement is not application receipt. |
| `delivery: copied` | Text is on the clipboard; paste manually. No keyboard delivery occurred. |
| `delivery: deferred` | Focus or session safety could not be established. Select the take and explicitly copy it rather than bypassing the guard. |
| `delivery: uncertain` | The clipboard or some keys may already have changed. Inspect the destination before retrying to avoid duplicates; Cantrip does not automatically retry an uncertain handoff. |
| `completeness: partial` | Available text is incomplete. Keep the audio and recover the whole recording; a partial transcript is not a successful retry. |
| `completeness: empty`, `failed`, or `cancelled` | No complete dictation was delivered. Read the notice and use the matching recording's available artifacts. |
| `cleanup: failed` | The raw transcript is used rather than dropped. Repair cleanup configuration or leave it disabled. |
| Storage warning | Delivery and durable retention are separate. Inspect storage before relying on recovery or recording more. |
| Disconnected/unknown HUD or failed `ping` | Use desktop diagnostics; do not infer Ready or start a competing owner. |

The default HUD is quiet and wordless for ordinary stages. It shows measured
microphone activity, indeterminate processing activity, and measured progress
only when multi-chunk reports exist. Exceptions such as Copied, deferred,
uncertain, and failed outcomes remain explicit.
[Labels and reduced motion](CONFIGURATION.md#hud--passive-status) are configurable.

## Recovery

Open the native recovery window:

```sh
"$HOME/.local/bin/cantrip" actions
```

Select the recording by capture time and ID. Copy, recover, and confirmed Forget
always target that selection, even if newer takes arrive. Use **Include
completed history** for resolved recordings. Arrow keys, Page Up/Down, and
Home/End navigate the list. Escape closes the window or confirmation without
deleting a recording.

For the CLI, list metadata and choose an exact ID:

```sh
"$HOME/.local/bin/cantrip" recordings
"$HOME/.local/bin/cantrip" copy RECORDING_ID
```

Copy uses saved text without retranscribing and never sends keys. For a retry
with the configured transcription provider and cleanup policy, copying the
result rather than typing it:

```sh
"$HOME/.local/bin/cantrip" recover --id RECORDING_ID --clipboard
```

For local recovery without changing your usual configuration:

```sh
"$HOME/.local/bin/cantrip" models pull   # only if Parakeet is not installed
"$HOME/.local/bin/cantrip" recover --id RECORDING_ID --local --clipboard
```

The explicit `--local` operation uses installed Parakeet and skips cleanup;
separately opted-in metadata telemetry remains enabled. A retry processes the
whole recording again. Omit `--id` only when you deliberately want the newest
unresolved take with retained audio. `last` similarly selects the latest saved
transcript once when accepted; use an exact-ID copy when choosing a particular
take matters.

Wait for the recovery outcome and inspect completeness before manual paste.
Dismiss only acknowledges a notice. Successful delivery/recovery retains audio.
Only confirmed [Forget](PRIVACY.md#what-forget-deletes) removes that take's audio
and incomplete text; **complete archived text remains**.

## Transcribe an existing file

One-shot transcription does not need a running daemon. For local Parakeet,
provide a 16 kHz mono PCM16 WAV and keep the original file until satisfied:

```sh
(umask 077; "$HOME/.local/bin/cantrip" transcribe --local recording.wav > recovered.txt)
```

Choose a new output filename to avoid replacing a previous export. Transcript
text goes to stdout and diagnostics to stderr. The command exits unsuccessfully
when only a partial transcript was produced, while still printing available
text. A successful STT result is also saved to local transcript history. The
input file is not deleted by Forget. See
[transcription formats and cloud chunking](CONFIGURATION.md#stt--transcription)
for non-native WAV formats.

## CLI reference

Use the installed executable path in place of `cantrip` unless you have added
its directory to `PATH`. `cantrip COMMAND --help` describes each command's flags.

| Command | Purpose |
|---|---|
| `cantrip daemon [--preload]` | Run the dictation daemon |
| `cantrip hud [--screenshot PATH]` | Show the passive layer-shell HUD; the daemon normally spawns and watches it |
| `cantrip settings [--screenshot PATH]` | Open configuration editing and reload |
| `cantrip actions [--doctor] [--screenshot PATH]` | Open recording recovery and setup |
| `cantrip toggle` / `start` / `stop` / `cancel` | Recording and processing transitions |
| `cantrip status [--json]` / `ping` | State, progress, outcomes, and capabilities / daemon liveness |
| `cantrip transcribe [--local] WAV` | One-shot file transcription; text stdout, diagnostics stderr |
| `cantrip models pull` / `status` | Deliberately download or inspect the local model |
| `cantrip config show` / `edit` / `init` / `path` | Serialize effective file configuration, edit, create a missing file, or locate it |
| `cantrip key set ID` / `rm ID` / `status ID` | Manage OS-keyring credentials by id |
| `cantrip doctor` | Read the configuration and environment report; not a readiness exit code |
| `cantrip recordings [--json]` | List recording IDs, times, durations, and artifact availability without transcript text |
| `cantrip copy ID` | Copy this exact recording's saved transcript without sending keys |
| `cantrip last` | Re-deliver the latest saved transcript, selected once when accepted |
| `cantrip recover [--id ID] [--local] [--clipboard]` | Retry the selected take; omitted ID chooses the newest unresolved audio |
| `cantrip dismiss [--event-id ID]` | Acknowledge feedback without deleting recordings |
| `cantrip forget ID --yes` | Delete retained audio and incomplete text; keep complete archived text |
| `cantrip reload` | Re-read configuration in the running daemon |
