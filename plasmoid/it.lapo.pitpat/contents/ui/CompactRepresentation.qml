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
 * Vertical panels: icon only. When stopped: icon only.
 *
 * Q1: the content is one RowLayout (icon, then text), so the text can
 * only ever sit right of the icon, and the width hints come from the
 * row's content width — the panel can never fall back to an icon-sized
 * square with the text painted over it. `clip` keeps everything inside
 * the widget's area whatever size the panel gives. Two width modes:
 * `auto` (fit the content, default) and `fixed` (config `panelWidth`,
 * text elides right, the icon always shows in full).
 */
Item {
    id: compact
    // Q1: nothing is ever painted outside the widget's area
    clip: true

    readonly property bool connected: root.snap.connected
    readonly property bool hasSession: root.snap.session !== null
    readonly property bool onMove: ["starting", "running", "paused"].indexOf(
        root.snap.state) >= 0
    readonly property real sessionKm: hasSession
        ? root.snap.session.distance_m / 1000.0
        : 0.0

    readonly property bool vertical:
        Plasmoid.formFactor === PlasmaCore.Types.Vertical

    // Q1: width mode — `auto` fits the content (default), `fixed` uses
    // `panelWidth` (Plasma 6 panels can't be resized by hand, so the
    // fixed width lives here)
    readonly property bool fixedWidth:
        (Plasmoid.configuration.panelWidthMode || "auto") === "fixed"
    readonly property int panelWidth: Plasmoid.configuration.panelWidth || 120

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
    readonly property bool showText: onMove && !vertical

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

    // Q1/no-jitter: every line is sized for the WIDEST value of its
    // current format (a template picked by field and magnitude), not for
    // the live text — combined with tabular figures the width only
    // changes when a value gains a digit (e.g. the first hour)
    readonly property var templates: ({
        distance: sessionKm >= 1000 ? "0000.00 km"
            : sessionKm >= 100 ? "000.00 km"
            : sessionKm >= 10 ? "00.00 km"
            : "0.00 km",
        time: (hasSession && root.snap.session.duration_s >= 3600)
            ? "0:00:00" : "00:00",
        speed: root.snap.speed_kmh >= 10 ? "00.0 km/h" : "0.0 km/h",
    })
    function lineTemplate(i) {
        const f = visibleLines[i];
        return f ? templates[f] : "";
    }
    // singleLine joins its fields with ` · ` like the live text
    readonly property string singleTemplate:
        visibleLines.map(function (l) { return templates[l]; }).join(" · ")

    // the fonts must match the rendered texts exactly (line 0 is the
    // larger bold one, the rest are the smaller grey ones)
    TextMetrics {
        id: singleMetrics
        font.pixelSize: icon.height / 2.4
        font.features: { "tnum": 1 }
        text: compact.singleTemplate
    }
    TextMetrics {
        id: line0Metrics
        font.pixelSize: icon.height / 2.2
        font.bold: true
        font.features: { "tnum": 1 }
        text: compact.lineTemplate(0)
    }
    TextMetrics {
        id: line1Metrics
        font.pixelSize: icon.height / 3.0
        font.features: { "tnum": 1 }
        text: compact.lineTemplate(1)
    }
    TextMetrics {
        id: line2Metrics
        font.pixelSize: icon.height / 3.0
        font.features: { "tnum": 1 }
        text: compact.lineTemplate(2)
    }
    // widest visible line of the column mode
    readonly property real columnTextWidth:
        Math.max(line0Metrics.width, line1Metrics.width, line2Metrics.width)

    readonly property real iconSpace:
        icon.width + Kirigami.Units.smallSpacing

    // R1: the icon fills the panel thickness, capped in very thick
    // panels; on vertical panels the width is fixed by the panel, so fit
    // that instead (J2: the size comes from the shell-provided compact
    // item, never from the row, so there is no feedback loop)
    readonly property real iconSize: compact.vertical
        ? Math.min(compact.width, Kirigami.Units.gridUnit * 3)
        : Math.min(compact.height, Kirigami.Units.gridUnit * 3)

    // Q1: one layout tree — icon, then the one-line text or the column of
    // lines, all aligned vertically by the layout, no manual anchors, so
    // the text can only ever sit to the right of the icon
    RowLayout {
        id: row
        anchors {
            left: parent.left
            right: parent.right
            verticalCenter: parent.verticalCenter
        }
        spacing: Kirigami.Units.smallSpacing

        Kirigami.Icon {
            id: icon
            source: "run-build"
            Layout.alignment: Qt.AlignVCenter
            // R1: a layout sets width/height of its children itself
            // (from Layout.preferred* or else the implicit size), so the
            // panel-thickness binding must be on the Layout hints, not
            // on width/height — a first-pass override would freeze the
            // icon at the initial size forever. min/max keep it square
            // and unstretched in fixed-width mode. J2: the size comes
            // from the shell-provided compact item, never from the row.
            Layout.preferredWidth: iconSize
            Layout.preferredHeight: iconSize
            Layout.minimumWidth: iconSize
            Layout.maximumWidth: iconSize
            Layout.minimumHeight: iconSize
            Layout.maximumHeight: iconSize
            opacity: compact.connected ? 1 : 0.35
        }

        Text {
            id: singleLine
            // L3: singleLine for lines <= 1 (two fields joined with ` · `)
            visible: compact.showText && compact.lines <= 1
            text: {
                const parts = compact.visibleLines.map(
                    function (l) { return compact.lineTexts[l]; });
                return parts.join(" · ");
            }
            color: Kirigami.Theme.textColor
            font.pixelSize: icon.height / 2.4
            font.features: { "tnum": 1 }
            elide: Text.ElideRight
            // Q1: width from the template (no-jitter); fillWidth makes
            // fixed-width mode elide whatever does not fit
            Layout.fillWidth: true
            Layout.preferredWidth: singleMetrics.width
            Layout.alignment: Qt.AlignVCenter
        }

        ColumnLayout {
            id: textCol
            // L3: textCol for lines >= 2 — the two modes never overlap
            visible: compact.showText && compact.lines >= 2
            spacing: 0
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter

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
                    font.features: { "tnum": 1 }
                    elide: Text.ElideRight
                    // Q1: width from the template (no-jitter)
                    Layout.fillWidth: true
                    Layout.preferredWidth: index === 0 ? line0Metrics.width
                        : index === 1 ? line1Metrics.width
                        : line2Metrics.width
                }
            }
        }
    }

    // Q1: the width hints come from the row's content width, so the
    // panel can never fall back to a square: min = preferred = content.
    // Fixed mode: min = preferred = max = `panelWidth` — but the icon
    // always shows in full, so it wins if the width is smaller.
    // Stopped (M2): icon only in both modes, no text width reserved.
    // K1: on vertical panels the width is the panel's to set.
    Layout.minimumWidth: compact.vertical ? Kirigami.Units.gridUnit
        : compact.fixedWidth && compact.showText
            ? Math.max(compact.panelWidth, compact.iconSpace)
        : row.implicitWidth
    Layout.preferredWidth: Layout.minimumWidth
    Layout.maximumWidth: compact.vertical || !compact.showText
            || !compact.fixedWidth
        ? -1
        : Math.max(compact.panelWidth, compact.iconSpace)
    // K1: nothing else gave the item a height in a vertical panel; Q1:
    // in horizontal panels the thickness comes from the panel, and an
    // explicit 0 is not "unset", so -1 is used instead
    Layout.minimumHeight: compact.vertical ? icon.height : -1
    Layout.preferredHeight: compact.vertical ? icon.height : -1

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
