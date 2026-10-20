# Talk to a T3 agent thread

`cantrip_t3.py` turns a Cantrip [handoff](../../docs/CONFIGURATION.md#handoffname--named-local-commands-us-011)
into a spoken conversation with one T3 thread (US-015). It is standard-library
Python 3.11+ with no install step beyond copying the file.

- `send` is the handoff command. It posts the transcript to the thread with
  `t3_thread_send` in `queue` mode, so each take gets its own reply run.
  `clientRequestId` is `cantrip-voice-<CANTRIP_TAKE_ID>`, so a retried or
  recovered take never creates a second message. T3 scopes that key to the
  OAuth client, so re-authorizing with a new client can repost an old take. It
  writes the pending record first and exits once T3 acknowledges. Each network
  step is bounded (6 s to connect, 15 s to send) to fit the handoff's 30-second
  timeout; if a slow DNS lookup still overruns it, the record is already in place
  and the collector finds the message once it appears.
- `collect` is a user service. For each sent take it finds the reply run, waits
  for it, and speaks the run's final assistant message once. One failing take
  never blocks the others: after three refusals from T3 it is given up with a
  notification, as is a reply still missing after six hours. A send that was
  never acknowledged keeps being looked for until then, since a queued message
  joins the timeline only when its turn starts. A run waiting for your input in the thread raises
  one notification. If T3 refuses the sign-in, the collector notifies you and
  exits so systemd restarts it with a renewed token.
- `talk` stops speech, then runs `cantrip toggle --handoff <handoff>`.
- `mute [on|off|toggle]` switches reply speech; muting also stops it.
  `stop` silences the current reply, including one held while you record.
  `repeat` speaks the last reply again, fetched from T3. Only one reply speaks
  at a time. `status` prints mute state, the pending count and the last error
  code, which is otherwise invisible because Cantrip discards handoff stderr.

Replies are spoken with an OpenAI-compatible `/audio/speech` voice when
`CANTRIP_T3_SPEECH_KEY` is set, falling back to local `espeak-ng` when that
voice fails or stalls for 20 seconds. Markdown becomes plain sentences and long
replies are cut at about 6,000 characters. Speech waits while Cantrip is
recording and stops if a take starts. A reply that finished more than 15
minutes earlier, for example while the service was down, is not spoken; a
desktop notification points to the thread instead.

Privacy: the transcript goes only to the T3 thread, and reply text only to the
configured speech endpoint. State under `$XDG_STATE_HOME/cantrip-t3` holds
take, message and run IDs, timestamps, counts and error codes, never text.
Logs carry the same. Requests never follow redirects, so a bearer token
cannot be forwarded to another host.

## Setup

Install the scripts somewhere on `PATH`, keeping them together, because the
hands-free module imports `cantrip_t3.py` from its own directory:

```sh
install -D -m 755 integrations/t3/cantrip_t3.py ~/.local/lib/cantrip-t3/cantrip_t3.py
ln -s ~/.local/lib/cantrip-t3/cantrip_t3.py ~/.local/bin/cantrip-t3
```

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

## Hands-free turn-taking (OpenRouter)

`cantrip_t3_converse.py` keeps the microphone open and takes turns with the
thread without a key per utterance. Local energy endpointing ends an utterance
after 1 s of quiet. The utterance is transcribed by OpenRouter's JSON
`/audio/transcriptions` endpoint (Parakeet by default) on a worker thread, so
capture never stalls, and sent with `cantrip_t3.send` as its own take. The
running collector then speaks the reply exactly as for push-to-talk. Audio
leaves the machine only for detected speech; nothing is retained.

- **Speakers (default):** the microphone is ignored while a reply plays and
  for 0.6 s after, so the agent never hears its own voice. Speech already under
  way is protected: a reply waits until you finish speaking. Use `stop` or the
  mute key to cut a reply.
- **Headphones** (`headphones = true`): speaking over a reply stops it
  (barge-in).
- **Background sound:** the noise floor follows quiet quickly and rises
  slowly through sustained sound. A steady sound such as a fan is recognized
  by its even level, never uploaded, and becomes the new background. Typing
  never counts, because speech needs 150 ms of continuous sound. Speech must be
  about three times louder than the background, so use push-to-talk in a loud
  room or with a TV talking.
- While a Cantrip take records (push-to-talk), hands-free ignores the
  microphone, so nothing is sent twice.
- A transient transcription failure asks you to say it again. A refused key
  or model (401/402/403/404) stops the session with a notice, and a refused key
  is caught at start. If a send may have reached the thread, you are told to
  wait for the reply instead of repeating.
- It ends after `idle_minutes` with no speech, no reply playing and no reply
  owed, or on SIGTERM.

```toml
[converse]            # in t3.toml; these are the defaults
stt_endpoint = "https://openrouter.ai/api/v1"
stt_model = "nvidia/parakeet-tdt-0.6b-v3"
language = "en"
end_silence_ms = 1000
min_speech_ms = 350
max_utterance_s = 60
idle_minutes = 5
headphones = false
```

Measured on Linux with OpenRouter: end of speech to the reply's first word
took 5.8 s against a fast, idle agent. That was endpointing (now 1 s), about
0.7 s of STT, a 3.5 s agent run, and about 1.4 s to first speech. A busy agent
adds its queue and run time, which dominated QA's 15–83 s measurements. OpenRouter offers no realtime or full-duplex voice API, so
this is turn-taking, not a live conversation. Install
[`cantrip-t3-converse.service`](cantrip-t3-converse.service) with the same
speech key entry as `cantrip-t3.service`, and bind a key to start and stop it.
