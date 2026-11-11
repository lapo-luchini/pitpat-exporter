// SPDX-License-Identifier: ISC
import QtQuick 2.15
import org.kde.plasma.core as PlasmaCore

/**
 * Fallback notifications: `notify-send` via the executable DataEngine,
 * used when the KF6 QML notification module is unavailable (§10).
 */
Item {
    id: bridge

    PlasmaCore.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => disconnectSource(source)
    }

    function shellQuote(s) {
        return "'" + s.replace(/'/g, "'\\''") + "'";
    }

    function notify(title, text) {
        exec.connectSource(
            "notify-send -i run-build " + shellQuote(title) + " " + shellQuote(text)
        );
    }
}
