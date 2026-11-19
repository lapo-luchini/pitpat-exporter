// SPDX-License-Identifier: ISC
import QtQuick 2.15
import QtQuick.Layouts
import org.kde.plasma.plasmoid 2.0
import org.kde.plasma.core as PlasmaCore
import org.kde.kirigami as Kirigami

/**
 * Compact panel item (§10): the panel thickness picks how many lines
 * show while a session is open (including paused); the line ORDER is
 * configurable (line1/2/3: distance | time | speed, 2/3 may be none).
 *   < 1 line → icon + `1.23 km` only
 *   1 line   → line1 · line2 joined
 *   2 lines  → line1 (larger) / line2
 *   3+ lines → line1 / line2 / line3
 * `none` lines are skipped without gaps. Distance first by default.
 * Vertical panels: icon only. When stopped: icon only. The tooltip
 * lives in main.qml (toolTip*Text).
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
    readonly property bool showDistance: onMove && !vertical

    // K2: configurable line order; the thickness still picks how many
    // lines show. Values: distance | time | speed | none (lines 2/3);
    // defaults match main.xml (distance/time/speed).
    readonly property var lineOrder: [
        Plasmoid.configuration.line1 || "distance",
        Plasmoid.configuration.line2 || "time",
        Plasmoid.configuration.line3 || "speed",
    ]
    readonly property var lineTexts: {
        const ses = root.snap.session;
        const dist = root.fmtDistance(compact.sessionKm);
        const time = ses ? root.fmtTime(ses.duration_s) : "–";
        const speed = root.fmtSpeed(root.snap.speed_kmh);
        return { distance: dist, time: time, speed: speed };
    }
    // M1: how many fields each mode needs — 0 lines: 1 (distance only),
    // 1 line: 2 joined, 2+ lines: min(lines, 3)
    readonly property int visibleCount: lines === 0 ? 1
        : lines === 1 ? 2
        : Math.min(lines, 3)
    // the visible lines, `none` skipped without leaving gaps
    readonly property var visibleLines: {
        const out = [];
        for (const l of lineOrder) {
            if (l !== "none" && out.length < visibleCount) {
                out.push(l);
            }
        }
        return out;
    }

    Kirigami.Icon {
        id: icon
        source: "run-build"
        anchors {
            left: parent.left
            verticalCenter: parent.verticalCenter
        }
        // fill the panel thickness; capped in very thick panels. On
        // vertical panels the width is fixed by the panel, so fit it too;
        // in horizontal panels parent.width derives from this item's own
        // width (J2: no feedback loop — only read parent.width when
        // vertical)
        height: compact.vertical
            ? Math.min(parent.width, Kirigami.Units.gridUnit * 3)
            : Math.min(parent.height, Kirigami.Units.gridUnit * 3)
        width: height
        opacity: compact.connected ? 1 : 0.35
    }

    Text {
        id: singleLine
        // L3: singleLine for lines <= 1 (two fields joined with ` · `)
        visible: compact.showDistance && compact.lines <= 1
        text: {
            const parts = compact.visibleLines.map(
                function (l) { return compact.lineTexts[l]; });
            return parts.join(" · ");
        }
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
        // L3: textCol for lines >= 2 — the two modes never overlap
        visible: compact.showDistance && compact.lines >= 2
        spacing: 0
        anchors {
            left: icon.right
            verticalCenter: parent.verticalCenter
            leftMargin: Kirigami.Units.smallSpacing
        }

        Repeater {
            model: compact.visibleLines
            Text {
                required property int index
                required property string modelData
                text: compact.lineTexts[modelData]
                color: index === 0 ? Kirigami.Theme.textColor
                    : Kirigami.Theme.disabledTextColor
                font.pixelSize: index === 0 ? icon.height / 2.2 : icon.height / 3.0
                font.bold: index === 0
            }
        }
    }

    // width follows the widest visible line plus the icon (G1: sized from
    // content, not anchored to the parent, so nothing is clipped)
    // G1/I1: singleLine covers lines <= 1 (distance-only or distance·speed)
    // K1: on vertical panels the width is the panel's to set — deriving it
    // from the icon feeds itself (icon grows with width grows with icon…),
    // so a constant is used there and the height follows the icon instead.
    // M2: the width terms follow the visibility conditions exactly — a
    // hidden Column keeps its implicitWidth, so a stopped walker on a
    // thick panel must not reserve the text width
    Layout.minimumWidth: compact.vertical
        ? Kirigami.Units.gridUnit
        : icon.width + Kirigami.Units.smallSpacing
          + (compact.showDistance && compact.lines <= 1
              ? singleLine.implicitWidth : 0)
          + (compact.showDistance && compact.lines >= 2
              ? textCol.implicitWidth : 0)
    Layout.preferredWidth: compact.vertical
        ? Layout.minimumWidth
        : icon.width + Kirigami.Units.smallSpacing
          + (compact.showDistance && compact.lines <= 1
              ? singleLine.implicitWidth : 0)
          + (compact.showDistance && compact.lines >= 2
              ? textCol.implicitWidth : 0)
    // K1: nothing else gave the item a height in a vertical panel
    Layout.minimumHeight: compact.vertical ? icon.height : 0
    Layout.preferredHeight: compact.vertical ? icon.height : 0
    // J2: no minimumHeight for horizontal panels — the thickness is the
    // user's choice, and forcing 2 gridUnit would hide the thin-panel
    // distance line (H2).

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
