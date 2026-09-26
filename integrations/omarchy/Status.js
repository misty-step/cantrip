// Transcript-free mapping of the public status snapshot. No socket protocol
// implementation here: the installed Cantrip CLI owns framing and deadlines.
function parse(exitCode, text, exitStatus) {
    if (exitCode !== 0 || exitStatus !== 0) return null
    try {
        var value = JSON.parse(text)
        if (!value || typeof value.epoch !== "string" || value.epoch.length === 0
                || typeof value.state !== "string"
                || !value.capabilities || typeof value.capabilities.cancel !== "boolean") return null
        return value
    } catch (_) {
        return null
    }
}

// A well-formed handoff target, or null for the default flow and malformed data.
function valid(value) {
    if (!value || typeof value.label !== "string" || typeof value.color !== "string"
            || !/^#[0-9a-f]{6}$/i.test(value.color)) return null
    return value
}

// The active take's handoff target; colors the mark only while that take is working.
function handoff(snapshot) {
    return valid(snapshot && snapshot.handoff)
}

// The latest outcome while it is still shown, i.e. not dismissed and not replaced.
function outcome(snapshot) {
    var value = snapshot && snapshot.outcome
    return value && !value.dismissed && typeof value.message === "string" ? value : null
}

// What the mark shows: "unavailable", "rest", "recording", "processing" or "attention".
function tone(snapshot) {
    if (!snapshot) return "unavailable"
    if (snapshot.state === "recording") return "recording"
    if (snapshot.state === "processing") return "processing"
    if (snapshot.state !== "idle") return "unavailable"
    // The daemon owns the rule (TerminalOutcome::needs_attention). It clears when
    // the next take replaces the outcome or the outcome is dismissed.
    return snapshot.attention === true ? "attention" : "rest"
}

function tooltip(snapshot) {
    var mode = tone(snapshot)
    if (mode === "unavailable") {
        return "Cantrip isn't answering. Recording and delivery status unknown. Right-click: setup."
    }
    var target = handoff(snapshot)
    var to = target ? " to " + target.label : ""
    var last = outcome(snapshot)
    var lastTarget = last && valid(last.handoff)
    // "Sent to Kaylee." already names the target; "Handoff failed." does not.
    var named = lastTarget && last.message.indexOf(lastTarget.label) < 0
    var lastText = last ? " " + last.message + (named ? " (to " + lastTarget.label + ")" : "") : ""
    var notice = snapshot.notice && typeof snapshot.notice.message === "string" ? " " + snapshot.notice.message : ""
    if (mode === "recording") {
        return "Cantrip: " + (snapshot.signal ? "Recording" : "Starting microphone") + to + "." + notice
    }
    if (mode === "processing") return "Cantrip: Working" + to + "." + notice
    if (mode === "attention") {
        return "Cantrip:" + lastText + notice
            + " Middle-click: dismiss (keeps the recording). Right-click: recordings and recovery."
    }
    return "Cantrip: Ready." + lastText + notice
        + " Left-click: raw dictation. Right-click: recordings and setup."
}

if (typeof module !== "undefined") {
    module.exports = { parse: parse, handoff: handoff, tone: tone, tooltip: tooltip }
}
