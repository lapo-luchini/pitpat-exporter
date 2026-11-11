// SPDX-License-Identifier: ISC
import QtQuick 2.15
// DataSource moved out of org.kde.plasma.core in Plasma 6 (REVIEW A5)
import org.kde.plasma.plasma5support as P5Support

/**
 * Fallback notifications: `notify-send` via the executable DataEngine,
 * used when the KF6 QML notification module is unavailable (§10).
 */
Item {
    id: bridge

    P5Support.DataSource {
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
