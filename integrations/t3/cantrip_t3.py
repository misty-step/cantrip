#!/usr/bin/python3 -I
"""Talk to a T3 agent thread through Cantrip and hear its final reply.

`send` is a Cantrip handoff command: it posts the transcript on stdin to one
T3 thread over authenticated HTTP MCP, idempotently on CANTRIP_TAKE_ID, and
exits as soon as T3 acknowledges. `collect` is a long-running user service that
waits for the reply run of each sent take and speaks its final assistant
message once. `mute`, `stop`, `repeat` and `talk` are keyboard actions.

Never logs transcript or reply text; state files hold IDs, timestamps, counts
and error codes only.
Secrets come from the environment (T3_MCP_TOKEN, CANTRIP_T3_SPEECH_KEY) so a
launcher such as pass-env owns credential lookup.
"""

from datetime import datetime
import fcntl
import json
import os
import queue
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import tomllib
import importlib.util
import urllib.error


def lazy_module(name):
    """Import on first attribute use: the bar polls `state` every few seconds and never needs HTTP."""
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.find_spec(name)
    spec.loader = importlib.util.LazyLoader(spec.loader)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


urllib.request = lazy_module("urllib.request")

PROTOCOL = "2025-06-18"
TERMINAL = {"completed", "failed", "cancelled", "interrupted", "rolled_back"}
WAIT_MS = 45_000
HTTP_SECONDS = 75
SEND_STEP_SECONDS = 6          # initialize and notification, inside the 30 s handoff timeout
SEND_SECONDS = 15
STALE_SECONDS = 15 * 60
SENDING_GRACE_SECONDS = 60     # leave a record alone while its handoff may still be sending
LOCATE_SECONDS = 10            # how often to look for an unacknowledged message
EXPIRE_SECONDS = 6 * 60 * 60   # stop waiting on a reply after this long
TOOL_FAILURES = 3              # consecutive T3 refusals before a take is given up
RECENT_ITEMS = 200
AUTH_EXIT = 75
SPOKEN_LIMIT = 6000
CHUNK_LIMIT = 1200
FIRST_CHUNK_LIMIT = 300
SPEECH_STALL_SECONDS = 20
PCM_RATE = 24_000

DEFAULTS = {
    "label": "Home",
    "prefix": "[voice] ",
    "handoff": "home",
    "speech": {
        "endpoint": "https://openrouter.ai/api/v1",
        "model": "microsoft/mai-voice-2.1-flash",
        "voice": "en-US-Iris:MAI-Voice-2.1-Flash",
        "local_voice": "en-us",
    },
}


def log(message):
    print("[T3Voice] " + message, file=sys.stderr, flush=True)


def config_path():
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "cantrip" / "t3.toml"


def load_config(path=None):
    path = Path(path) if path else config_path()
    with open(path, "rb") as handle:
        data = tomllib.load(handle)
    config = dict(DEFAULTS)
    config["speech"] = {**DEFAULTS["speech"], **data.pop("speech", {})}
    config.update(data)
    for key in ("mcp_url", "thread_id"):
        if not isinstance(config.get(key), str) or not config[key]:
            raise SystemExit("cantrip-t3: " + key + " is required in " + str(path))
    return config


def state_dir():
    base = os.environ.get("XDG_STATE_HOME") or str(Path.home() / ".local/state")
    path = Path(base) / "cantrip-t3"
    for child in ("pending", "done"):
        (path / child).mkdir(parents=True, exist_ok=True, mode=0o700)
    return path


def runtime_dir():
    base = os.environ.get("XDG_RUNTIME_DIR") or str(state_dir())
    path = Path(base) / "cantrip-t3"
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    return path


def write_json(path, value):
    temporary = path.with_name("." + path.name + "." + str(os.getpid()) + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True))
    os.replace(temporary, path)


def take_name(take):
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,128}", take or ""):
        raise ValueError("CANTRIP_TAKE_ID is missing or not a plain identifier")
    return take


def no_redirect():
    """A handler that refuses redirects, so a bearer token never follows one to another host."""
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None
    return NoRedirect()


_opener = []


def OPEN(request, timeout):
    if not _opener:
        _opener.append(urllib.request.build_opener(no_redirect()).open)
    return _opener[0](request, timeout=timeout)


class McpError(RuntimeError):
    def refused(self):
        """T3 refused this call for this record: a tool error, not transport or sign-in trouble."""
        return str(self).startswith("tool-")

    def unauthorized(self):
        return str(self) in ("http-401", "http-403")


class Mcp:
    """Minimal streamable-HTTP MCP client for one bearer-authenticated server."""

    def __init__(self, url, token, opener=None, timeout=HTTP_SECONDS):
        self.url = url
        self.token = token
        self.session = None
        self.sequence = 0
        self.open = opener or OPEN
        self.rpc("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                                "clientInfo": {"name": "cantrip-t3", "version": "1"}}, timeout=timeout)
        self.rpc("notifications/initialized", {}, notify=True, timeout=timeout)

    def rpc(self, method, params, notify=False, timeout=HTTP_SECONDS):
        payload = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notify:
            self.sequence += 1
            payload["id"] = self.sequence
        headers = {"Authorization": "Bearer " + self.token,
                   "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream",
                   "MCP-Protocol-Version": PROTOCOL}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        request = urllib.request.Request(self.url, data=json.dumps(payload).encode(), headers=headers)
        try:
            with self.open(request, timeout=timeout) as response:
                self.session = response.headers.get("Mcp-Session-Id", self.session)
                raw = response.read().decode()
        except urllib.error.HTTPError as error:
            raise McpError("http-" + str(error.code)) from None
        if notify or not raw.strip():
            return None
        if raw.lstrip().startswith("{"):
            frames = [json.loads(raw)]
        else:
            frames = [json.loads(line[5:]) for line in raw.splitlines() if line.startswith("data:")]
        data = next((frame for frame in frames if frame.get("id") == payload["id"]), None)
        if data is None:
            raise McpError("no-response")
        if "error" in data:
            raise McpError("rpc-" + str(data["error"].get("code")))
        return data.get("result")

    def call(self, name, arguments, timeout=HTTP_SECONDS):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments}, timeout=timeout)
        content = result.get("structuredContent")
        if content is None:
            texts = [block.get("text", "") for block in result.get("content", []) if block.get("type") == "text"]
            try:
                content = json.loads(texts[0]) if len(texts) == 1 else {}
            except ValueError:
                content = {}
        if result.get("isError"):
            code = content.get("code") if isinstance(content, dict) else None
            raise McpError("tool-" + (code or "error"))
        return content


def secret(name):
    """A secret from the environment: the first line only, as `pass` stores it."""
    value = os.environ.get(name, "").strip()
    return value.splitlines()[0].strip() if value else ""


def describe(error):
    """Loggable error description: our own codes, otherwise only the class (messages can echo headers)."""
    return str(error)[:40] if isinstance(error, McpError) else type(error).__name__


def connect(config, timeout=HTTP_SECONDS):
    token = secret("T3_MCP_TOKEN")
    if not token:
        raise SystemExit("cantrip-t3: T3_MCP_TOKEN is not set")
    return Mcp(config["mcp_url"], token, timeout=timeout)


# ── send ──────────────────────────────────────────────────────────────────


def request_id(take):
    return "cantrip-voice-" + take


def send(config, text, take, mcp=None, now=time.time):
    """Queue one message for this take. The pending record is written before T3 is called,
    so a handoff killed after T3 accepted the message still has its reply collected."""
    take = take_name(take)
    text = text.strip()
    if not text:
        raise ValueError("empty transcript")
    pending = state_dir() / "pending" / (take + ".json")
    done = state_dir() / "done" / (take + ".json")
    if done.exists():
        return json.loads(done.read_text())
    if mcp is None and not secret("T3_MCP_TOKEN"):
        raise ValueError("T3_MCP_TOKEN is not set")   # before any record: nothing can be sent
    record = {"take": take, "thread_id": config["thread_id"], "request_id": request_id(take),
              "sent_at": now(), "status": "sending"}
    created = not pending.exists()
    if created:
        write_json(pending, record)
    else:
        record = {**record, **json.loads(pending.read_text())}
    try:
        mcp = mcp or connect(config, timeout=SEND_STEP_SECONDS)
    except (McpError, OSError, ValueError) as error:
        if created:   # never reached t3_thread_send: nothing was posted
            pending.unlink(missing_ok=True)
        record_error("send", error)
        raise
    try:
        result = mcp.call("t3_thread_send", {"threadId": config["thread_id"],
                                             "message": config["prefix"] + text,
                                             "mode": "queue", "clientRequestId": request_id(take)},
                          timeout=SEND_SECONDS)
    except McpError as error:
        # A definite refusal of this take's first attempt posted nothing. An uncertain
        # failure, or a retry after an earlier attempt, stays pending for the collector.
        if created and (error.refused() or str(error).startswith(("http-4", "rpc-"))):
            pending.unlink(missing_ok=True)
        record_error("send", error)
        raise
    except OSError as error:
        record_error("send", error)
        raise
    record.update({"run_id": find_key(result, "runId") or record.get("run_id"),
                   "message_id": find_key(result, "messageId") or record.get("message_id"),
                   "status": "sent"})
    if not done.exists():   # the collector may already have finished this take
        write_json(pending, record)
    log("sent take=" + take + " chars=" + str(len(text)))
    return record


def record_error(stage, error):
    """Keep the last failure's class and code (never text) for `cantrip-t3 status`."""
    code = describe(error)
    write_json(state_dir() / "last-error.json", {"stage": stage, "code": code, "at": time.time()})


def find_key(value, key):
    if isinstance(value, dict):
        if isinstance(value.get(key), str):
            return value[key]
        for item in value.values():
            found = find_key(item, key)
            if found:
                return found
    elif isinstance(value, list):
        for item in value:
            found = find_key(item, key)
            if found:
                return found
    return None


# ── collect ───────────────────────────────────────────────────────────────


def read_items(mcp, thread_id, after=None, chars=50_000):
    """Thread items after position `after` (exclusive), or from the start when None."""
    while True:
        arguments = {"threadId": thread_id, "limit": 100, "maxCharsPerItem": chars}
        if after is not None:
            arguments["afterPosition"] = after
        page = mcp.call("t3_thread_read", arguments)
        items = page.get("items", [])
        yield from items
        if not page.get("hasMore") or not items:
            return
        after = page.get("nextPosition", items[-1]["position"])


def thread_head(mcp, thread_id):
    return mcp.call("t3_thread_read", {"threadId": thread_id, "limit": 1, "maxCharsPerItem": 1, "runLimit": 50})


def recent_items(mcp, thread_id, head=None, chars=50_000):
    head = head or thread_head(mcp, thread_id)
    after = int((head.get("thread") or {}).get("itemCount") or 0) - RECENT_ITEMS - 1
    return read_items(mcp, thread_id, after if after >= 0 else None, chars)


def full_text(mcp, thread_id, item):
    text = item.get("text") or ""
    offset = item.get("nextTextOffset")
    while offset:
        page = mcp.call("t3_thread_read", {"threadId": thread_id, "itemId": item["itemId"],
                                           "textOffset": offset, "maxCharsPerItem": 50_000})
        part = page.get("item") or next(iter(page.get("items", [])), {})
        text += part.get("text") or ""
        offset = part.get("nextTextOffset")
    return text


def locate_run(mcp, record):
    """Find the run that carries this take's message, for a send that was never acknowledged."""
    marker = "thread-send:" + record["request_id"]
    for item in recent_items(mcp, record["thread_id"], chars=1):
        message = item.get("messageId") or ""
        if item.get("type") == "user_message" and (message == record.get("message_id") or message.endswith(marker)):
            return item.get("runId")
    return None


def final_reply(mcp, record, head=None):
    """The run's last assistant message. Its position (not its text) is kept for `repeat`."""
    reply = None
    if record.get("reply_position") is not None:
        after = record["reply_position"] - 1
        page = mcp.call("t3_thread_read", {"threadId": record["thread_id"], "limit": 1, "maxCharsPerItem": 50_000,
                                           **({"afterPosition": after} if after >= 0 else {})})
        reply = next(iter(page.get("items", [])), None)
        if not reply or reply.get("runId") != record.get("run_id"):
            record.pop("reply_position")
            return final_reply(mcp, record, head)
    else:
        for item in recent_items(mcp, record["thread_id"], head):
            if item.get("runId") == record["run_id"] and item.get("type") == "assistant_message":
                reply = item
        if reply:
            record["reply_position"] = reply.get("position")
    return full_text(mcp, record["thread_id"], reply) if reply else ""


def completed_at(head, run_id):
    """The run's completion time from the thread's recent runs; None when unknown."""
    for run in head.get("recentRuns") or []:
        if run.get("runId") == run_id and run.get("completedAt"):
            try:
                return datetime.fromisoformat(run["completedAt"].replace("Z", "+00:00")).timestamp()
            except ValueError:
                return None
    return None


def advance(mcp, record, now=time.time, wait_ms=WAIT_MS):
    """Advance one pending take: ("wait", record, "") or (outcome, record, reply text)."""
    if not record.get("run_id"):
        # Unacknowledged send: the message may still be queued outside the timeline, so keep
        # looking (throttled) until the take expires.
        if now() - record.get("located_at", 0) < LOCATE_SECONDS:
            return ("wait", record, "")
        record["located_at"] = now()
        record["run_id"] = locate_run(mcp, record)
        if not record["run_id"]:
            return ("wait", record, "")
    waited = mcp.call("t3_thread_wait", {"threadId": record["thread_id"], "runId": record["run_id"],
                                         "timeoutMs": wait_ms}, timeout=wait_ms / 1000 + 30)
    status = find_key(waited, "status")
    if status not in TERMINAL:
        if status == "waiting" and not record.get("waiting_noticed"):
            record["waiting_noticed"] = True
            notify(record.get("label", "Home") + " is waiting for you in its thread.")
        return ("wait", record, "")
    record["run_status"] = status
    head = thread_head(mcp, record["thread_id"])
    record["finished_at"] = completed_at(head, record["run_id"]) or now()
    if status != "completed":
        return ("failed", record, "")
    return ("reply", record, final_reply(mcp, record, head))


REQUIRED = ("take", "thread_id", "request_id")


def readable(path):
    """A state record, or None for a damaged one."""
    try:
        record = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(record, dict) or not all(isinstance(record.get(key), str) for key in REQUIRED):
        return None
    return record


def pending_records():
    """Pending takes, oldest first; damaged records move aside once instead of failing every pass."""
    records = []
    for path in sorted((state_dir() / "pending").glob("*.json")):
        record = readable(path)
        if record is None:
            if not path.exists():
                continue   # finished or withdrawn by `send` since the listing
            damaged = state_dir() / "damaged"
            damaged.mkdir(exist_ok=True, mode=0o700)
            try:
                os.replace(path, damaged / path.name)
            except FileNotFoundError:
                continue
            log("damaged pending record moved aside " + path.name)
            continue
        records.append((path, record))
    return sorted(records, key=lambda pair: pair[1].get("sent_at") or 0)


def collect(config, once=False, mcp=None, speaker=None, now=time.time):
    with open(state_dir() / "collector.lock", "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("cantrip-t3: another collector is running") from None
        run_collector(config, once, mcp, speaker, now)


def run_collector(config, once, mcp, speaker, now):
    speaker = speaker or Speaker(config)
    backoff = 5
    while True:
        records = pending_records()
        if not records:
            if once:
                return
            time.sleep(1)
            continue
        try:
            mcp = mcp or connect(config)
            # With several takes pending, wait briefly on each so one long run never delays another's reply.
            wait_ms = WAIT_MS if len(records) == 1 else 3000
            for path, record in records:
                record["label"] = config.get("label", "Home")
                step(mcp, path, record, speaker, now, wait_ms)
            backoff = 5
            if not once:
                time.sleep(1)
        except McpError as error:
            if error.unauthorized():
                notify("T3 refused " + config.get("label", "Home") + "'s voice sign-in; spoken replies are paused.")
                record_error("collect", error)
                raise SystemExit(AUTH_EXIT)
            mcp, backoff = transport_failure(error, backoff, once)
        except (OSError, ValueError) as error:
            mcp, backoff = transport_failure(error, backoff, once)
        if once:
            return


def transport_failure(error, backoff, once):
    log("collector error class=" + type(error).__name__ + " code=" + describe(error))
    if not once:
        time.sleep(backoff)
    return None, min(backoff * 2, 120)


def save(path, record):
    """Write back a pending record without regressing what `send` wrote meanwhile."""
    try:
        current = json.loads(path.read_text())
    except (OSError, ValueError):
        return   # finished or removed meanwhile
    if current.get("status") == "sent" and record.get("status") != "sent":
        record.update({key: current[key] for key in ("status", "run_id", "message_id") if current.get(key)})
    write_json(path, record)


def step(mcp, path, record, speaker, now, wait_ms=WAIT_MS):
    """Advance one record; a refusal for this record never blocks the others."""
    if record.get("status") == "sending" and now() - record.get("sent_at", now()) < SENDING_GRACE_SECONDS:
        return   # its handoff may still be talking to T3
    if now() - record.get("sent_at", now()) > EXPIRE_SECONDS:
        finish(path, record, "expired", "", speaker, now)
        return
    try:
        outcome, record, text = advance(mcp, record, now=now, wait_ms=wait_ms)
    except McpError as error:
        if not error.refused():
            raise
        record["failures"] = record.get("failures", 0) + 1
        record["error"] = str(error)
        log("take=" + record["take"] + " refused code=" + str(error) + " failures=" + str(record["failures"]))
        if record["failures"] >= TOOL_FAILURES:
            finish(path, record, "error", "", speaker, now)
        else:
            save(path, record)
        return
    record.pop("failures", None)
    if outcome == "wait":
        save(path, record)
    else:
        finish(path, record, outcome, text, speaker, now)


def finish(path, record, outcome, text, speaker, now):
    """Move the record to done *before* speaking: a reply is spoken at most once."""
    label = config_label(speaker)
    stale = now() - record.get("finished_at", now()) > STALE_SECONDS
    record["outcome"] = outcome
    record["reply_chars"] = len(text)
    spoken, notice = "", ""
    if outcome == "failed":
        spoken = label + " could not finish that request."
    elif outcome == "reply" and text.strip():
        spoken = text
    elif outcome == "reply":
        record["outcome"] = "empty"
    else:   # expired or refused: nothing to say, but the user should know
        notice = label + "'s reply could not be collected; see the thread."
    verb = "could not finish a request" if outcome == "failed" else "replied"
    if not spoken:
        record["speech"] = "none"
    elif is_muted():
        record["speech"] = "muted"
    elif stale:
        record["speech"] = "stale"
    else:
        record["speech"] = "spoken"
    if record["speech"] in ("muted", "stale"):
        notice = label + " " + verb + " (" + record["speech"] + "); see the thread."
    write_json(state_dir() / "done" / path.name, record)
    path.unlink(missing_ok=True)
    log("reply take=" + record["take"] + " outcome=" + record["outcome"] + " chars="
        + str(len(text)) + " speech=" + record["speech"])
    if record["speech"] == "spoken":
        result = speaker.say(spoken)
        if result != "spoken":
            record["speech"] = result
            write_json(state_dir() / "done" / path.name, record)
            if result in ("muted", "held"):
                notice = label + " " + verb + " (" + result + "); see the thread."
            elif result == "unplayable":
                notice = label + "'s reply could not be played; see the thread."
    if notice:
        notify(notice)


def config_label(speaker):
    return speaker.config.get("label", "Home")


# ── speech ────────────────────────────────────────────────────────────────


def speakable(markdown):
    """Turn a Markdown reply into plain sentences for speech."""
    text = re.sub(r"```.*?```", " (code omitted) ", markdown, flags=re.S)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<?(https?|t3-thread)://\S+>?", "a link", text)
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"^\s*\|?\s*:?-{3,}.*$", " ", text, flags=re.M)
    text = text.replace("|", ", ")
    text = re.sub(r"^\s{0,3}(#{1,6}|>|[-*+]|\d+[.)])\s+", "", text, flags=re.M)
    text = re.sub(r"(?<![\w*~])(\*\*|__|\*|_|~~)(?=\S)(.+?)(?<=\S)\1(?![\w*~])", r"\2", text)
    lines = [line.strip() for line in text.splitlines()]
    text = " ".join(line if re.search(r"[.!?:;,]$", line) else line + "." for line in lines if line)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > SPOKEN_LIMIT:
        cut = text.rfind(". ", 0, SPOKEN_LIMIT)
        text = text[: cut + 1 if cut > 0 else SPOKEN_LIMIT] + " The rest is in the thread."
    return text


def chunks(text, limit=CHUNK_LIMIT, first=FIRST_CHUNK_LIMIT):
    """Split at sentence ends; the first chunk is short so speech starts quickly."""
    pieces = []
    for sentence in re.split(r"(?<=[.!?])\s+", text):
        while len(sentence) > limit:
            pieces.append(sentence[:limit])
            sentence = sentence[limit:]
        if sentence:
            pieces.append(sentence)
    current, cap = "", min(first, limit)
    for piece in pieces:
        if current and len(current) + 1 + len(piece) > cap:
            yield current
            current, cap = piece, limit
        else:
            current = (current + " " + piece).strip()
    if current:
        yield current


USER_SPEAKING_SECONDS = 30   # a hands-free marker older than this is stale


def user_speaking():
    """Hands-free capture marks speech in progress; replies wait for the user to finish."""
    marker = runtime_dir() / "user-speaking"
    try:
        return time.time() - marker.stat().st_mtime < USER_SPEAKING_SECONDS
    except OSError:
        return False


def cantrip_recording():
    try:
        status = subprocess.run(["cantrip", "status", "--json"], capture_output=True, text=True, timeout=5)
        return json.loads(status.stdout).get("state") == "recording"
    except (OSError, ValueError, subprocess.SubprocessError):
        return False


def is_muted():
    return (state_dir() / "muted").exists()


def notify(message):
    if shutil.which("notify-send"):
        subprocess.run(["notify-send", "-a", "Cantrip", "-t", "2500", message],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


class Speaker:
    """Speak text once: natural remote voice when configured, else espeak-ng."""

    def __init__(self, config, opener=None, recording=cantrip_recording):
        self.config = config
        self.open = opener or OPEN
        self.recording = recording

    def say(self, text):
        """Speak once, one voice at a time: "spoken", "muted", "stopped" or "unplayable"."""
        text = speakable(text)
        if not text:
            return "spoken"
        with open(runtime_dir() / "speech.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            started = time.time()
            self.checked = (0.0, False)
            while self.recording() or user_speaking():
                if self.silenced(started):
                    return self.silenced(started)
                if time.time() - started > 600:
                    return "held"   # still recording after ten minutes: leave it to the thread
                time.sleep(0.5)
            if self.silenced(started):
                return self.silenced(started)
            parts = list(chunks(text))
            played = 0
            key = secret("CANTRIP_T3_SPEECH_KEY")
            if key:
                try:
                    played = self.remote(parts, key, started)
                except OSError as error:
                    log("speech player unavailable class=" + type(error).__name__)
            if played < len(parts) and not self.interrupted(started):
                log("speech fallback=local chunks=" + str(len(parts) - played))
                try:
                    self.local(" ".join(parts[played:]), started)
                except OSError as error:
                    log("local speech unavailable class=" + type(error).__name__)
                    return "spoken" if played else "unplayable"
            return self.silenced(started) or "spoken"   # cut short by stop or mute: say so

    def silenced(self, started):
        return "muted" if is_muted() else "stopped" if self.stopped(started) else None

    def stopped(self, started):
        marker = runtime_dir() / "stop"
        return marker.exists() and marker.stat().st_mtime >= started - 0.01

    def interrupted(self, started):
        """Stopped or muted, or a Cantrip take began recording (checked once a second)."""
        if self.stopped(started) or is_muted():
            return True
        checked, recording = getattr(self, "checked", (0.0, False))
        if time.time() - checked >= 1:
            recording = self.recording()
            self.checked = (time.time(), recording)
        return recording

    def player(self, argv):
        (runtime_dir() / "playing").unlink(missing_ok=True)   # left by a crashed collector: nothing audible yet
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, start_new_session=True)
        (runtime_dir() / "player.pid").write_text(str(process.pid))
        return process

    def release(self, process, started):
        if self.interrupted(started) and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.stdin.close()
        except BrokenPipeError:
            pass
        process.wait()
        (runtime_dir() / "player.pid").unlink(missing_ok=True)
        (runtime_dir() / "playing").unlink(missing_ok=True)

    def fetch(self, parts, key, audio, cancelled):
        """Producer thread: request each chunk in turn and queue its PCM blocks."""
        speech = self.config["speech"]
        for index, part in enumerate(parts):
            body = json.dumps({"model": speech["model"], "voice": speech["voice"],
                               "input": part, "response_format": "pcm"}).encode()
            request = urllib.request.Request(speech["endpoint"].rstrip("/") + "/audio/speech", data=body,
                                             headers={"Authorization": "Bearer " + key,
                                                      "Content-Type": "application/json"})
            try:
                with self.open(request, timeout=SPEECH_STALL_SECONDS) as response:
                    while block := response.read(8192):
                        if cancelled.is_set():
                            return
                        audio.put((index, block))
            except Exception as error:  # any failure ends remote speech; the player loop falls back
                audio.put((index, error))
                return
            audio.put((index, None))

    def remote(self, parts, key, started):
        """Play natural speech; return how many chunks were fully handed to the player."""
        audio = queue.Queue()
        cancelled = threading.Event()
        process = self.player(["pw-cat", "--playback", "--raw", "--format", "s16",
                               "--rate", str(PCM_RATE), "--channels", "1", "-"])   # before paying for speech
        threading.Thread(target=self.fetch, args=(parts, key, audio, cancelled), daemon=True).start()
        played = 0
        last = time.time()
        try:
            while played < len(parts) and not self.interrupted(started):
                try:
                    index, block = audio.get(timeout=0.1)
                except queue.Empty:
                    if time.time() - last > SPEECH_STALL_SECONDS:
                        log("speech stalled chunk=" + str(played))
                        break
                    continue
                last = time.time()
                if isinstance(block, Exception):
                    log("speech error class=" + type(block).__name__ + " chunk=" + str(index))
                    break
                if block is None:
                    played = index + 1
                else:
                    while user_speaking() and not self.interrupted(started):   # never talk over hands-free speech
                        time.sleep(0.1)
                    if self.interrupted(started):
                        break
                    (runtime_dir() / "playing").touch()   # audible from now: hands-free stops listening
                    process.stdin.write(block)
                    process.stdin.flush()   # unflushed PCM would sit in the pipe buffer as a gap
        except BrokenPipeError:
            pass
        finally:
            cancelled.set()
            self.release(process, started)
        return played

    def local(self, text, started):
        process = self.player(["espeak-ng", "-v", self.config["speech"]["local_voice"], "-s", "170"])
        (runtime_dir() / "playing").touch()
        try:
            process.stdin.write(text.encode())
            process.stdin.close()
        except BrokenPipeError:
            pass
        while process.poll() is None and not self.interrupted(started):
            time.sleep(0.1)
        self.release(process, started)


# ── keyboard actions ──────────────────────────────────────────────────────


def repeat(config, mcp=None, speaker=None):
    """Speak the most recent reply again, fetched from T3; no reply text is stored locally."""
    done = [readable(p) for p in (state_dir() / "done").glob("*.json")]
    done = [record for record in done if record and record.get("outcome") == "reply"]
    if not done:
        return False
    record = max(done, key=lambda item: item.get("finished_at") or 0)
    text = final_reply(mcp or connect(config), record)
    log("repeat take=" + record["take"] + " chars=" + str(len(text)))
    if text and not is_muted():
        (speaker or Speaker(config)).say(text)
    return bool(text)


PLAYERS = ("pw-cat", "espeak-ng")


def stop_speaking():
    """Mark speech stopped and end the current player, if the pid file still names one."""
    (runtime_dir() / "stop").write_text(str(time.time()))
    pid_file = runtime_dir() / "player.pid"
    try:
        pid = int(pid_file.read_text())
        argv0 = Path("/proc", str(pid), "cmdline").read_bytes().split(b"\0")[0].decode()
    except (OSError, ValueError):
        pid_file.unlink(missing_ok=True)
        return False
    if Path(argv0).name not in PLAYERS:
        pid_file.unlink(missing_ok=True)   # stale: the pid now belongs to something else
        return False
    try:
        os.killpg(pid, signal.SIGTERM)
        return True
    except OSError:
        return False


def set_mute(action):
    marker = state_dir() / "muted"
    muted = marker.exists()
    target = {"on": True, "off": False, "toggle": not muted}[action]
    if target:
        marker.write_text("muted\n")
        stop_speaking()
    else:
        marker.unlink(missing_ok=True)
    return target


# ── voice status for the desktop bar ──────────────────────────────────────

SESSION_MODES = {"live": "cantrip_t3_live", "hands-free": "cantrip_t3_converse"}
SESSION_PHASES = ("listening", "hearing", "thinking", "speaking")


def session_state(mode):
    """A running session's state file, trusted only while its process lives.

    Sessions write $XDG_RUNTIME_DIR/cantrip-t3/session-<mode>.json:
    {"mode", "pid", "phase": listening|hearing|thinking|speaking, "muted": bool}.
    """
    path = runtime_dir() / ("session-" + mode + ".json")
    try:
        state = json.loads(path.read_text())
        argv = Path("/proc", str(int(state["pid"])), "cmdline").read_bytes().decode(errors="replace")
    except (OSError, ValueError, KeyError, TypeError):
        return None
    names = {Path(arg).name.replace("-", "_").removesuffix(".py") for arg in argv.split("\0") if arg}
    if SESSION_MODES[mode] not in names or state.get("phase") not in SESSION_PHASES:
        return None
    return state


def player_live():
    try:
        pid = int((runtime_dir() / "player.pid").read_text())
        argv0 = Path("/proc", str(pid), "cmdline").read_bytes().split(b"\0")[0].decode()
    except (OSError, ValueError):
        return False
    return Path(argv0).name in PLAYERS


def cantrip_snapshot():
    try:
        result = subprocess.run(["cantrip", "status", "--json"], capture_output=True, text=True, timeout=3)
        return json.loads(result.stdout) if result.returncode == 0 else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


PHASE_WORDS = {"listening": "listening", "hearing": "hearing you", "thinking": "{label} thinking",
               "speaking": "{label} speaking", "recording": "recording", "transcribing": "transcribing"}
PTT_WORDS = {"recording": "recording", "transcribing": "transcribing", "thinking": "thinking", "speaking": "speaking"}
GLYPHS = {"listening": "\U000f036c", "hearing": "\U000f036c", "recording": "\U000f036c",   # nf-md-microphone
          "transcribing": "\U000f0150", "thinking": "\U000f0150",                            # nf-md-clock_outline
          "speaking": "\U000f057e",                                                           # nf-md-volume_high
          "mic-muted": "\U000f036d", "replies-muted": "\U000f0581"}                          # microphone_off, volume_off
OWED_SECONDS = 10 * 60   # a reply owed longer than this is no longer shown as "thinking"


def voice_status(config, snapshot=None, now=time.time):
    """One transcript-free line for the bar: which voice mode is active and what it is doing."""
    label = config.get("label", "Home")
    pending = [readable(p) for p in (state_dir() / "pending").glob("*.json")]
    pending = [record for record in pending if record and now() - (record.get("sent_at") or 0) < OWED_SECONDS]
    speaking = player_live()
    replies_muted = is_muted()
    mic_muted = False
    mode = phase = None
    live = session_state("live")
    hands_free = session_state("hands-free")
    if live:
        mode, phase = "live", live["phase"]
        mic_muted = bool(live.get("muted"))
        replies_muted = False   # live speech is GPT-Live's own; reply muting doesn't apply
    elif hands_free:
        mode = "hands-free"
        phase = hands_free["phase"]
        if phase == "listening":
            phase = "speaking" if speaking else "thinking" if any(
                r["take"].startswith("converse-") for r in pending) else "listening"
    else:
        snapshot = snapshot if snapshot is not None else cantrip_snapshot()
        snapshot = snapshot if isinstance(snapshot, dict) else {}
        handoff = snapshot.get("handoff") if isinstance(snapshot.get("handoff"), dict) else {}
        if handoff.get("name") == config.get("handoff", "home") and snapshot.get("state") in ("recording", "processing"):
            mode, phase = "push-to-talk", "recording" if snapshot["state"] == "recording" else "transcribing"
        elif speaking:
            mode, phase = "push-to-talk", "speaking"
        elif pending:
            mode, phase = "push-to-talk", "thinking"
    names = {"live": "Live", "hands-free": "Hands-free", "push-to-talk": label}
    muted_segments = ([{"glyph": GLYPHS["mic-muted"], "text": "mic muted", "tone": "dim"}] if mic_muted else []) + (
        [{"glyph": GLYPHS["replies-muted"], "text": "replies muted", "tone": "dim"}] if replies_muted else [])
    if not mode:
        return {"visible": bool(muted_segments), "mode": None, "phase": None, "mic_muted": False,
                "replies_muted": replies_muted, "segments": muted_segments,
                "tooltip": label + "'s spoken replies are muted." if replies_muted else ""}
    words = PTT_WORDS[phase] if mode == "push-to-talk" else PHASE_WORDS[phase].format(label=label)
    segments = [{"glyph": GLYPHS[phase], "text": names[mode] + " · " + words,
                 "tone": "active" if phase in ("listening", "hearing", "recording") else "normal"}] + muted_segments
    about = {"live": "Live conversation with " + label, "hands-free": "Hands-free conversation with " + label,
             "push-to-talk": "Push-to-talk to " + label}[mode]
    clicks = {"live": "Left click ends the session; middle click mutes the microphone.",
              "hands-free": "Left click ends hands-free; middle click stops speech.",
              "push-to-talk": "Middle click stops speech."}[mode]
    flags = "".join(", " + segment["text"] for segment in muted_segments)
    return {"visible": True, "mode": mode, "phase": phase, "mic_muted": mic_muted, "replies_muted": replies_muted,
            "segments": segments, "tooltip": about + ": " + words + flags + ". " + clicks}


def main(argv=None):
    raw = sys.argv[1:] if argv is None else argv
    if raw == ["state"]:   # the bar polls this; skip argparse's import cost
        print(json.dumps(voice_status(load_config()), ensure_ascii=False))
        return 0
    import argparse
    parser = argparse.ArgumentParser(prog="cantrip-t3", description=__doc__.split("\n\n")[0])
    parser.add_argument("--config", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("send", help="Cantrip handoff: send stdin to the configured thread")
    collector = commands.add_parser("collect", help="wait for replies and speak them")
    collector.add_argument("--once", action="store_true")
    mute = commands.add_parser("mute", help="mute, unmute or toggle reply speech")
    mute.add_argument("action", nargs="?", choices=("on", "off", "toggle"), default="toggle")
    commands.add_parser("stop", help="stop speaking the current reply")
    commands.add_parser("repeat", help="speak the most recent reply again")
    commands.add_parser("talk", help="stop speech, then toggle a Cantrip take to this handoff")
    commands.add_parser("status", help="print mute state and pending take count")
    commands.add_parser("state", help="one JSON line for the desktop bar: active voice mode and phase")
    args = parser.parse_args(argv)

    if args.command == "stop":
        stop_speaking()
        return 0
    if args.command == "mute":
        muted = set_mute(args.action)
        notify("Spoken replies " + ("muted" if muted else "on"))
        print("muted" if muted else "unmuted")
        return 0
    if args.command == "status":
        pending = len(list((state_dir() / "pending").glob("*.json")))
        last = state_dir() / "last-error.json"
        error = json.loads(last.read_text()) if last.exists() else None
        print(json.dumps({"muted": is_muted(), "pending": pending, "last_error": error}))
        return 0
    config = load_config(args.config)
    if args.command == "state":
        print(json.dumps(voice_status(config), ensure_ascii=False))
        return 0
    if args.command == "talk":
        stop_speaking()
        os.execvp("cantrip", ["cantrip", "toggle", "--handoff", config["handoff"]])
    if args.command == "repeat":
        try:
            return 0 if repeat(config) else 1
        except (McpError, OSError, ValueError) as error:
            log("repeat failed class=" + type(error).__name__ + " code=" + describe(error))
            return 1
    if args.command == "send":
        try:
            send(config, sys.stdin.read(), os.environ.get("CANTRIP_TAKE_ID", ""))
        except (McpError, OSError, ValueError) as error:
            log("send failed class=" + type(error).__name__ + " code=" + describe(error))
            return 1
        return 0
    collect(config, once=args.once)
    return 0


if __name__ == "__main__":
    sys.exit(main())
