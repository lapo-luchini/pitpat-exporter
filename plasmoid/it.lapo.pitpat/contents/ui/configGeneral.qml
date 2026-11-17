// SPDX-License-Identifier: ISC
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC
import org.kde.kcmutils as KCM
import org.kde.kirigami as Kirigami

/**
 * Config page (§10): daemon URL, token, milestone km (0 = off), show speed
 * in panel. The `cfg_*` properties are filled in by the configuration
 * dialog from the KConfigXT keys — do not bind them to
 * Plasmoid.configuration here (REVIEW §1.3).
 */
KCM.SimpleKCM {
    id: general

    property string cfg_daemonUrl: ""
    property string cfg_token: ""
    property real cfg_milestoneKm: 1.0
    property string cfg_line1: "distance"
    property string cfg_line2: "time"
    property string cfg_line3: "speed"

    Kirigami.FormLayout {
        QQC.TextField {
            id: daemonUrlField
            text: general.cfg_daemonUrl
            placeholderText: "http://127.0.0.1:9839"
            onTextChanged: general.cfg_daemonUrl = text
            Kirigami.FormData.label: i18n("Daemon URL:")
        }
        QQC.TextField {
            id: tokenField
            text: general.cfg_token
            echoMode: TextInput.Password
            onTextChanged: general.cfg_token = text
            Kirigami.FormData.label: i18n("API token:")
        }
        QQC.SpinBox {
            id: milestoneSpin
            from: 0
            to: 100
            value: general.cfg_milestoneKm
            editable: true
            stepSize: 1
            onValueModified: general.cfg_milestoneKm = value
            textFromValue: function (value, locale) {
                return Number(value).toFixed(2);
            }
            valueFromText: function (text, locale) {
                return Number.fromLocaleString(locale, text);
            }
            Kirigami.FormData.label: i18n("Milestone every X km (0 = off):")
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: true
            type: Kirigami.MessageType.Information
            text: i18n("The token is only needed for non-loopback daemons.")
        }
        // K2: panel line order (thickness still picks how many lines show)
        QQC.ComboBox {
            id: line1Box
            model: ["distance", "time", "speed"]
            textRole: "modelData"
            displayText: currentText
            currentIndex: model.indexOf(general.cfg_line1)
            onActivated: general.cfg_line1 = currentText
            Kirigami.FormData.label: i18n("Panel line 1:")
        }
        QQC.ComboBox {
            model: ["distance", "time", "speed", "none"]
            textRole: "modelData"
            displayText: currentText
            currentIndex: model.indexOf(general.cfg_line2)
            onActivated: general.cfg_line2 = currentText
            Kirigami.FormData.label: i18n("Panel line 2:")
        }
        QQC.ComboBox {
            model: ["distance", "time", "speed", "none"]
            textRole: "modelData"
            displayText: currentText
            currentIndex: model.indexOf(general.cfg_line3)
            onActivated: general.cfg_line3 = currentText
            Kirigami.FormData.label: i18n("Panel line 3:")
        }
    }
}
