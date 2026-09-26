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
    var outcome = snapshot.outcome && !snapshot.outcome.dismissed
        && typeof snapshot.outcome.message === "string" ? snapshot.outcome : null
    var detail = outcome ? " " + outcome.message : ""
    var last = outcome && valid(outcome.handoff)
    if (last) detail += " (to " + last.label + ")"
    if (snapshot.notice && typeof snapshot.notice.message === "string") {
        detail += " " + snapshot.notice.message
    }
    // Only a take headed to a non-default target is named; the default flow is unchanged.
    var target = handoff(snapshot)
    if (target) state += " to " + target.label
    var pending = snapshot.pending_recordings > 0 ? " " + snapshot.pending_recordings + " recording(s) waiting for recovery." : ""
    return "Cantrip: " + state + "." + detail + pending
        + " Left-click: raw dictation. Right-click: recordings, cancel and setup."
}

// A well-formed handoff target, or null for the default flow and malformed data.
function valid(value) {
    if (!value || typeof value.label !== "string" || typeof value.color !== "string"
            || !/^#[0-9a-f]{6}$/i.test(value.color)) return null
    return value
}

// The active take's handoff target; colors the bar only while that take is working.
function handoff(snapshot) {
    return valid(snapshot && snapshot.handoff)
}

if (typeof module !== "undefined") module.exports = { parse: parse, tooltip: tooltip, handoff: handoff }
