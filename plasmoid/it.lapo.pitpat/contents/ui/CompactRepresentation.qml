// SPDX-License-Identifier: ISC
import QtQuick 2.15
import QtQuick.Layouts
import QtQuick.Controls as QQC
import org.kde.plasma.plasmoid 2.0
import org.kde.kirigami as Kirigami

/**
 * Compact panel item (REVIEW round-8 F2): the panel thickness picks the
 * level of detail, no config option. While a session is open
 * (including paused):
 *   < 1 line → icon + `1.23 km` (icon only on vertical panels)
 *   1 line   → `1.23 km · 2.5 km/h`
 *   2 lines  → distance (larger) / speed
 *   3+ lines → distance / speed / session time (mm:ss)
 * Distance first, always metric. When stopped: icon only.
 */
Item {
    id: compact

    readonly property bool connected: root.snap.connected
    readonly property bool hasSession: root.snap.session !== null
    readonly property bool onMove: ["starting", "running", "paused"].indexOf(
        root.snap.state) >= 0
    readonly property real sessionKm: hasSession
        ? root.snap.session.distance_m / 1000.0
        : 0.0
    readonly property real sessionMin: hasSession
        ? root.snap.session.duration_s / 60.0
        : 0.0

    // how many text lines fit at a readable size next to the icon
    readonly property int lines: {
        const h = height;
        const oneLine = Kirigami.Units.gridUnit * 1.6;
        if (h < oneLine) {
            return 0;
        }
        return Math.max(0, Math.floor(h / oneLine) - 0);
    }

    // width follows the widest line plus the icon (F2: no clipping)
    Layout.minimumWidth: icon.width
        + (compact.onMove ? textCol.width + Kirigami.Units.smallSpacing : 0)
    Layout.minimumHeight: Kirigami.Units.gridUnit * 2

    Kirigami.Icon {
        id: icon
        source: "run-build"
        anchors {
            left: parent.left
            verticalCenter: parent.verticalCenter
        }
        width: Kirigami.Units.gridUnit
        height: width
        opacity: compact.connected ? 1 : 0.35
    }

    Text {
        // single-line mode
        visible: compact.lines < 2
        text: {
            if (!compact.hasSession) {
                return "";
            }
            if (compact.lines < 1) {
                return compact.sessionKm.toFixed(2) + " km";
            }
            return compact.sessionKm.toFixed(2) + " km · "
                + root.snap.speed_kmh.toFixed(1) + " km/h";
        }
        color: Kirigami.Theme.textColor
        font.pixelSize: Math.min(layout.height / 2.4, Kirigami.Units.gridUnit * 1.1)
        visible: text.length > 0 && compact.onMove
        anchors {
            left: icon.right
            right: parent.right
            verticalCenter: parent.verticalCenter
        }
    }

    Column {
        id: textCol
        // multi-line modes
        visible: compact.lines >= 2
        anchors {
            left: icon.right
            right: parent.right
            verticalCenter: parent.verticalCenter
        }

        Text {
            // distance first and larger
            text: compact.sessionKm.toFixed(2) + " km"
            color: Kirigami.Theme.textColor
            font.pixelSize: layout.height / 2.4
            font.bold: true
        }

        Text {
            visible: compact.lines >= 2
            text: root.snap.speed_kmh.toFixed(1) + " km/h"
            color: Kirigami.Theme.textColor
            font.pixelSize: layout.height / 3.2
        }

        Text {
            visible: compact.lines >= 3
            text: {
                const ses = root.snap.session;
                const s = Math.round(ses.duration_s);
                const h = Math.floor(s / 3600);
                const m = Math.floor((s % 3600) / 60);
                const r = s % 60;
                return h > 0
                    ? h + ":" + String(m).padStart(2, "0") + ":" + String(r).padStart(2, "0")
                    : m + ":" + String(r).padStart(2, "0");
            }
            color: Kirigami.Theme.disabledTextColor
            font.pixelSize: layout.height / 3.6
        }
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        hoverEnabled: true
        acceptedButtons: Qt.LeftButton | Qt.MiddleButton
        onClicked: mouse => {
            if (mouse.button === Qt.MiddleButton) {
                root.toggleStartPause();
            } else if (mouse.button === Qt.LeftButton) {
                root.expanded = !root.expanded;
            }
        }
        onWheel: wheel => {
            // touchpads send many small angleDelta events per gesture:
            // accumulate and step once per ±120 (REVIEW round-2 A6)
            wheelAccum += wheel.angleDelta.y;
            if (Math.abs(wheelAccum) < 120) {
                return;
            }
            root.wheelAdjust(wheelAccum > 0);
            wheelAccum = 0;
        }

        QQC.ToolTip {
            id: tip
            visible: mouse.containsMouse && mouse.buttons === Qt.NoButton
            delay: 350
            timeout: 8000
            text: {
                const head = root.connected
                    ? i18nc("tooltip head", "%1 · %2 km/h",
                            root.snap.state, root.snap.speed_kmh.toFixed(1))
                    : root.snap.state === "sleeping"
                        ? i18n("asleep")
                        : i18n("pitpat daemon not running");
                const ses = root.hasSession
                    ? i18nc("tooltip", "session: %1 km", sessionKm.toFixed(2))
                    : i18nc("tooltip empty", "no open session");
                const today = root.snap.today
                    ? i18nc("tooltip", "today: %1 km",
                            (root.snap.today.distance_m / 1000).toFixed(2))
                    : "";
                return head + "\n" + ses + "\n" + today;
            }
        }
    }
}
