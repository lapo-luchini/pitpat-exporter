import QtQuick 2.15
import QtQuick.Controls as QQC
import org.kde.plasma.plasmoid 2.0
import org.kde.kirigami as Kirigami

/**
 * Compact panel icon (§10): a standard icon greyed out when disconnected;
 * speed + session distance shown while a session is open; middle-click
 * toggles start/pause, mouse wheel adjusts ±0.1 km/h.
 */
Plasmoid.CompactRepresentation {
    readonly property bool connected: root.snap.connected
    readonly property bool hasSession: root.snap.session !== null
    readonly property real sessionKm: hasSession ? root.snap.session.distance_m / 1000.0 : 0.0
    readonly property bool showSpeed: Plasmoid.configuration.showSpeedInPanel
        && root.moving && !root.snap.imperial

    Item {
        id: layout
        anchors.fill: parent
        opacity: connected ? 1 : 0.35

        Kirigami.Icon {
            id: icon
            source: "run-build"
            anchors {
                left: parent.left
                verticalCenter: parent.verticalCenter
            }
        }

        Text {
            text: showSpeed
                ? root.snap.speed_kmh.toFixed(1) + " km/h"
                : (hasSession ? sessionKm.toFixed(2) + " km" : "")
            color: Kirigami.Theme.textColor
            font.pixelSize: layout.height / 2.4
            visible: text.length > 0
            horizontalAlignment: Text.AlignHCenter
            anchors {
                right: parent.right
                verticalCenter: parent.verticalCenter
            }
        }
    }

    MouseArea {
        id: mouse
        hoverEnabled: true
        acceptedButtons: Qt.LeftButton | Qt.MiddleButton
        anchors.fill: parent
        onClicked: mouse => {
            if (mouse.button === Qt.MiddleButton) {
                root.toggleStartPause();
            }
        }
        onWheel: wheel => {
            root.wheelAdjust(wheel.angleDelta.y > 0);
        }

        // tooltip: state, speed, session distance, today's distance (§10)
        QQC.ToolTip {
            id: tip
            visible: mouse.containsMouse && mouse.buttons === Qt.NoButton
            delay: 350
            timeout: 8000
            text: {
                const head = root.connected
                    ? i18nc("tooltip head", "%1 · %2 km/h",
                            root.snap.state, root.snap.speed_kmh.toFixed(1))
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
