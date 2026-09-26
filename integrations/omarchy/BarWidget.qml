import QtQuick
import Quickshell.Io
import qs.Ui
import qs.Commons
import "Status.js" as Status

// Omarchy 4.0.0.alpha: Ui/BarWidget.qml, Ui/BarIconButton.qml,
// Ui/WidgetButton.qml and services/PluginRegistry.qml define these APIs.
// One fixed slot holding the Cantrip mark: dim at rest, the take's route color
// while recording or processing, the theme's urgent color when the latest take
// needs attention. No count and no text; the tooltip carries the words.
BarWidget {
    id: root
    moduleName: "cantrip.dictation"

    property var snapshot: null
    readonly property string tone: Status.tone(snapshot)
    readonly property var handoff: Status.handoff(snapshot)
    readonly property color routeColor: handoff ? handoff.color : Color.accent
    readonly property color restColor: bar ? bar.barForeground : Color.bar.text
    readonly property bool animate: !bar || bar.foregroundAnimationEnabled

    implicitWidth: Style.bar.iconSlot
    implicitHeight: Style.bar.iconSlot

    function refresh() {
        if (!statusProcess.running) statusProcess.running = true
    }

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
        onTriggered: root.refresh()
    }

    // Processing: three lit cells travel around the C.
    property int chase: 0
    Timer {
        interval: 140
        running: root.tone === "processing" && root.animate
        repeat: true
        onTriggered: root.chase = (root.chase + 1) % 8
    }

    Component {
        id: cantripMark
        Item {
            id: mark
            // The solid pixel C of Cantrip's mark (site/public/favicon.svg), on a 4×4 cell grid.
            readonly property real cell: width / 4
            readonly property var cells: [[3, 0], [2, 0], [1, 0], [0, 1], [0, 2], [1, 3], [2, 3], [3, 3]]
            Repeater {
                model: mark.cells
                Rectangle {
                    required property var modelData
                    required property int index
                    x: modelData[0] * mark.cell
                    y: modelData[1] * mark.cell
                    width: mark.cell
                    height: mark.cell
                    antialiasing: false
                    color: root.tone === "attention" ? Color.urgent
                        : root.tone === "recording" || root.tone === "processing" ? root.routeColor
                        : root.restColor
                    opacity: root.tone === "unavailable" ? 0.3
                        : root.tone === "rest" ? 0.55
                        : root.tone === "processing" && (index - root.chase + 8) % 8 > 2 ? 0.35
                        : 1
                    Behavior on color {
                        enabled: root.animate
                        ColorAnimation { duration: 160 }
                    }
                }
            }
        }
    }

    BarIconButton {
        id: button
        anchors.fill: parent
        bar: root.bar
        iconComponent: cantripMark
        tooltipText: Status.tooltip(root.snapshot)
        // The host caches tooltip text on entry; refresh it while hovered too.
        onTooltipTextChanged: if (bar && tooltipHovered) bar.showTooltip(button, tooltipText)
        onPressed: function(mouseButton) {
            if (!root.bar) return
            if (mouseButton === Qt.LeftButton) {
                root.bar.run("cantrip toggle --postproc raw")
            } else if (mouseButton === Qt.MiddleButton) {
                // Dismissal acknowledges the outcome; it never deletes a recording.
                if (root.tone === "attention") root.bar.run("cantrip dismiss")
                root.refresh()
            } else if (mouseButton === Qt.RightButton) {
                root.bar.run("cantrip actions")
            }
        }
    }

    Accessible.role: Accessible.Button
    Accessible.name: "Cantrip"
    Accessible.description: button.tooltipText
}
