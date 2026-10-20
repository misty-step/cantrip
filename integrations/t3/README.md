# Talk to a T3 agent thread

`cantrip_t3.py` turns a Cantrip [handoff](../../docs/CONFIGURATION.md#handoffname--named-local-commands-us-011)
into a spoken conversation with one T3 thread (US-015). It is standard-library
Python 3.11+ with no install step beyond copying the file.

- `send` is the handoff command. It posts the transcript to the thread with
  `t3_thread_send` in `queue` mode, so each take gets its own reply run.
  `clientRequestId` is `cantrip-voice-<CANTRIP_TAKE_ID>`, so a retried or
  recovered take never creates a second message. It exits once T3 acknowledges.
- `collect` is a user service. For each sent take it finds the reply run, waits
  for it, and speaks the run's final assistant message once.
- `talk` stops speech, then runs `cantrip toggle --handoff <handoff>`.
- `mute [on|off|toggle]` switches reply speech; muting also stops it.
  `stop` silences the current reply. `repeat` speaks the last reply again,
  fetched from T3. `status` prints mute state and the pending count.

Replies are spoken with an OpenAI-compatible `/audio/speech` voice when
`CANTRIP_T3_SPEECH_KEY` is set, falling back to local `espeak-ng` when that
voice fails or stalls for 20 seconds. Markdown becomes plain sentences and long
replies are cut at about 6,000 characters. Speech waits while Cantrip is
recording and stops if a take starts. A reply that finished more than 15
minutes earlier, for example while the service was down, is not spoken; a
desktop notification points to the thread instead.

Privacy: the transcript goes only to the T3 thread, and reply text only to the
configured speech endpoint. State under `$XDG_STATE_HOME/cantrip-t3` holds
take, message and run IDs and timestamps, never text. Logs carry character
counts and error classes only.

## Setup

`$XDG_CONFIG_HOME/cantrip/t3.toml`:

```toml
mcp_url = "https://<environment-address>/mcp"   # T3 Settings → Connections → Copy MCP URL
thread_id = "<thread id>"
label = "Home"        # spoken in failure messages
handoff = "home"      # the [handoff.NAME] that `talk` toggles
prefix = "[voice] "   # marks voice messages for the agent

[speech]              # optional; these are the defaults
endpoint = "https://openrouter.ai/api/v1"
model = "microsoft/mai-voice-2.1-flash"
voice = "en-US-Iris:MAI-Voice-2.1-Flash"
local_voice = "en-us"
```

`T3_MCP_TOKEN` must be an OAuth access token for that MCP server, approved with
an access mode at least as broad as the target thread's runtime mode. Keep it
and the speech key in a secret store and inject them with a launcher. The
example below uses `pass-env`:

```toml
# ~/.config/cantrip/config.toml
[handoff.home]
command = ["/home/you/.local/bin/pass-env", "run", "-e", "T3_MCP_TOKEN=t3/home-mcp",
           "--", "/home/you/.local/bin/cantrip-t3", "send"]
timeout_seconds = 30
label = "Home"
```

Install [`cantrip-t3.service`](cantrip-t3.service) in `~/.config/systemd/user/`
with your launcher and entries, then run
`systemctl --user enable --now cantrip-t3.service`. Bind keys to
`cantrip-t3 talk`, `cantrip-t3 mute toggle` and `cantrip-t3 stop`.
