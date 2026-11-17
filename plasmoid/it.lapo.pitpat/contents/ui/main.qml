// SPDX-License-Identifier: ISC
import QtQuick 2.15
import org.kde.plasma.plasmoid 2.0

/**
 * State engine shared by the compact + full representations: polls
 * /api/state, POSTs commands with the JSON content-type (+ optional
 * token), and fires the milestone / target / disconnect notifications.
 */
PlasmoidItem {
    id: root

    // F3: while pinned the popup stays open and live-updates
    hideOnWindowDeactivate: !Plasmoid.configuration.pin

    // ------------------------------------------------------------ state
    readonly property string apiBase: Plasmoid.configuration.daemonUrl
    // K3/K4: one formatter set per UI, shared by both representations
    function fmtDistance(km) { return Number(km).toFixed(2) + " km"; }
    function fmtSpeed(kmh) { return Number(kmh).toFixed(1) + " km/h"; }
    function fmtTime(sec) {
        const s = Math.round(sec);
        const h = Math.floor(s / 3600);
        const m = Math.floor((s % 3600) / 60);
        const r = s % 60;
        return h > 0
            ? h + ":" + String(m).padStart(2, "0") + ":" + String(r).padStart(2, "0")
            : m + ":" + String(r).padStart(2, "0");
    }

    // K3: machine ids stay as-is in the API; only human text maps them
    function stateLabel(state) {
        switch (state) {
            case "running": return i18n("walking");
            case "starting": return i18n("starting…");
            case "paused": return i18n("paused");
            case "stopped": return i18n("stopped");
            case "sleeping": return i18n("asleep");
            case "disconnected": return i18n("disconnected");
            default: return state;
        }
    }

    readonly property string token: Plasmoid.configuration.token || ""
    readonly property real milestoneKm: Plasmoid.configuration.milestoneKm || 0
    // snap starts as a disconnected stub so the representations can bind
    property var snap: ({
        connected: false,
        address: null,
        state: "disconnected",
        imperial: false,
        speed_kmh: 0.0,
        target_speed_kmh: 2.0,
        min_speed_kmh: 1.0,
        max_speed_kmh: 6.0,
        presets_kmh: [],
        session: null,
        target: null,
        today: null,
        lifetime: null,
        last_end_reason: null
    })
    property bool busy: false
    // guard against re-notifying the disconnect on every failed poll
    property bool wasRunning: false
    // while the popup is open or the belt moves: 1 s, otherwise 10 s (§10)
    readonly property bool moving: ["starting", "running", "paused"].indexOf(snap.state) >= 0
    readonly property int pollInterval: root.expanded || root.moving ? 1000 : 10000

    // notification bookkeeping (§10)
    property int lastMilestone: 0
    property string lastSessionStart: ""
    property string lastEndSeen: ""
    // the first successful poll baselines lastEndSeen without notifying
    // (REVIEW round-3 B1): a widget restart must not re-announce a stale
    // end reason
    property bool endSeenBaseline: false

    function refresh() {
        var xhr = new XMLHttpRequest();
        xhr.open("GET", apiBase + "/api/state");
        xhr.onreadystatechange = function () {
            if (xhr.readyState !== XMLHttpRequest.DONE) {
                return;
            }
            if (xhr.status === 200) {
                loggedDaemonFail = false;
                try {
                    root.onSnap(JSON.parse(xhr.responseText));
                } catch (e) {
                    console.warn("pitpat: bad JSON from daemon");
                }
            } else {
                // unreachable daemon; log only once (§10)
                if (!loggedDaemonFail) {
                    console.warn("pitpat: daemon unreachable");
                    loggedDaemonFail = true;
                }
                root.snap = Object.assign({}, root.snap, {
                    connected: false,
                    state: "disconnected",
                    speed_kmh: 0,
                    session: null,
                    target: null,
                    address: null
                });
                root.wasRunning = false;
            }
        };
        xhr.send();
    }
    property bool loggedDaemonFail: false

    function onSnap(next) {
        // milestone counter must follow the session, not the daemon's reach
        // (a new session restarts at 0 km — REVIEW §1.10). On the first
        // snapshot after a widget (re)start the counter baselines from the
        // session already in progress, so a milestone walked before the
        // restart is not re-announced (REVIEW round-5 optional).
        const start = next.session ? next.session.start : "";
        if (start !== lastSessionStart) {
            lastSessionStart = start;
            root.lastMilestone = next.session && root.milestoneKm > 0
                ? Math.floor(next.session.distance_m / 1000.0 / root.milestoneKm)
                : 0;
        }
        root.snap = next;

        if (root.milestoneKm > 0 && next.session) {
            const n = Math.floor(next.session.distance_m / 1000.0 / root.milestoneKm);
            if (n >= 1 && n > root.lastMilestone) {
                root.lastMilestone = n;
                root.notify(
                    i18n("PitPat milestone %1", n),
                    i18n("You walked %1 km in this walk!", (n * root.milestoneKm).toFixed(2))
                );
            }
        }
        // auto-stop detection: the daemon clears target and session in the
        // same snapshot, so watch the end reason instead (REVIEW §1.9).
        // null clears the seen-marker so two consecutive target_reached
        // sessions both notify (REVIEW round-3 B1).
        if (!endSeenBaseline) {
            endSeenBaseline = true;
            lastEndSeen = next.last_end_reason || "";
        } else if (!next.last_end_reason) {
            lastEndSeen = "";
        } else if (next.last_end_reason !== lastEndSeen) {
            if (next.last_end_reason === "target_reached") {
                root.notify(i18n("PitPat target reached"), i18n("Auto-stop: the belt is stopping"));
            }
            lastEndSeen = next.last_end_reason;
        }
        if (root.wasRunning && !next.connected) {
            root.notify(i18n("PitPat disconnected"), i18n("Lost the treadmill while walking"));
        }
        root.wasRunning = next.state === "running" || next.state === "starting";

        // Plasma tooltip (§10): state, speed, session distance, today
        toolTipMainText = next.connected
            ? i18nc("tooltip head", "%1 · %2 km/h",
                    root.stateLabel(next.state), next.speed_kmh.toFixed(1))
            : next.state === "sleeping"
                ? i18nc("tooltip head", "asleep")
                : i18n("pitpat daemon not running");
        const ses = next.session
            ? i18nc("tooltip", "walk: %1 km", (next.session.distance_m / 1000).toFixed(2))
            : i18nc("tooltip empty", "no walks yet");
        const today = next.today
            ? i18nc("tooltip", "today: %1 km", (next.today.distance_m / 1000).toFixed(2))
            : "";
        const wake = next.state === "sleeping"
            ? i18nc("tooltip", "use Connect in the popup to wake it")
            : "";
        toolTipSubText = [ses, today, wake].filter(function (x) { return x; }).join("\n");
    }

    // POST/DELETE with the JSON media type the daemon requires (§6.1)
    function api(method, path, body) {
        if (root.busy) {
            return;
        }
        root.busy = true;
        var xhr = new XMLHttpRequest();
        xhr.open(method, apiBase + path);
        xhr.setRequestHeader("Content-Type", "application/json");
        if (root.token) {
            xhr.setRequestHeader("Authorization", "Bearer " + root.token);
        }
        xhr.onreadystatechange = function () {
            if (xhr.readyState !== XMLHttpRequest.DONE) {
                return;
            }
            root.busy = false;
            root.refresh();
        };
        xhr.send(body === undefined ? "{}" : body === null ? "" : JSON.stringify(body));
    }

    function toggleStartPause() {
        if (root.snap.state === "running") {
            root.api("POST", "/api/pause", {});
        } else {
            root.api("POST", "/api/start", {});
        }
    }

    function stop() {
        root.api("POST", "/api/stop", {});
    }

    function speedDelta(d) {
        root.api("POST", "/api/speed", { delta_kmh: d });
    }

    function setSpeed(v) {
        root.api("POST", "/api/speed", { speed_kmh: v });
    }

    function setTarget(distanceM, durationS) {
        const b = {};
        if (distanceM > 0) {
            b.distance_m = Math.round(distanceM);
        }
        if (durationS > 0) {
            b.duration_s = Math.round(durationS);
        }
        root.api("POST", "/api/target", b);
    }

    function clearTarget() {
        root.api("DELETE", "/api/target", null);
    }

    // wheel over the compact: ±0.1 km/h (§10)
    function wheelAdjust(up) {
        root.speedDelta(up ? 0.1 : -0.1);
    }

    // ------------------------------------------------------------ layout
    compactRepresentation: CompactRepresentation {}
    fullRepresentation: FullRepresentation {}
    preferredRepresentation: compactRepresentation

    // Notifications via the KF6 QML module; if that module (or the
    // executable DataEngine fallback) is unavailable the widget keeps
    // working and logs a single warning (§10).
    Loader {
        id: notificationLoader
        source: "NotificationBridge.qml"
    }
    Loader {
        id: execNotificationLoader
        active: notificationLoader.status === Loader.Error
        source: "ExecNotificationBridge.qml"
    }

    function notify(title, text) {
        const bridge = notificationLoader.item || execNotificationLoader.item;
        if (bridge) {
            bridge.notify(title, text);
        } else if (notificationLoader.status === Loader.Error) {
            console.warn("pitpat: no notification mechanism available");
            notificationLoader.active = false; // stop re-probing
            execNotificationLoader.active = false;
        }
    }

    Timer {
        id: pollTimer
        interval: root.pollInterval
        running: true
        repeat: true
        onTriggered: root.refresh()
        onIntervalChanged: restart()
    }

    Binding {
        target: pollTimer
        property: "interval"
        value: root.pollInterval
    }

    Component.onCompleted: root.refresh()
}
