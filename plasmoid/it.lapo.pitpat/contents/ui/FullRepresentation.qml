// SPDX-License-Identifier: ISC
import QtQuick 2.15
import QtQuick.Layouts
import QtQuick.Controls as QQC
import org.kde.plasma.plasmoid 2.0
import org.kde.plasma.components 3.0 as PlasmaComponents3
import org.kde.kirigami as Kirigami

/**
 * Full popup (§10): same information and controls as the web page, minus
 * the sessions table (only the last 3 sessions), in native Plasma style.
 * Speed/presets stay available while the belt is merely stopped (§3.6);
 * only a disconnected daemon or the starting ramp disables them.
 */
Item {
    id: full

    readonly property var snap: root.snap
    readonly property bool working: snap.connected
    // §4.5a: asleep still accepts start/preset/speed/stop — they wake it
    readonly property bool asleep: snap.state === "sleeping"
    readonly property bool reachable: working || asleep
    readonly property bool onMove: snap.state === "running"
        || snap.state === "starting"
        || snap.state === "paused"
    // stopped still allows set-speed: it only updates the remembered
    // target speed for the next start (§3.6); asleep wakes (§4.5a)
    readonly property bool ctlEnabled: reachable && !root.busy && snap.state !== "starting"

    property var lastSessions: []
    // K4/L1: the Target section; opens by default when a target is set
    property bool targetOpen: snap.target
        && (snap.target.distance_m || snap.target.duration_s) ? true : false
    // K5: fetch sessions only when the popup opens, when a walk starts or
    // ends (the last_end_reason / session-start key changes), and once a
    // minute otherwise — not on every 1 s snapshot
    property string lastSessionKey: ""
    property string lastEndKey: ""
    property bool lastFetchMinute: false
    property var snapArrival: new Date()

    function sessionKeyChanged() {
        const start = snap.session ? snap.session.start : "";
        const end = snap.last_end_reason || "";
        if (start !== lastSessionKey || end !== lastEndKey) {
            lastSessionKey = start;
            lastEndKey = end;
            return true;
        }
        const now = new Date();
        if (now - snapArrival > 60000) {
            snapArrival = now;
            return lastFetchMinute;
        }
        return false;
    }

    // F4: a counter still at 0 after ~50 m of walking is treated as
    // "not reported by this treadmill" and hidden
    function sessionCounterUnreported(v) {
        const ses = snap.session;
        return !ses || (v === 0 && ses.distance_m > 50);
    }



    implicitWidth: Kirigami.Units.gridUnit * 18
    implicitHeight: Kirigami.Units.gridUnit * 22

    onSnapChanged: {
        const d = new Date();
        if (lastFetchMinute === false) {
            lastFetchMinute = true;
        } else if (d.getSeconds() !== snapArrival.getSeconds()) {
            snapArrival = snapArrival; // keep the reference date stable
        }
        refreshSessions();
    }

    function refreshSessions() {
        var xhr = new XMLHttpRequest();
        xhr.open("GET", root.apiBase + "/api/sessions?limit=3");
        xhr.onreadystatechange = function () {
            if (xhr.readyState !== XMLHttpRequest.DONE) {
                return;
            }
            if (xhr.status === 200) {
                try {
                    full.lastSessions = JSON.parse(xhr.responseText);
                } catch (e) {
                }
            }
        };
        xhr.send();
    }

    function applyTarget() {
        const km = parseFloat(distanceField.text);
        const minutes = parseInt(timeField.text, 10);
        root.setTarget(km > 0 ? km * 1000 : 0, minutes > 0 ? minutes * 60 : 0);
        distanceField.text = "";
        timeField.text = "";
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        // F3: pin — the popup stays open and live-updates during a walk
        RowLayout {
            Layout.fillWidth: true
            Item { Layout.fillWidth: true }
            PlasmaComponents3.ToolButton {
                icon.name: "window-pin"
                checkable: true
                checked: Plasmoid.configuration.pin
                onToggled: Plasmoid.configuration.pin = checked
                Accessible.name: i18n("Keep popup open")
            }
        }

        // K6/L4: walks not being saved must stay visible
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: full.snap.persist_ok === false
            type: Kirigami.MessageType.Warning
            text: i18n("Walks are not being saved: %1",
                       full.snap.persist_error || "unknown reason")
        }

        // state / connection chip (K3 labels, coloured dot)
        RowLayout {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignHCenter
            spacing: Kirigami.Units.smallSpacing
            Rectangle {
                width: Kirigami.Units.smallSpacing * 1.2
                height: width
                radius: width / 2
                visible: full.working || full.asleep
                color: full.snap.state === "running" ? "#2ecc71"
                    : full.snap.state === "paused" ? "#f1c40f"
                    : full.snap.state === "starting" ? "#3498db"
                    : full.asleep ? Kirigami.Theme.disabledTextColor
                    : Kirigami.Theme.textColor
            }
            PlasmaComponents3.Label {
                text: full.working || full.asleep
                    ? root.stateLabel(full.snap.state)
                    : i18n("pitpat daemon not running")
                font.bold: true
            }
            Item { Layout.fillWidth: true }
        }

        // K4 hero row: distance / time / speed, same formatters as the panel
        RowLayout {
            spacing: Kirigami.Units.mediumSpacing * 2
            Layout.alignment: Qt.AlignHCenter
            Kirigami.Heading {
                level: 2
                text: full.snap.session ? fmtDistance(full.snap.session.distance_m / 1000) : "–"
            }
            Kirigami.Heading {
                level: 3
                text: full.snap.session ? fmtTime(full.snap.session.duration_s) : "–"
            }
            Kirigami.Heading {
                level: 3
                text: fmtSpeed(full.snap.speed_kmh)
            }
        }

        RowLayout {
            spacing: Kirigami.Units.mediumSpacing * 2
            Layout.alignment: Qt.AlignHCenter
            // F4: some models don't report steps/kcal — hide a zero
            // counter once the session distance shows the walk is real
            PlasmaComponents3.Label {
                visible: full.snap.session
                    && !full.sessionCounterUnreported(full.snap.session.steps)
                text: full.snap.session ? i18n("%1 steps", full.snap.session.steps) : ""
            }
            PlasmaComponents3.Label {
                visible: full.snap.session
                    && !full.sessionCounterUnreported(full.snap.session.kcal)
                text: full.snap.session ? i18n("%1 kcal", full.snap.session.kcal) : ""
            }
        }

        // target progress
        PlasmaComponents3.Label {
            visible: full.snap.target && full.snap.session
            text: {
                const t = full.snap.target;
                const s = full.snap.session;
                if (!t || !s) {
                    return "";
                }
                if (t.distance_m) {
                    return i18n("%1% of %2 m", (s.distance_m / t.distance_m * 100).toFixed(0), t.distance_m);
                }
                if (t.duration_s) {
                    return i18n("%1 of %2 min", Math.round(s.duration_s / 60), Math.round(t.duration_s / 60));
                }
                return "";
            }
        }

        // today + lifetime
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.alignment: Qt.AlignHCenter
            PlasmaComponents3.Label {
                text: full.snap.today ? i18n("today %1 km", (full.snap.today.distance_m / 1000).toFixed(2)) : ""
            }
            PlasmaComponents3.Label {
                text: full.snap.lifetime ? i18n("lifetime %1 km", (full.snap.lifetime.distance_m / 1000).toFixed(1)) : ""
                color: Kirigami.Theme.disabledTextColor
            }
        }

        // controls row: toggle, stop (K4: ToolButton, no full-width red
        // row), ±0.1, slider
        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing
            PlasmaComponents3.ToolButton {
                display: PlasmaComponents3.AbstractButton.IconOnly
                icon.name: full.snap.state === "running" ? "media-playback-pause" : "media-playback-start"
                enabled: full.reachable && !root.busy && full.snap.state !== "starting"
                onClicked: root.toggleStartPause()
            }
            PlasmaComponents3.ToolButton {
                display: PlasmaComponents3.AbstractButton.IconOnly
                icon.name: "media-playback-stop"
                enabled: full.reachable && !root.busy && full.snap.state !== "stopped"
                onClicked: root.stop()
                QQC.ToolTip.text: i18n("Stop and end the walk")
                QQC.ToolTip.visible: hovered
            }
            PlasmaComponents3.Button {
                // §4.5a: waking without starting the belt
                visible: full.asleep
                text: i18n("Connect")
                icon.name: "network-connect"
                enabled: !root.busy
                onClicked: root.api("POST", "/api/connect", {})
            }
            PlasmaComponents3.ToolButton {
                text: "−0.1"
                enabled: full.ctlEnabled
                onClicked: root.speedDelta(-0.1)
            }
            PlasmaComponents3.ToolButton {
                text: "+0.1"
                enabled: full.ctlEnabled
                onClicked: root.speedDelta(0.1)
            }
            QQC.Slider {
                id: speedSlider
                Layout.fillWidth: true
                from: full.snap.min_speed_kmh
                to: full.snap.max_speed_kmh
                stepSize: 0.1
                value: full.snap.target_speed_kmh
                enabled: full.ctlEnabled
                // send when the drag stops, as the web page does (§9):
                // QQC2's Slider has no released signal, only moved(), so
                // the send is debounced 250 ms after the last change
                onMoved: sliderSend.restart()
            }
            Timer {
                id: sliderSend
                interval: 250
                onTriggered: root.setSpeed(speedSlider.value)
            }
        }

        // presets
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Repeater {
                model: full.snap.presets_kmh
                PlasmaComponents3.ToolButton {
                    required property real modelData
                    text: modelData.toFixed(1).replace(/\.0$/, "")
                    enabled: full.ctlEnabled
                    onClicked: root.setSpeed(modelData)
                }
            }
            Item {
                Layout.fillWidth: true
            }
        }

        // K4: target section, collapsed by default
        PlasmaComponents3.ToolButton {
            id: targetToggle
            text: full.targetOpen
                ? i18n("Target ▾")
                : i18n("Target ▸")
            icon.name: "flag-red"
            onClicked: full.targetOpen = !full.targetOpen
        }
        RowLayout {
            visible: full.targetOpen
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            QQC.TextField {
                id: distanceField
                Layout.fillWidth: true
                placeholderText: i18n("km target")
                onAccepted: full.applyTarget()
            }
            QQC.TextField {
                id: timeField
                Layout.fillWidth: true
                placeholderText: i18n("minutes")
                onAccepted: full.applyTarget()
            }
            PlasmaComponents3.Button {
                text: i18n("Set")
                enabled: distanceField.text.length > 0 || timeField.text.length > 0
                onClicked: full.applyTarget()
            }
            PlasmaComponents3.Button {
                text: i18n("Clear")
                onClicked: root.clearTarget()
            }
        }

        Kirigami.Separator {
            Layout.fillWidth: true
            visible: full.lastSessions.length > 0
        }
        PlasmaComponents3.Label {
            text: i18n("Last walks")
            font.bold: true
        }
        ListView {
            id: sessionList
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            spacing: Kirigami.Units.smallSpacing / 2
            model: full.lastSessions
            delegate: PlasmaComponents3.Label {
                required property var modelData
                text: {
                    const counters = modelData.steps > 0
                        ? i18n(", %1 steps", modelData.steps)
                        : "";
                    return i18n("%1 — %2 km%3, %4 min",
                        new Date(modelData.start).toLocaleString(Qt.locale(), "MM-dd hh:mm"),
                        (modelData.distance_m / 1000).toFixed(2),
                        counters,
                        Math.round(modelData.duration_s / 60));
                }
                elide: Text.ElideRight
            }
            visible: count > 0
        }
        PlasmaComponents3.Label {
            visible: full.lastSessions.length === 0
            text: full.working ? i18n("no walks yet") : ""
            color: Kirigami.Theme.disabledTextColor
        }
    }
}
