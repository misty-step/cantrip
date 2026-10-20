import QtQuick
import Quickshell
import Quickshell.Io
import qs.Ui
import qs.Commons

// Shows when a voice conversation with the agent thread is active, and what it is doing:
// "Live · listening", "Hands-free · Home thinking", "Home · recording". Plain text with one
// glyph, in the bar's own colors; nothing at rest except a dim marker while replies are muted.
// `cantrip-t3 state` decides what it says (transcript-free JSON, one line).
BarWidget {
    id: root
    moduleName: "cantrip.home-voice"

    property var state: null
    readonly property var segments: state && state.visible && Array.isArray(state.segments) ? state.segments : []
    readonly property color textColor: bar ? bar.barForeground : Color.bar.text

    visible: segments.length > 0
    implicitWidth: visible ? (vertical ? barSize : row.implicitWidth + Style.space(12)) : 0
    implicitHeight: visible ? (vertical ? row.implicitHeight + Style.space(8) : barSize) : 0

    function toneColor(tone) { return tone === "active" ? Color.bar.active : root.textColor }
    function refresh() { if (!stateProc.running) stateProc.running = true }

    Process {
        id: stateProc
        command: ["cantrip-t3", "state"]
        running: true
        stdout: StdioCollector { id: stateOut; waitForEnd: true }
        onExited: function(exitCode, exitStatus) {
            var parsed = null
            if (exitCode === 0) {
                try { parsed = JSON.parse(stateOut.text) } catch (_) { parsed = null }
            }
            root.state = parsed
            if (parsed) expiry.restart()
        }
    }

    // A state older than this is not trusted: hide rather than claim a session that ended.
    Timer { id: expiry; interval: 5000; onTriggered: root.state = null }
    Timer { interval: 1000; running: true; repeat: true; onTriggered: root.refresh() }

    Row {
        id: row
        anchors.centerIn: parent
        spacing: Style.space(6)
        Repeater {
            model: root.segments
            delegate: Row {
                required property var modelData
                spacing: Style.space(3)
                opacity: modelData.tone === "dim" ? 0.55 : 1
                Text {
                    text: modelData.glyph
                    textFormat: Text.PlainText
                    color: root.toneColor(modelData.tone)
                    font.family: root.bar ? root.bar.fontFamily : Style.font.family
                    font.pixelSize: Style.bar.iconFont
                    anchors.verticalCenter: parent.verticalCenter
                }
                Text {
                    visible: !root.vertical && modelData.text !== ""
                    text: modelData.text
                    textFormat: Text.PlainText
                    color: root.toneColor(modelData.tone)
                    font.family: root.bar ? root.bar.fontFamily : Style.font.family
                    font.pixelSize: Style.font.caption
                    anchors.verticalCenter: parent.verticalCenter
                }
            }
        }
    }

    MouseArea {
        anchors.fill: parent
        hoverEnabled: true
        acceptedButtons: Qt.LeftButton | Qt.MiddleButton
        cursorShape: Qt.PointingHandCursor
        readonly property string tip: root.state && root.state.tooltip ? root.state.tooltip : ""
        onTipChanged: if (containsMouse && root.bar) root.bar.showTooltip(root, tip)
        onEntered: if (root.bar && tip) root.bar.showTooltip(root, tip)
        onExited: if (root.bar) root.bar.hideTooltip(root)
        onClicked: function(event) {
            if (!root.state) return
            if (event.button === Qt.MiddleButton) {
                Quickshell.execDetached(["cantrip-t3", "stop"])
            } else if (root.state.mode === "live") {
                Quickshell.execDetached(["systemctl", "--user", "stop", "cantrip-t3-live.service"])
            } else if (root.state.mode === "hands-free") {
                Quickshell.execDetached(["systemctl", "--user", "stop", "cantrip-t3-converse.service"])
            }
            root.refresh()
        }
    }

    Accessible.role: Accessible.Button
    Accessible.name: "Home voice"
    Accessible.description: root.state && root.state.tooltip ? root.state.tooltip : ""
}
