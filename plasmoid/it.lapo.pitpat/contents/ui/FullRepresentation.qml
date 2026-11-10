import QtQuick 2.15
import QtQuick.Layouts 1.15
import QtQuick.Controls as QQC
import org.kde.plasma.plasmoid 2.0
import org.kde.plasma.components 3.0 as PlasmaComponents3
import org.kde.kirigami as Kirigami

/**
 * Full popup (§10): same information and controls as the web page, minus
 * the sessions table (only the last 3 sessions), in native Plasma style.
 */
Plasmoid.FullRepresentation {
    id: full

    readonly property var snap: root.snap
    readonly property bool working: snap.connected
    readonly property bool onMove: snap.state === "running"
        || snap.state === "starting"
        || snap.state === "paused"
    readonly property bool ctlEnabled: working && !busy && snap.state !== "starting"
        && snap.state !== "stopped"

    property var lastSessions: []

    implicitWidth: Kirigami.Units.gridUnit * 18
    implicitHeight: Kirigami.Units.gridUnit * 22

    onSnapChanged: refreshSessions()

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

        // state / connection chip
        PlasmaComponents3.Label {
            text: working && onMove
                ? i18nc("chip", "%1 · %2 km",
                        snap.state,
                        (snap.session ? snap.session.distance_m / 1000 : 0).toFixed(2))
                : (working ? snap.state : i18n("pitpat daemon not running"))
            horizontalAlignment: Text.AlignHCenter
            Layout.fillWidth: true
            font.bold: true
            color: !working
                ? Kirigami.Theme.disabledTextColor
                : snap.state === "running" ? "#2ecc71"
                : snap.state === "paused" ? "#f1c40f"
                : Kirigami.Theme.textColor
        }

        PlasmaComponents3.Heading {
            level: 4
            text: onMove ? Number(snap.speed_kmh).toFixed(1) : "–"
            Layout.alignment: Qt.AlignHCenter
            font.pointSize: 34
        }

        RowLayout {
            spacing: Kirigami.Units.mediumSpacing * 2
            Layout.alignment: Qt.AlignHCenter
            PlasmaComponents3.Label {
                text: snap.session ? i18n("%1 km", (snap.session.distance_m / 1000).toFixed(2)) : "–"
            }
            PlasmaComponents3.Label {
                text: snap.session ? i18n("%1 steps", snap.session.steps) : "–"
            }
            PlasmaComponents3.Label {
                text: snap.session ? i18n("%1 kcal", snap.session.kcal) : "–"
            }
            PlasmaComponents3.Label {
                text: snap.session ? i18n("%1 min", Math.round(snap.session.duration_s / 60)) : "–"
            }
        }

        // target progress
        PlasmaComponents3.Label {
            visible: snap.target && snap.session
            text: {
                const t = snap.target;
                const s = snap.session;
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
                text: snap.today ? i18n("today %1 km", (snap.today.distance_m / 1000).toFixed(2)) : ""
            }
            PlasmaComponents3.Label {
                text: snap.lifetime ? i18n("lifetime %1 km", (snap.lifetime.distance_m / 1000).toFixed(1)) : ""
                color: Kirigami.Theme.disabledTextColor
            }
        }

        // controls row: toggle, ±0.1, slider
        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing
            PlasmaComponents3.Button {
                display: PlasmaComponents3.ToolButton.IconOnly
                width: height
                icon.name: snap.state === "running" ? "media-playback-pause" : "media-playback-start"
                enabled: working && !busy && snap.state !== "starting"
                onClicked: root.toggleStartPause()
            }
            PlasmaComponents3.ToolButton {
                text: "−0.1"
                enabled: root.ctlEnabled
                onClicked: root.speedDelta(-0.1)
            }
            PlasmaComponents3.ToolButton {
                text: "+0.1"
                enabled: root.ctlEnabled
                onClicked: root.speedDelta(0.1)
            }
            QQC.Slider {
                id: speedSlider
                Layout.fillWidth: true
                from: snap.min_speed_kmh
                to: snap.max_speed_kmh
                stepSize: 0.1
                value: snap.target_speed_kmh
                enabled: root.ctlEnabled
                // send on release, as the web page does (§9)
                onReleased: root.setSpeed(value)
            }
        }

        // presets
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Repeater {
                model: snap.presets_kmh
                PlasmaComponents3.ToolButton {
                    required property real modelData
                    text: modelData.toFixed(1).replace(/\.0$/, "")
                    enabled: root.ctlEnabled || (full.working && snap.state !== "starting")
                    onClicked: root.setSpeed(modelData)
                }
            }
            Item {
                Layout.fillWidth: true
            }
        }

        // stop: visually distinct, always reachable while working (§9)
        PlasmaComponents3.Button {
            enabled: working && !busy && snap.state !== "stopped"
            text: i18n("Stop")
            icon.name: "media-playback-stop"
            Layout.fillWidth: true
            Kirigami.Theme.colorSet: Kirigami.Theme.Selection
            palette {
                button: "#c0392b"
                buttonText: "#ffffff"
            }
        }

        // target inputs with Set/Clear
        RowLayout {
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

        // last 3 sessions
        PlasmaComponents3.Label {
            text: i18n("Last sessions")
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
                text: i18n("%1 — %2 km, %3 steps, %4 min",
                    new Date(modelData.start).toLocaleString(Qt.locale(), "MM-dd hh:mm"),
                    (modelData.distance_m / 1000).toFixed(2),
                    modelData.steps,
                    Math.round(modelData.duration_s / 60))
                elide: Text.ElideRight
            }
            visible: count > 0
        }
        PlasmaComponents3.Label {
            visible: full.lastSessions.length === 0
            text: root.working ? i18n("no sessions yet") : ""
            color: Kirigami.Theme.disabledTextColor
        }
    }
}
