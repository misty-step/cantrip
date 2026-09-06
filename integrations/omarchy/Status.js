// Transcript-free mapping of the public status snapshot. No socket protocol
// implementation here: the installed Cantrip CLI owns framing and deadlines.
function parse(exitCode, text, exitStatus) {
    if (exitCode !== 0 || exitStatus !== 0) return null
    try {
        var value = JSON.parse(text)
        if (!value || typeof value.epoch !== "string" || value.epoch.length === 0
                || typeof value.state !== "string"
                || typeof value.pending_recordings !== "number" || !isFinite(value.pending_recordings)
                || value.pending_recordings < 0 || Math.floor(value.pending_recordings) !== value.pending_recordings
                || !value.capabilities || typeof value.capabilities.cancel !== "boolean") return null
        return value
    } catch (_) {
        return null
    }
}

function tooltip(snapshot, lastPending) {
    if (!snapshot) {
        return "Cantrip connection unavailable. Recording and delivery status unknown."
            + (lastPending > 0 ? " Last confirmed pending recordings: " + lastPending + "." : "")
            + " Right-click for recovery and setup."
    }
    var state
    if (snapshot.state === "recording") {
        state = snapshot.signal ? "Recording" : "Starting microphone"
    } else if (snapshot.state === "processing") {
        state = "Working"
    } else if (snapshot.state === "idle") {
        state = "Ready"
    } else {
        state = "State unknown"
    }
    var detail = snapshot.outcome && !snapshot.outcome.dismissed
        && typeof snapshot.outcome.message === "string" ? " " + snapshot.outcome.message : ""
    if (snapshot.notice && typeof snapshot.notice.message === "string") {
        detail += " " + snapshot.notice.message
    }
    var pending = snapshot.pending_recordings > 0 ? " " + snapshot.pending_recordings + " recording(s) waiting for recovery." : ""
    return "Cantrip: " + state + "." + detail + pending
        + " Left-click: raw dictation. Right-click: recordings, cancel and setup."
}

if (typeof module !== "undefined") module.exports = { parse: parse, tooltip: tooltip }
