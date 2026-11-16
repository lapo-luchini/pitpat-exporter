// SPDX-License-Identifier: ISC
import QtQuick 2.15
import QtQuick.Layouts
import org.kde.plasma.plasmoid 2.0
import org.kde.plasma.core as PlasmaCore
import org.kde.kirigami as Kirigami

/**
 * Compact panel item (§10): the panel thickness picks the level of detail
 * while a session is open (including paused):
 *   < 1 line → icon + `1.23 km` only
 *   1 line   → `1.23 km · 2.5 km/h`
 *   2 lines  → distance (larger) / speed
 *   3+ lines → distance / speed / session time (mm:ss)
 * Distance first, always metric. Vertical panels: icon only. When
 * stopped: icon only. The tooltip lives in main.qml (toolTip*Text).
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

    readonly property bool vertical:
        Plasmoid.formFactor === PlasmaCore.Types.Vertical

    // how many text lines fit next to the icon at a readable size (G1:
    // derived from the panel thickness via gridUnit, no hard pixels)
    readonly property int lines: {
        if (vertical) {
            return 0;
        }
        const oneLine = Kirigami.Units.gridUnit * 1.6;
        return Math.floor(height / oneLine);
    }

    // H2: a thin panel still shows the distance — the user asked for it
    readonly property bool showDistance: onMove && !vertical && lines >= 0

    Kirigami.Icon {
        id: icon
        source: "run-build"
        anchors {
            left: parent.left
            verticalCenter: parent.verticalCenter
        }
        // fill the panel thickness; capped in very thick panels; on
        // vertical panels the width is fixed, so fit it too (H2 nit)
        height: Math.min(parent.height, parent.width, Kirigami.Units.gridUnit * 3)
        width: height
        opacity: compact.connected ? 1 : 0.35
    }

    Text {
        id: singleLine
        // thin panel: distance only; one-line mode: `1.23 km · 2.5 km/h`
        visible: compact.showDistance && compact.lines <= 1
        text: compact.lines === 0
            ? compact.sessionKm.toFixed(2) + " km"
            : compact.sessionKm.toFixed(2) + " km · "
                + root.snap.speed_kmh.toFixed(1) + " km/h"
        color: Kirigami.Theme.textColor
        font.pixelSize: icon.height / 2.4
        anchors {
            left: icon.right
            verticalCenter: parent.verticalCenter
            leftMargin: Kirigami.Units.smallSpacing
        }
    }

    Column {
        id: textCol
        // multi-line modes
        visible: compact.onMove && compact.lines >= 2
        spacing: 0
        anchors {
            left: icon.right
            verticalCenter: parent.verticalCenter
            leftMargin: Kirigami.Units.smallSpacing
        }

        Text {
            // distance first and larger
            text: compact.sessionKm.toFixed(2) + " km"
            color: Kirigami.Theme.textColor
            font.pixelSize: icon.height / 2.2
            font.bold: true
        }

        Text {
            text: root.snap.speed_kmh.toFixed(1) + " km/h"
            color: Kirigami.Theme.textColor
            font.pixelSize: icon.height / 3.0
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
            font.pixelSize: icon.height / 3.4
        }
    }

    // width follows the widest visible line plus the icon (G1: sized from
    // content, not anchored to the parent, so nothing is clipped)
    Layout.minimumWidth: icon.width + Kirigami.Units.smallSpacing
        + (compact.showDistance ? distanceLine.implicitWidth : 0)
        + (compact.onMove && compact.lines === 1 ? singleLine.implicitWidth : 0)
        + (compact.onMove && compact.lines >= 2 ? textCol.implicitWidth : 0)
    Layout.preferredWidth: Layout.minimumWidth
    Layout.minimumHeight: Kirigami.Units.gridUnit * 2

    // touchpad gestures send many small angleDelta events: accumulate and
    // step once per ±120 (REVIEW round-2 A6)
    property real wheelAccum: 0

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
            wheelAccum += wheel.angleDelta.y;
            if (Math.abs(wheelAccum) < 120) {
                return;
            }
            root.wheelAdjust(wheelAccum > 0);
            wheelAccum = 0;
        }
        // the Plasma tooltip comes from toolTipMainText/SubText in
        // main.qml (G1: no second QQC.ToolTip here)
    }
}
