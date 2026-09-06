import QtQuick
import Quickshell.Io
import qs.Ui
import qs.Commons
import "Status.js" as Status

// Omarchy 4.0.0.alpha: Ui/BarWidget.qml, Ui/BarIconButton.qml,
// Ui/WidgetButton.qml and services/PluginRegistry.qml define these APIs.
BarWidget {
    id: root
    moduleName: "cantrip.dictation"

    property var snapshot: null
    property int lastPending: 0
    readonly property string currentState: snapshot ? snapshot.state : "unknown"
    readonly property bool working: currentState === "recording" || currentState === "processing"
    readonly property string tooltip: Status.tooltip(snapshot, lastPending)

    implicitWidth: Style.bar.iconSlot
    implicitHeight: Style.bar.iconSlot

    Process {
        id: statusProcess
        command: ["cantrip", "status", "--json"]
        running: true
        stdout: StdioCollector {
            id: statusOutput
            waitForEnd: true
        }
        onExited: function(exitCode, exitStatus) {
            root.snapshot = Status.parse(exitCode, statusOutput.text, exitStatus)
            if (root.snapshot) statusExpiry.restart()
            if (root.snapshot) root.lastPending = root.snapshot.pending_recordings
        }
    }

    // Failed process launches need not emit exited. Expire the last snapshot
    // after the CLI's ten-second request deadline rather than claiming Ready.
    Timer {
        id: statusExpiry
        interval: 12000
        onTriggered: root.snapshot = null
    }

    Timer {
        interval: 1000
        running: true
        repeat: true
        onTriggered: if (!statusProcess.running) statusProcess.running = true
    }

    BarIconButton {
        id: button
        anchors.fill: parent
        bar: root.bar
        text: "󰍬"
        active: root.working
        useActiveColor: true
        activeColor: Color.accent
        tooltipText: root.tooltip
        // The host caches tooltip text on entry; refresh it while hovered too.
        onTooltipTextChanged: if (bar && tooltipHovered) bar.showTooltip(button, tooltipText)
        onPressed: function(mouseButton) {
            if (!root.bar) return
            if (mouseButton === Qt.LeftButton) {
                root.bar.run("cantrip toggle --postproc raw")
            } else if (mouseButton === Qt.RightButton) {
                root.bar.run("cantrip actions")
            }
        }
    }

    // A labelled pending count is not inferred from the latest success/error.
    // Dismissed exceptions stay discoverable until their own take is resolved.
    Text {
        anchors.right: parent.right
        anchors.top: parent.top
        visible: !root.snapshot || root.lastPending > 0
        text: !root.snapshot ? "?" : String(root.lastPending)
        textFormat: Text.PlainText
        color: Color.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
    }
}
