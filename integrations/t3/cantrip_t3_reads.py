"""Read-only snapshot of the agent's own status, for fast spoken answers.

A refresher rebuilds the snapshot every `refresh_seconds` from local commands:
the agent's NOW.md, its latest inbox entries, and the newest lage brief. Read
tools and the session digest are served from that cache, so answering never
waits on the network and nothing here can write, steer or dispatch.

Content stays in memory and goes only to the live session it serves; logs carry
sizes, counts and error codes, never text.
"""

from pathlib import Path
import re
import subprocess
import sys
import threading
import time

READS_DEFAULTS = {
    "now_command": [],                       # prints NOW.md
    "inbox_command": ["home", "inbox"],      # prints unread inbox entries, without marking them read
    "lage_dir": "~/.local/state/home/lage",  # newest *.md is the current brief
    "recall_command": ["home", "recall", "--limit", "8"],   # + one regex argument
    "refresh_seconds": 60,
    "digest_chars": 9000,                    # about 2,250 tokens
}
COMMAND_SECONDS = 20
INBOX_KEEP = 30
TOOL_CHARS = 1600                            # each tool result stays well inside a 500-token append


def log(message):
    print("[T3Reads] " + message, file=sys.stderr, flush=True)


def run(command, *args):
    if not command:
        return ""
    try:
        result = subprocess.run([*command, *args], capture_output=True, text=True, timeout=COMMAND_SECONDS)
    except (OSError, subprocess.SubprocessError) as error:
        log("command failed class=" + type(error).__name__)
        return ""
    if result.returncode != 0:
        log("command failed exit=" + str(result.returncode))
        return ""
    return result.stdout


ENTRY = re.compile(r"^## (\S+) \[([^\]]+)\][^\n]*\n(.*?)(?=^<!-- entry|\Z)", re.M | re.S)


def parse_inbox(text):
    """Inbox entries, newest last: {"at", "lane", "headline", "body"}."""
    entries = []
    for at, lane, body in ENTRY.findall(text):
        body = body.strip()
        headline = next((line.strip("* ").strip() for line in body.splitlines() if line.strip()), "")
        entries.append({"at": at, "lane": lane, "headline": headline, "body": body})
    return entries[-INBOX_KEEP:]


LANE_BULLET = re.compile(r"^- \*\*([^*:]+):\*\*\s*(.+)$", re.M)


def parse_lage(text):
    """The brief's opening paragraphs and its per-lane bullets ("- **Tach:** ...")."""
    paragraphs = [p.strip() for p in text.split("\n\n") if p.strip()]
    overview = re.split(r"^\*\*What needs you\*\*", text, maxsplit=1, flags=re.M)[0]
    lanes = {}
    for name, line in LANE_BULLET.findall(overview):
        lanes.setdefault(name.strip(), []).append(line.strip())
    return {"headline": "\n\n".join(paragraphs[:2]), "lanes": lanes, "text": text}


def newest_brief(directory):
    briefs = sorted(Path(directory).expanduser().glob("*.md"))
    return (briefs[-1].name, briefs[-1].read_text(errors="replace")) if briefs else ("", "")


def clip(text, limit=TOOL_CHARS):
    text = text.strip()
    return text if len(text) <= limit else text[:limit].rsplit(" ", 1)[0] + " …"


class Snapshot:
    """Thread-safe cache of the agent's status, refreshed in the background."""

    def __init__(self, settings, clock=time.time):
        self.settings = {**READS_DEFAULTS, **settings}
        self.clock = clock
        self.lock = threading.Lock()
        self.data = {"now": "", "inbox": [], "lage": parse_lage(""), "brief": "", "built_at": 0.0}
        self.version = 0
        self.stopping = threading.Event()

    def refresh(self):
        now = run(self.settings["now_command"])
        inbox = parse_inbox(run(self.settings["inbox_command"]))
        brief, lage = newest_brief(self.settings["lage_dir"])
        data = {"now": now, "inbox": inbox, "lage": parse_lage(lage), "brief": brief, "built_at": self.clock()}
        with self.lock:
            changed = (now, [e["at"] + e["headline"] for e in inbox], brief) != (
                self.data["now"], [e["at"] + e["headline"] for e in self.data["inbox"]], self.data["brief"])
            self.data = data
            if changed:
                self.version += 1
        log("refreshed now_chars=%d inbox=%d brief=%s changed=%s" % (len(now), len(inbox), bool(brief), changed))
        return changed

    def start(self):
        def loop():
            while not self.stopping.is_set():
                try:
                    self.refresh()
                except Exception as error:  # a bad source must never end the session
                    log("refresh failed class=" + type(error).__name__)
                self.stopping.wait(self.settings["refresh_seconds"])

        threading.Thread(target=loop, daemon=True).start()

    def get(self):
        with self.lock:
            return self.data

    # ── what the voice model sees ────────────────────────────────────────

    def digest(self):
        """A compact view for the live session's context: the brief's opening, lanes, recent inbox."""
        data = self.get()
        age = int((self.clock() - data["built_at"]) / 60) if data["built_at"] else None
        parts = ["Status snapshot (read-only, about %s min old). Answer status questions from this; "
                 "use the read tools for detail." % (age if age is not None else "?")]
        if data["lage"]["headline"]:
            parts.append("Latest brief:\n" + clip(data["lage"]["headline"], 2500))
        if data["lage"]["lanes"]:
            parts.append("Lanes:\n" + "\n".join("- %s: %s" % (name, clip(" ".join(lines), 300))
                                                for name, lines in data["lage"]["lanes"].items()))
        if data["inbox"]:
            parts.append("Recent reports:\n" + "\n".join("- %s [%s] %s" % (e["at"], e["lane"], clip(e["headline"], 200))
                                                         for e in data["inbox"][-10:]))
        if data["now"]:
            parts.append("NOW.md (opening):\n" + clip(data["now"], 2500))
        return clip("\n\n".join(parts), self.settings["digest_chars"])

    # ── read tools ───────────────────────────────────────────────────────

    def now(self, section=""):
        text = self.get()["now"]
        if not text:
            return "NOW.md is not available right now."
        if section:
            match = re.search(r"^(#+ [^\n]*%s[^\n]*\n.*?)(?=^#+ |\Z)" % re.escape(section), text, re.M | re.S | re.I)
            if match:
                return clip(match.group(1))
            return "No section of NOW.md matches %r. Sections: %s" % (
                section, ", ".join(re.findall(r"^#+ (.+)$", text, re.M))[:600])
        return clip(text)

    def inbox(self, count=5, lane=""):
        entries = self.get()["inbox"]
        if lane:
            entries = [e for e in entries if lane.lower() in e["lane"].lower()]
        if not entries:
            return "No recent reports" + (" from " + lane if lane else "") + "."
        count = max(1, min(int(count or 5), 10))
        return clip("\n\n".join("%s [%s]\n%s" % (e["at"], e["lane"], clip(e["body"], 500)) for e in entries[-count:]))

    def lage(self, section=""):
        lage = self.get()["lage"]
        if not lage["text"]:
            return "No lage brief is available."
        if section:
            paragraphs = [p for p in lage["text"].split("\n\n") if section.lower() in p.lower()]
            return clip("\n\n".join(paragraphs)) if paragraphs else "The latest brief doesn't mention %r." % section
        return clip(lage["headline"])

    def lane(self, name):
        data = self.get()
        lines = [line for lane, found in data["lage"]["lanes"].items() if name.lower() in lane.lower() for line in found]
        reports = [e for e in data["inbox"] if name.lower() in e["lane"].lower()][-3:]
        if not lines and not reports:
            return "Nothing in the snapshot about %r." % name
        out = ["Brief: " + " ".join(lines)] if lines else []
        out += ["Report %s: %s" % (e["at"], clip(e["body"], 400)) for e in reports]
        return clip("\n".join(out))

    def recall(self, query):
        query = (query or "").strip()[:120]
        if not query:
            return "Say what to look for."
        return clip(run(self.settings["recall_command"], query)) or "Nothing found for %r." % query
