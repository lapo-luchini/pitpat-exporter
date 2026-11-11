// SPDX-License-Identifier: ISC
import QtQuick 2.15
import org.kde.plasma.plasmoid 2.0
import org.kde.kirigami as Kirigami

/**
 * Compact panel icon (§10): standard icon greyed out when disconnected;
 * speed + session distance shown while a session is open (when configured
 * and metric); middle-click toggles start/pause, mouse wheel ±0.1 km/h,
 * left click opens the popup. The Plasma tooltip lives on the PlasmoidItem
 * (toolTipMainText/toolTipSubText in main.qml).
 */
Item {
    id: compact

    readonly property bool connected: root.snap.connected
    readonly property bool hasSession: root.snap.session !== null
    readonly property real sessionKm: hasSession ? root.snap.session.distance_m / 1000.0 : 0.0
    readonly property bool showSpeed: Plasmoid.configuration.showSpeedInPanel
        && root.moving && !root.snap.imperial

    Layout.minimumWidth: Kirigami.Units.gridUnit * 2
    Layout.minimumHeight: Kirigami.Units.gridUnit * 2

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
            // accept only accumulated wheel events so one notch = ±0.1
            if (wheel.angleDelta.y === 0) {
                return;
            }
            root.wheelAdjust(wheel.angleDelta.y > 0);
        }
    }

    Item {
        id: layout
        anchors.fill: parent
        opacity: compact.connected ? 1 : 0.35

        Kirigami.Icon {
            id: icon
            source: "run-build"
            anchors {
                left: parent.left
                verticalCenter: parent.verticalCenter
            }
            width: height
            height: parent.height
        }

        Text {
            text: compact.showSpeed
                ? root.snap.speed_kmh.toFixed(1) + " km/h"
                : (compact.hasSession ? compact.sessionKm.toFixed(2) + " km" : "")
            color: Kirigami.Theme.textColor
            font.pixelSize: layout.height / 2.4
            visible: text.length > 0
            horizontalAlignment: Text.AlignHCenter
            anchors {
                left: icon.right
                right: parent.right
                verticalCenter: parent.verticalCenter
            }
        }
    }
}
