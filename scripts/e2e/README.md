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

- `sway_check.py BINARY OUTDIR [MEDIA]`: the full live Sway check (HUD
  placement on workspaces 1 and 2, focus, click-through with `VPTR`, Escape
  cancel, hold and tap takes through the stub, Settings rebind, nothing left
  bound after quit). `SHOT_PREFIX` prefixes its screenshot names.
- `sway_ipc_check.py BINARY OUTDIR latency N|reload|include`: Sway shortcut
  checks timed on Sway's own `binding` events (a separate IPC subscriber):
  binding event → HUD pill visible via `grim` over N taps; recovery after
  `swaymsg reload`; user bindings in an `include`d file left intact (run with
  a config whose included file binds `Ctrl+Alt+space` and `Escape` to
  `exec touch $MARKER_DIR/chord|escape`).
- `escape_check.py DISPLAY BINARY LABEL OUTDIR [next]`: launches the app with
  the stub, focuses a receiver, taps the shortcut, presses plain Escape and
  reports HUD hide / staged-audio deletion times and stub requests (with
  `next`, also what the next press sends). Run it on a stock Openbox session
  (e.g. `Xephyr :2` + `openbox --config-file /etc/xdg/openbox/rc.xml`), whose
  `Alt+Escape` binding broke an `AnyModifier` Escape grab.
- `llm500_check.py DISPLAY BINARY LABEL OUTDIR none|receiver-moved`: a take
  with rewrite HTTP 500 on an uncertain target (nothing focused) or a focus
  change; reads the HUD's accessible text (`hud_atspi.py`) before and after a
  foreign copy and the clipboard.
- `fsync_fault.c`: `LD_PRELOAD` shim that fails `fsync` of one directory with
  `EIO` while a flag file exists and logs every flush; with
  `cantrip e2e meeting` / `cantrip e2e meeting-retry` it proves staging stays
  until the whole Meeting-folder chain flushes.
- `slow_write.c`: `LD_PRELOAD` slow-disk shim. The first `write(2)` to each
  file under `$SLOW_WRITE_PREFIX` (the Meeting folder, including an unnamed
  `O_TMPFILE` inode there) writes half, then sleeps `$SLOW_WRITE_MS` (1500).
  `SLOW_WRITE_NO_TMPFILE=1` refuses `O_TMPFILE` with `EOPNOTSUPP` (the
  direct-create fallback); `SLOW_WRITE_RACE=1` creates the first `linkat`
  destination just before the call (a racing writer → `EEXIST`).
- `publish_watch.py FOLDER LOG STOPFILE`: scans the Meeting folder in a tight
  loop and on inotify events, logging every distinct (path, size, SHA-256) it
  sees. With `slow_write.c` it shows a note is only ever absent or whole.
- `fake_logind.py LOG`: a fake `org.freedesktop.login1` on a private bus
  (`FAKE_BUS`); logs inhibitor locks and their release, and emits
  `PrepareForSleep(true)` on `SIGUSR1`. Point the app at it with
  `DBUS_SYSTEM_BUS_ADDRESS`.

Example runs:

```sh
cantrip e2e dictate clip.wav --mode clean --key-env    # live, key from env
cantrip e2e meeting you.wav others.wav --folder DIR --key-env
CANTRIP_STAGING=ROOT cantrip e2e meeting-retry --folder DIR --key-env
```

To feed a real-app take, create a PipeWire virtual mic and play a clip into it:

```sh
pw-loopback -m '[ FL ]' \
  --capture-props='media.class=Audio/Sink node.name=cantrip_test_mic_in audio.channels=1 audio.position=[MONO]' \
  --playback-props='media.class=Audio/Source node.name=cantrip_test_mic node.description="Cantrip Test Mic" audio.channels=1 audio.position=[MONO]' &
pw-play --target cantrip_test_mic_in clip.wav
```
