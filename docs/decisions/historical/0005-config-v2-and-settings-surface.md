# ADR 0005: Config schema v2 and the settings surface

Date: 2026-08-03. Status: accepted.

## Schema

One TOML file stays the single source of truth
(`~/.config/cantrip/config.toml`), extended with two tables:

```toml
injection = "auto"        # auto | type | clipboard
keep_warm = true
# audio_source = "…"      # optional PipeWire target
vocabulary = []           # exact-spelling terms for postproc + cloud STT

[stt]
model = "parakeet-tdt-0.6b-v3-int8"   # local registry name
# Cloud STT: set endpoint to any OpenAI-compatible base URL and the
# engine switches to POST {endpoint}/audio/transcriptions (multipart).
# endpoint = "https://api.groq.com/openai/v1"
# model = "whisper-large-v3-turbo"
# api_key_id = "groq"

[postproc]
enabled = false
endpoint = "http://localhost:11434/v1"
model = ""                # required when enabled
# api_key_id = "openai"   # keyring entry; omit for local endpoints
timeout_ms = 10000
instructions = ""         # optional extra style guidance
```

`endpoint` presence selects cloud; there is no separate `backend` enum to
drift out of sync. Local `stt.model` is validated against the model
registry (`models::spec`) so a typo fails with the valid names, not a
mysterious missing-dir error. `enabled = true` with an empty
`postproc.model` is a load-time error.

## API keys: OS keyring, never files

Config stores only `api_key_id` labels. Secrets live in the OS keyring
(Secret Service) under service `cantrip` via the `keyring` crate's
pure-Rust `async-secret-service` + `async-io` feature (zbus, no libdbus C dependency; the keyring API stays blocking) — no key bytes
in TOML, logs, or `config show` output. Managed by `cantrip key set|rm|status <id>`
(set reads the secret from stdin).

## Settings surface: CLI now, GTK later

GTK remains uninstallable this milestone (ADR 0002), so the settings
"page" is the CLI over the same file:

- `cantrip config path` — where the file lives
- `cantrip config show` — effective config, defaults filled in
- `cantrip config init` — write the commented template above
- `cantrip config edit` — open `$EDITOR`, then validate and report errors
- `cantrip reload` — new IPC command; the daemon re-reads the file and
  keeps the old config if the new one fails to parse or validate

The future GTK settings window edits the same TOML and sends the same
`reload` — no daemon changes required, per ADR 0002.

## Reload semantics

The serve loop owns the `Config` by value; a reload swaps it, and a file
that fails to parse or validate is rejected while the old config stays
active. A reload never touches the recorder process. Each stage reads
the newest config at its own boundary: the job snapshots stt,
vocabulary, and postproc settings when recording stops, and injection
mode is read when the transcript comes back — latest config wins per
stage. The worker caches the loaded local model keyed by model name and
reloads lazily when a job names a different one; `keep_warm` controls
startup preloading only. Startup requires an installed local model only
when `stt.endpoint` is unset.
