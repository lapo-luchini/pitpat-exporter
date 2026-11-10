import QtQuick 2.15
import org.kde.notification as Notification

/**
 * Thin wrapper around the KF6 QML notification module. Exposed through a
 * Loader so the rest of the widget works even if the module is missing.
 */
Notification {
    id: self
    component: "Plasma"

    function notify(title, text) {
        self.compose({
            title: title,
            text: text,
            iconName: "run-build"
        });
    }

    onDefault: {}
}
