# Real-path verification helpers (Linux X11 session)

Used to verify v2 on a real desktop session with the compiled binary; see
ARCHITECTURE.md › Verification boundaries.

- `receiver.py NAME LOG`: a GTK3 window with one entry that logs every value
  change (the "receiver A/B" of DESIGN.md's delivery checklist).
- `openrouter_stub.py PORT SCENARIO LOG`: a loopback OpenRouter stub
  (`ok`, `llm500`, `llmempty`, `llmlength`, `llmslow`, `stt500`, `sttempty`).
  It logs each request's non-secret payload fields. Point Cantrip at it with
  `CANTRIP_OPENROUTER_BASE=http://127.0.0.1:PORT/api/v1`.
- `hud_latency.py N`: presses the real shortcut N times with XTEST. It
  measures press → HUD on-screen and Escape → HUD hidden / audio deleted, and
  checks that a tap keeps listening. Run it against a running `cantrip`.

Example runs:

```sh
cantrip e2e dictate clip.wav --mode clean --key-env    # live, key from env
cantrip e2e meeting you.wav others.wav --folder DIR --key-env
```

To feed a real-app take, create a PipeWire virtual mic and play a clip into it:

```sh
pw-loopback -m '[ FL ]' \
  --capture-props='media.class=Audio/Sink node.name=cantrip_test_mic_in audio.channels=1 audio.position=[MONO]' \
  --playback-props='media.class=Audio/Source node.name=cantrip_test_mic node.description="Cantrip Test Mic" audio.channels=1 audio.position=[MONO]' &
pw-play --target cantrip_test_mic_in clip.wav
```
