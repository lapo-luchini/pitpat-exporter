// SPDX-License-Identifier: ISC
import QtQuick 2.15
import org.kde.notification as Notification

/**
 * Milestone/target/disconnect notifications through the KF6 QML
 * notification module. componentName "plasma_workspace" + eventId
 * "notification" works without shipping a .notifyrc file.
 */
Notification.Notification {
    id: self

    componentName: "plasma_workspace"
    eventId: "notification"
    iconName: "run-build"

    function notify(title, text) {
        self.title = title;
        self.text = text;
        self.sendEvent();
    }
}
