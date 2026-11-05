// Setup and settings. Everything here is shown from, and acted on by, the Rust
// controller: this file holds layout only.
import QtQuick
import QtQuick.Controls
import QtQuick.Dialogs
import QtQuick.Layouts

ApplicationWindow {
    id: win
    objectName: "settings"
    title: "WispX"
    width: 560
    height: 720
    minimumWidth: 440
    minimumHeight: 360

    readonly property var s: bridge.state

    function act(name, args) {
        bridge.action(name, args || {})
    }

    function indexOf(list, key, value) {
        if (!list) {
            return -1
        }
        for (var i = 0; i < list.length; ++i) {
            if (list[i][key] === value) {
                return i
            }
        }
        return -1
    }

    function levelColor(level) {
        switch (level) {
        case "ok": return "#2EA043"
        case "warn": return "#D29922"
        case "fail": return "#E5534B"
        case "busy": return "#58A6FF"
        default: return "#8B949E"
        }
    }

    function pressed(action, args) {
        if (action === "choose-model") {
            modelFile.open()
        } else {
            act(action, args)
        }
    }

    Component.onCompleted: act("settings-opened")

    FileDialog {
        id: modelFile
        title: "Choose a whisper.cpp model"
        nameFilters: ["Whisper models (*.bin)", "All files (*)"]
        onAccepted: {
            var path = decodeURIComponent(selectedFile.toString().replace(/^file:\/\//, ""))
            win.act("set-model", { path: path })
        }
    }

    Dialog {
        id: chatgptConfirm
        objectName: "chatgpt-confirm"
        parent: Overlay.overlay
        anchors.centerIn: parent
        width: Math.min(420, win.width - 48)
        modal: true
        title: "Send your dictation to ChatGPT?"
        footer: DialogButtonBox {
            Button {
                objectName: "chatgpt-decline"
                text: "Stay on this computer"
                DialogButtonBox.buttonRole: DialogButtonBox.RejectRole
            }
            Button {
                objectName: "chatgpt-accept"
                text: "Use ChatGPT"
                DialogButtonBox.buttonRole: DialogButtonBox.AcceptRole
            }
        }
        onAccepted: win.act("set-backend", { backend: "chatgpt" })
        // However it closed, the box goes back to showing what the daemon says.
        onClosed: backendBox.currentIndex = Qt.binding(() => win.s.backend === "chatgpt" ? 1 : 0)
        contentItem: Label {
            wrapMode: Text.WordWrap
            text: "Your recorded audio will go to chatgpt.com using your Codex login, "
                + "through a private endpoint that can change without notice. "
                + "On this computer, nothing leaves your machine."
        }
    }

    ScrollView {
        id: scroller
        anchors.fill: parent
        contentWidth: availableWidth

        ColumnLayout {
            width: scroller.availableWidth
            spacing: 16

            // Header
            RowLayout {
                Layout.fillWidth: true
                Layout.topMargin: 20
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                spacing: 12
                Rectangle {
                    objectName: "summary-dot"
                    Layout.alignment: Qt.AlignVCenter
                    width: 12
                    height: 12
                    radius: 6
                    color: win.levelColor(win.s.summary ? win.s.summary.level : "wait")
                }
                Label {
                    objectName: "summary"
                    Layout.fillWidth: true
                    text: win.s.summary ? win.s.summary.text : "Getting ready…"
                    font.pixelSize: 18
                    font.weight: Font.DemiBold
                    wrapMode: Text.WordWrap
                }
            }

            // Errors worth reading stay here until the next thing goes right.
            Label {
                objectName: "problem"
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                visible: text !== ""
                text: win.s.problem ?? win.s.last_error ?? ""
                color: "#E5534B"
                wrapMode: Text.WordWrap
            }

            // How to use, with the keys that are really bound.
            GroupBox {
                objectName: "how-to-use"
                title: "How to use"
                padding: 12
                topPadding: 12 + implicitLabelHeight + 6
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                GridLayout {
                    anchors.left: parent.left
                    anchors.right: parent.right
                    columns: 2
                    columnSpacing: 16
                    rowSpacing: 6
                    Repeater {
                        model: win.s.shortcuts ?? []
                        delegate: Label {
                            required property var modelData
                            required property int index
                            Layout.row: index
                            Layout.column: 0
                            Layout.fillWidth: true
                            wrapMode: Text.WordWrap
                            text: modelData.label
                        }
                    }
                    Repeater {
                        model: win.s.shortcuts ?? []
                        delegate: Label {
                            required property var modelData
                            required property int index
                            objectName: "keys-" + index
                            Layout.row: index
                            Layout.column: 1
                            text: modelData.keys
                            font.weight: Font.DemiBold
                        }
                    }
                }
            }

            // Readiness
            ColumnLayout {
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                spacing: 12
                Repeater {
                    model: win.s.rows ?? []
                    delegate: RowLayout {
                        id: rowItem
                        required property var modelData
                        objectName: "row-" + modelData.id
                        Layout.fillWidth: true
                        spacing: 10
                        Rectangle {
                            Layout.alignment: Qt.AlignTop
                            Layout.topMargin: 5
                            width: 10
                            height: 10
                            radius: 5
                            color: win.levelColor(rowItem.modelData.level)
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 4
                            Label {
                                Layout.fillWidth: true
                                text: rowItem.modelData.title
                                font.weight: Font.DemiBold
                            }
                            Label {
                                objectName: "detail-" + rowItem.modelData.id
                                Layout.fillWidth: true
                                text: rowItem.modelData.detail
                                wrapMode: Text.WordWrap
                                opacity: 0.8
                            }
                            Flow {
                                Layout.fillWidth: true
                                Layout.topMargin: rowItem.modelData.buttons.length > 0 ? 4 : 0
                                spacing: 8
                                visible: rowItem.modelData.buttons.length > 0
                                Repeater {
                                    model: rowItem.modelData.buttons
                                    delegate: Button {
                                        required property var modelData
                                        objectName: "button-" + modelData.action
                                        text: modelData.label
                                        highlighted: modelData.primary
                                        enabled: modelData.enabled
                                        onClicked: win.pressed(modelData.action, modelData.args)
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Download under way
            RowLayout {
                objectName: "download"
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                visible: !!win.s.download
                spacing: 10
                ColumnLayout {
                    Layout.fillWidth: true
                    Label {
                        text: win.s.download
                            ? "Downloading " + win.s.download.label + " (" + win.s.download.percent + "%)"
                            : ""
                    }
                    ProgressBar {
                        Layout.fillWidth: true
                        from: 0
                        to: 100
                        value: win.s.download ? win.s.download.percent : 0
                    }
                }
                Button {
                    objectName: "button-cancel-download"
                    text: "Cancel"
                    onClicked: win.act("cancel-download")
                }
            }

            // Settings
            GroupBox {
                title: "Settings"
                padding: 12
                topPadding: 12 + implicitLabelHeight + 6
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                GridLayout {
                    anchors.left: parent.left
                    anchors.right: parent.right
                    columns: 2
                    columnSpacing: 12
                    rowSpacing: 8

                    Label { text: "Speech" }
                    ComboBox {
                        id: backendBox
                        objectName: "backend"
                        Layout.fillWidth: true
                        enabled: win.s.ready === true
                        textRole: "label"
                        model: [
                            { id: "local", label: "On this computer (whisper.cpp)" },
                            { id: "chatgpt", label: "ChatGPT (sends audio online)" }
                        ]
                        currentIndex: win.s.backend === "chatgpt" ? 1 : 0
                        onActivated: index => {
                            if (index === 1 && win.s.backend !== "chatgpt") {
                                chatgptConfirm.open()
                            } else if (index === 0 && win.s.backend !== "local") {
                                win.act("set-backend", { backend: "local" })
                            }
                        }
                    }

                    Label { text: "Model"; visible: modelRow.visible }
                    RowLayout {
                        id: modelRow
                        Layout.fillWidth: true
                        visible: win.s.backend !== "chatgpt"
                        spacing: 8
                        ComboBox {
                            objectName: "model"
                            Layout.fillWidth: true
                            enabled: win.s.ready === true
                            textRole: "label"
                            model: win.s.models ?? []
                            currentIndex: win.indexOf(win.s.models, "path", win.s.model)
                            onActivated: index => win.act("set-model", { path: win.s.models[index].path })
                        }
                        Button {
                            objectName: "choose-model"
                            text: "Choose…"
                            enabled: win.s.ready === true
                            onClicked: modelFile.open()
                        }
                    }

                    Label { text: "Get a model"; visible: downloadRow.visible }
                    RowLayout {
                        id: downloadRow
                        Layout.fillWidth: true
                        visible: win.s.backend !== "chatgpt"
                            && !win.s.download
                            && (win.s.downloads ?? []).length > 0
                        spacing: 8
                        ComboBox {
                            id: downloadBox
                            objectName: "download-choice"
                            Layout.fillWidth: true
                            textRole: "label"
                            model: win.s.downloads ?? []
                            enabled: win.s.ready === true
                        }
                        Button {
                            objectName: "download-start"
                            text: "Download"
                            enabled: win.s.ready === true
                            onClicked: win.act("download-model", { id: win.s.downloads[downloadBox.currentIndex].id })
                        }
                    }

                    Label { text: "Language" }
                    ComboBox {
                        objectName: "language"
                        Layout.fillWidth: true
                        enabled: win.s.ready === true
                        textRole: "label"
                        model: win.s.languages ?? []
                        currentIndex: win.indexOf(win.s.languages, "code", win.s.language)
                        onActivated: index => win.act("set-language", { code: win.s.languages[index].code })
                    }

                    Label { text: "Microphone" }
                    ComboBox {
                        objectName: "device"
                        Layout.fillWidth: true
                        enabled: win.s.ready === true
                        textRole: "label"
                        model: win.s.devices ?? []
                        currentIndex: Math.max(0, win.indexOf(win.s.devices, "id", win.s.device))
                        onActivated: index => win.act("set-device", { id: win.s.devices[index].id })
                    }

                    Label { text: "Sounds" }
                    Switch {
                        objectName: "sounds"
                        enabled: win.s.ready === true
                        checked: win.s.sounds === true
                        onToggled: win.act("set-sounds", { checked: checked })
                    }

                    Label { text: "Show idle pill" }
                    Switch {
                        objectName: "idle-pill"
                        enabled: win.s.ready === true
                        checked: win.s.idle_pill === true
                        onToggled: win.act("set-idle-pill", { checked: checked })
                    }

                    Label { text: "Start at login" }
                    Switch {
                        objectName: "autostart"
                        checked: win.s.autostart === true
                        onToggled: win.act("set-autostart", { checked: checked })
                    }
                }
            }

            // Somewhere safe for a first try.
            GroupBox {
                title: "Try it"
                padding: 12
                topPadding: 12 + implicitLabelHeight + 6
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                TextArea {
                    objectName: "try-it"
                    anchors.left: parent.left
                    anchors.right: parent.right
                    height: 72
                    wrapMode: TextEdit.Wrap
                    placeholderText: "Click here, then dictate into this box."
                }
            }

            Label {
                objectName: "tray-note"
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                visible: win.s.tray_available === false
                wrapMode: Text.WordWrap
                opacity: 0.8
                text: "No system tray was found, so run wispx again to come back to this window."
            }

            Label {
                objectName: "overlay-note"
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                visible: win.s.overlay_available === false
                wrapMode: Text.WordWrap
                opacity: 0.8
                text: "This desktop can't show the floating pill, so WispX uses notifications instead."
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.leftMargin: 24
                Layout.rightMargin: 24
                Layout.bottomMargin: 20
                spacing: 8
                Button {
                    objectName: "copy-last"
                    text: "Copy last transcript"
                    enabled: win.s.has_transcript === true
                    onClicked: win.act("copy-last")
                }
                Button {
                    objectName: "recheck"
                    text: "Check again"
                    onClicked: win.act("recheck")
                }
                Item { Layout.fillWidth: true }
                Button {
                    objectName: "quit"
                    text: "Quit WispX"
                    onClicked: win.act("quit")
                }
            }
        }
    }
}
