# ADR 0015: Command acknowledgements and status snapshots use distinct IPC types

Date: 2026-08-15. Status: accepted.

## Problem

The Unix-socket protocol used one public Rust `Reply` type for two different
contracts:

- command acknowledgements report success, a message, and the daemon's current
  state;
- `status` reports the complete observable payload for that state.

These contracts are not interchangeable. For example, `start` can acknowledge
`state: "recording"` without elapsed time or audio telemetry, while a recording
status snapshot supplies elapsed time and an optional complete audio signal.
The shared flat type forced every command reply to populate unrelated status
fields with `None`. HUD and settings callers then reconstructed state from a
string and several optional fields.

Keeping `Status` in `Command` would preserve that ambiguity because
`send_command(Command::Status)` could still request the status view through the
command interface.

## Decision

The Rust IPC boundary separates requests before dispatch:

- `Request::Command(Command)` carries an operational command;
- `Request::Status` carries the status query;
- `Command` has no `Status` variant.

Clients use two concrete interfaces:

- `send_command(Command) -> CommandReply` returns command success, current
  `StateKind`, message, optional stage, and optional terminal outcome;
- `status() -> StatusSnapshot` returns `Idle`, `Recording`, `Processing`, or
  `Unknown` with only the payload valid for that state.

A recording snapshot groups level, silence, and waveform into one optional
`AudioSignal`. The group is present only when all three wire fields are present.
A partial group is a malformed status reply and returns an IPC error.

`ipc` owns the crate-private `WireReply`, exact JSON serialization,
deserialization defaults, state-name conversion, and the two public views. The
daemon owns runtime state. The HUD and settings window consume
`StatusSnapshot`; they do not interpret the flat wire record.

## Compatibility policy

The socket command strings and flat JSON field names remain unchanged. Command
replies continue to serialize status-only fields as null, so existing clients
keep their current input shape.

A new client accepts older daemon replies that omit fields added by ADRs 0006
and 0014:

- missing recording elapsed time defaults to zero;
- missing audio fields mean that live monitoring is unavailable;
- missing processing stage defaults to `transcribing`;
- a terminal message without `last_ok` is a notice, not a delivered dictation.

Known command states use `StateKind`. An unrecognized command state is retained
as `StateKind::Unknown(String)`. An unrecognized status state becomes
`StatusSnapshot::Unknown`, retaining its name and terminal outcome. Settings can
show the name, the CLI can print it, and an older HUD treats it as idle instead
of rejecting the reply or displaying a false active state.

Command acknowledgements may name `recording` or `processing` without status
telemetry. Only `status()` validates and constructs a state-specific snapshot.

## Why not alternatives

- **Keep one public flat reply and add constructors:** removes literal
  boilerplate but leaves every caller responsible for command-versus-status
  meaning and legal field combinations.
- **Use one tagged reply enum for every response:** cannot honestly represent a
  command acknowledgement that names an active state but carries no telemetry.
- **Keep `Command::Status` with a typed `status()` helper:** still permits the
  wrong interface at compile time and preserves two paths for one query.
- **Version the protocol or add command tags to replies:** unnecessary because
  the existing request string tells the client which view it requested and the
  current JSON can represent both contracts.
- **Reject unknown states:** breaks rolling daemon/client replacement. An older
  client must remain safe when a newer daemon adds a state.

## Consequences

Adding status telemetry changes the relevant `StatusSnapshot` variant and its
producer instead of every command reply. Command call sites cannot request
status. HUD and settings code receive a validated status representation.
Daemon command replies no longer repeat audio-field initialization.

The private wire conversion remains necessary for compatibility. It must stay a
small boundary, not become a version-negotiation framework or a generic request
abstraction.

Normal recording, stop, cancellation, STT, injection, recovery, HUD polling,
privacy, and `SIGINT` shutdown behavior remain unchanged. The design adds no
protocol version, configuration, dependency, thread, or runtime component.

## Verification

The implementation is accepted with these checks:

- request parsing proves that `status` becomes `Request::Status` and cannot be a
  `Command`;
- command fixtures accept active state names with absent telemetry;
- status fixtures cover idle, recording with and without signal, processing,
  legacy missing fields, incomplete audio rejection, and an unknown future
  state;
- the HUD maps an unknown future state to idle;
- the complete Rust test suite passes;
- Clippy passes for all targets with warnings denied;
- an isolated daemon answers both `cantrip status` and `cantrip ping` through
  their distinct client paths.
