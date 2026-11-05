// The floating pill, ported from the original WispX overlay (windows/src/UI/Overlay.xaml.cs).
// Every state and level here comes from the running daemon; nothing is simulated.
import QtQuick
import QtQuick.Window

Window {
    id: host
    objectName: "overlay"
    title: "WispX pill"
    width: 500
    height: 120
    color: "transparent"
    flags: Qt.FramelessWindowHint | Qt.WindowDoesNotAcceptFocus | Qt.WindowTransparentForInput
    visible: mapped

    readonly property string mode: bridge.overlay.mode ?? "idle"
    readonly property string message: bridge.overlay.message ?? ""
    readonly property string kind: bridge.overlay.kind ?? "info"
    readonly property bool idlePill: bridge.overlay.idle_pill ?? false
    readonly property bool wanted: bridge.overlayAvailable
        && (mode !== "idle" || message !== "" || idlePill)
    readonly property bool busy: mode !== "idle" || message !== ""

    property bool mapped: false
    property bool rendering: false
    property string shownMode: "idle"
    property string shownMessage: ""

    // Frame state, as in the original's OnFrame.
    readonly property int barCount: 15
    readonly property real maxHeight: 24
    readonly property real minHeight: 3
    property real clock: 0
    property real smooth: 0
    property var seeds: []
    property var heights: []

    onModeChanged: apply()
    onMessageChanged: apply()
    onIdlePillChanged: apply()
    onWantedChanged: updateMapped()
    Component.onCompleted: {
        var seeds = [], heights = []
        for (var i = 0; i < barCount; ++i) {
            seeds.push(Math.random() * 2 * Math.PI)
            heights.push(minHeight)
        }
        host.seeds = seeds
        host.heights = heights
        updateMapped()
    }

    // A change of layer arrives with the window's next frame, so this makes one.
    // The pixel is in an empty corner, and too faint to see.
    function drawOneFrame() {
        frameNudge.opacity = frameNudge.opacity === 1 ? 0.5 : 1
    }

    Rectangle {
        id: frameNudge
        width: 1
        height: 1
        color: "#01000000"
    }

    function updateMapped() {
        if (wanted) {
            unmapTimer.stop()
            mapped = true
        } else {
            unmapTimer.restart()
        }
    }

    function apply() {
        var previous = shownMode
        var hadMessage = shownMessage !== ""
        shownMode = mode
        shownMessage = message
        settleTimer.stop()
        doneTimer.stop()

        // From these two directly: a binding on them may not have updated yet.
        if (mode !== "idle" || message !== "") {
            rendering = true
        }
        lock.opacity = mode === "locked" ? 1 : 0
        recDot.running = mode === "locked"
        bars.opacity = (mode === "recording" || mode === "locked" || mode === "transcribing") ? 1 : 0
        if (message !== "" && mode === "idle") {
            bars.opacity = 0
            glow.fadeTo(0)
            messageText.text = message
            // Set here rather than bound, so the width is right for this message.
            metrics.text = message
            morph(Math.min(470, metrics.advanceWidth + 42), 40, 1, 0.8)
            messageText.show()
            return
        }
        messageText.hide()
        if (mode === "recording") {
            glow.fadeTo(0)
            morph(156, 38, 1, 1.0)
            pop(1.06, 160)
        } else if (mode === "locked") {
            glow.fadeTo(0)
            morph(176, 38, 1, 1.0)
            pop(1.04, 160)
        } else if (mode === "transcribing") {
            glow.fadeTo(1)
            morph(120, 38, 1, 0)
        } else if (previous === "transcribing" && !hadMessage) {
            // Done, and the text has arrived: a small pop says so.
            glow.fadeTo(0)
            pop(1.1, 140)
            doneTimer.restart()
        } else {
            rest()
        }
    }

    function rest() {
        glow.fadeTo(0)
        morph(44, 8, idlePill ? 0.55 : 0, 0)
        settleTimer.restart()
    }

    function morph(width, height, opacity, overshoot) {
        morphing.stop()
        morphWidth.to = width
        morphHeight.to = height
        morphOpacity.to = opacity
        var duration = overshoot > 0 ? 380 : 300
        var curve = overshoot > 0 ? Easing.OutBack : Easing.OutCubic
        morphWidth.duration = duration
        morphHeight.duration = duration
        morphWidth.easing.type = curve
        morphHeight.easing.type = curve
        morphWidth.easing.overshoot = overshoot
        morphHeight.easing.overshoot = overshoot
        morphing.start()
    }

    function pop(peak, duration) {
        popping.stop()
        popUp.to = peak
        popUp.duration = duration * 0.45
        popDown.duration = duration * 1.6
        popping.start()
    }

    function frame(step) {
        var dt = Math.min(0.05, step)
        clock += dt
        var level = (mode === "recording" || mode === "locked") ? bridge.level : 0
        var k = level > smooth ? 22 : 9
        smooth += (level - smooth) * Math.min(1, dt * k)

        var mid = (barCount - 1) / 2
        for (var i = 0; i < barCount; ++i) {
            var center = 1 - Math.abs(i - mid) / mid
            var target, alpha
            if (mode === "recording" || mode === "locked") {
                var envelope = 0.28 + 0.72 * Math.pow(center, 0.9)
                var wobble = 0.55 + 0.45 * Math.sin(clock * (5.5 + (i % 4) * 1.3) + seeds[i])
                var breathe = 1.2 * (0.5 + 0.5 * Math.sin(clock * 2.4 + i * 0.5))
                target = minHeight + breathe + (maxHeight - minHeight) * smooth * envelope * wobble
                alpha = 0.55 + 0.45 * Math.min(1, smooth * 2 + center * 0.4)
            } else if (mode === "transcribing") {
                var wave = 0.5 + 0.5 * Math.sin(clock * 7.5 - i * 0.62)
                target = minHeight + 7 * wave * wave
                alpha = 0.35 + 0.65 * wave
            } else {
                target = minHeight
                alpha = 0.6
            }
            heights[i] += (target - heights[i]) * Math.min(1, dt * 18)
            var bar = barRepeater.itemAt(i)
            if (bar) {
                bar.height = heights[i]
                bar.y = (bars.height - heights[i]) / 2
                bar.opacity = alpha
            }
        }
    }

    FrameAnimation {
        running: host.rendering && host.visible
        onTriggered: host.frame(frameTime)
    }

    Timer {
        id: doneTimer
        interval: 170
        onTriggered: host.rest()
    }

    // Stops drawing once it's at rest, so an idle pill costs nothing.
    Timer {
        id: settleTimer
        interval: 450
        onTriggered: host.rendering = false
    }

    Timer {
        id: unmapTimer
        interval: 320
        onTriggered: if (!host.wanted) host.mapped = false
    }

    ParallelAnimation {
        id: morphing
        NumberAnimation { id: morphWidth; target: pill; property: "width" }
        NumberAnimation { id: morphHeight; target: pill; property: "height" }
        NumberAnimation {
            id: morphOpacity
            target: pill
            property: "opacity"
            duration: 220
            easing.type: Easing.OutQuad
        }
    }

    SequentialAnimation {
        id: popping
        NumberAnimation { id: popUp; target: pill; property: "scale"; easing.type: Easing.OutQuad }
        NumberAnimation {
            id: popDown
            target: pill
            property: "scale"
            to: 1
            easing.type: Easing.OutBack
            easing.overshoot: 0.8
        }
    }

    TextMetrics {
        id: metrics
        font: messageText.font
    }

    Item {
        id: pill
        objectName: "pill"
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 12
        width: 44
        height: 8
        opacity: 0

        // Soft shadow from plain shapes, which every renderer can draw. It fades out
        // before the window's edge, 12 px below the pill.
        Repeater {
            model: 10
            Rectangle {
                required property int index
                readonly property real spread: index + 1
                x: -spread
                y: -spread + 2
                width: pill.width + spread * 2
                height: pill.height + spread * 2
                radius: 20 + spread
                color: "#000000"
                opacity: host.busy ? 0.035 : 0.012
            }
        }

        Rectangle {
            id: surface
            anchors.fill: parent
            radius: 20
            color: "#F20E0E10"
            border.color: "#33FFFFFF"
            border.width: 1
        }

        // The band of light that travels while transcribing.
        Rectangle {
            id: glow
            anchors.fill: parent
            anchors.margins: 1
            radius: 19
            opacity: 0
            visible: opacity > 0

            property real sweep: -1
            readonly property real band: sweep + 0.5
            function shine(at) {
                return Qt.rgba(1, 1, 1, Math.max(0, 1 - Math.abs(at - band) / 0.5) * 0x22 / 255)
            }
            function fadeTo(value) {
                fading.stop()
                fading.to = value
                fading.start()
            }

            gradient: Gradient {
                orientation: Gradient.Horizontal
                GradientStop { position: 0; color: glow.shine(0) }
                GradientStop {
                    position: Math.max(0.001, Math.min(0.999, glow.band))
                    color: glow.shine(Math.max(0.001, Math.min(0.999, glow.band)))
                }
                GradientStop { position: 1; color: glow.shine(1) }
            }

            NumberAnimation { id: fading; target: glow; property: "opacity"; duration: 300 }
            NumberAnimation on sweep {
                running: glow.visible
                loops: Animation.Infinite
                from: -1
                to: 1
                duration: 1100
                easing.type: Easing.InOutSine
            }
        }

        Item {
            id: bars
            anchors.centerIn: parent
            width: 87
            height: 26
            opacity: 0
            visible: opacity > 0
            Behavior on opacity { NumberAnimation { duration: 180 } }

            Repeater {
                id: barRepeater
                model: host.barCount
                Rectangle {
                    required property int index
                    objectName: "bar-" + index
                    x: index * 6
                    y: 11.5
                    width: 3
                    height: 3
                    radius: 1.5
                    color: "white"
                }
            }
        }

        Item {
            id: lock
            objectName: "lock"
            anchors.left: parent.left
            anchors.leftMargin: 14
            anchors.verticalCenter: parent.verticalCenter
            width: 11
            height: 13
            opacity: 0
            visible: opacity > 0
            Behavior on opacity { NumberAnimation { duration: 180 } }

            Rectangle {
                x: 2
                y: 0
                width: 7
                height: 8
                radius: 3.5
                color: "transparent"
                border.color: "#D9FFFFFF"
                border.width: 1.5
            }
            Rectangle {
                x: 0
                y: 5
                width: 11
                height: 8
                radius: 2
                color: "#D9FFFFFF"
            }
        }

        Rectangle {
            id: recDot
            objectName: "rec-dot"
            property bool running: false
            anchors.right: parent.right
            anchors.rightMargin: 15
            anchors.verticalCenter: parent.verticalCenter
            width: 6
            height: 6
            radius: 3
            color: "#FF5A5F"
            visible: running
            SequentialAnimation on opacity {
                running: recDot.running
                loops: Animation.Infinite
                PauseAnimation { duration: 200 }
                NumberAnimation { from: 1; to: 0.25; duration: 700; easing.type: Easing.InOutSine }
                NumberAnimation { from: 0.25; to: 1; duration: 700; easing.type: Easing.InOutSine }
            }
        }

        Text {
            id: messageText
            objectName: "message"
            anchors.fill: parent
            anchors.leftMargin: 18
            anchors.rightMargin: 18
            verticalAlignment: Text.AlignVCenter
            horizontalAlignment: Text.AlignHCenter
            elide: Text.ElideRight
            font.pixelSize: 15
            font.weight: Font.DemiBold
            opacity: 0
            visible: opacity > 0
            color: host.kind === "error" ? "#FFA8A8"
                : host.kind === "warn" ? "#FFDE9E"
                : host.kind === "success" ? "#A8F0C0"
                : "white"

            function show() {
                textHide.stop()
                textShow.restart()
            }
            function hide() {
                textShow.stop()
                textHide.restart()
            }

            SequentialAnimation {
                id: textShow
                PauseAnimation { duration: 90 }
                NumberAnimation { target: messageText; property: "opacity"; to: 1; duration: 220 }
            }
            NumberAnimation { id: textHide; target: messageText; property: "opacity"; to: 0; duration: 120 }
        }
    }
}
