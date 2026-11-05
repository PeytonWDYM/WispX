// Qt interop for WispX, and only that: windows, the tray and the layer surface.
// Application logic lives in Rust (src/app), which calls in through bridge.h and
// hears back through the action callback.

#include "bridge.h"

#include <LayerShellQt/Window>

#include <QAction>
#include <QApplication>
#include <QByteArray>
#include <QGuiApplication>
#include <QIcon>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QMargins>
#include <QMenu>
#include <QPixmap>
#include <QPointer>
#include <QQmlComponent>
#include <QQmlContext>
#include <QQmlEngine>
#include <QQuickItem>
#include <QQuickWindow>
#include <QScreen>
#include <QSystemTrayIcon>
#include <QTimer>
#include <QVariantMap>

#include <wayland-client.h>

#include <cstdlib>
#include <cstring>
#include <future>
#include <memory>
#include <mutex>

namespace {

const int kIdleLayerDelayMs = 450;
const int kTrayPollMs = 5000;
const int kInspectTimeoutMs = 2000;

QVariantMap parseObject(const char *json)
{
    return QJsonDocument::fromJson(QByteArray(json)).object().toVariantMap();
}

// Whether the compositor can place a surface itself. Asking on a separate
// connection keeps this from touching anything Qt is doing on its own.
bool compositorHasLayerShell()
{
    struct Found {
        bool yes = false;
    } found;
    wl_display *display = wl_display_connect(nullptr);
    if (!display) {
        return false;
    }
    wl_registry *registry = wl_display_get_registry(display);
    static const wl_registry_listener listener = {
        [](void *data, wl_registry *, uint32_t, const char *interface, uint32_t) {
            if (std::strcmp(interface, "zwlr_layer_shell_v1") == 0) {
                static_cast<Found *>(data)->yes = true;
            }
        },
        [](void *, wl_registry *, uint32_t) {},
    };
    wl_registry_add_listener(registry, &listener, &found);
    wl_display_roundtrip(display);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return found.yes;
}

} // namespace

class Bridge : public QObject
{
    Q_OBJECT
    Q_PROPERTY(QVariantMap state READ state NOTIFY stateChanged)
    Q_PROPERTY(QVariantMap overlay READ overlay NOTIFY overlayChanged)
    Q_PROPERTY(double level READ level NOTIFY levelChanged)
    Q_PROPERTY(bool overlayAvailable READ overlayAvailable NOTIFY overlayAvailableChanged)

public:
    explicit Bridge(const wispx_ui_params &params)
        : m_params(params)
    {
        m_engine.rootContext()->setContextProperty(QStringLiteral("bridge"), this);
        QPixmap pixmap;
        pixmap.loadFromData(params.icon_png, static_cast<uint>(params.icon_png_len), "PNG");
        m_icon = QIcon(pixmap);
        QGuiApplication::setWindowIcon(m_icon);
    }

    QVariantMap state() const { return m_state; }
    QVariantMap overlay() const { return m_overlay; }
    double level() const { return m_level; }
    bool overlayAvailable() const { return m_overlayAvailable; }

    Q_INVOKABLE void action(const QString &name, const QVariantMap &args = {})
    {
        const QByteArray json =
            QJsonDocument(QJsonObject::fromVariantMap(args)).toJson(QJsonDocument::Compact);
        m_params.on_action(name.toUtf8().constData(), json.constData(), m_params.user);
    }

    void start()
    {
        createOverlay();
        createTray();
        m_params.on_ready(m_overlayAvailable, QSystemTrayIcon::isSystemTrayAvailable(),
                          m_params.user);
    }

    void setState(const QVariantMap &state)
    {
        m_state = state;
        emit stateChanged();
    }

    void setOverlay(const QVariantMap &overlay)
    {
        const bool wasActive = isActive(m_overlay);
        m_overlay = overlay;
        emit overlayChanged();
        const bool active = isActive(m_overlay);
        if (active != wasActive) {
            activeChanged(active);
        }
    }

    void setLevel(float level)
    {
        m_level = level;
        emit levelChanged();
    }

    void setTray(const QJsonObject &tray)
    {
        if (!m_tray) {
            return;
        }
        m_tray->setToolTip(tray.value(QStringLiteral("tooltip")).toString());
        const QJsonArray items = tray.value(QStringLiteral("items")).toArray();
        if (!menuMatches(items)) {
            buildMenu(items);
        }
        const QList<QAction *> actions = m_menu->actions();
        for (int i = 0; i < items.size(); ++i) {
            const QJsonObject item = items.at(i).toObject();
            QAction *action = actions.at(i);
            if (item.value(QStringLiteral("separator")).toBool()) {
                continue;
            }
            action->setText(item.value(QStringLiteral("label")).toString());
            action->setEnabled(item.value(QStringLiteral("enabled")).toBool());
            action->setChecked(item.value(QStringLiteral("checked")).toBool());
        }
    }

    void showSettings(const QByteArray &activationToken)
    {
        // A window that's open but behind others can't take focus by itself on
        // Wayland, and a new one can. Either way, the person sees it in front.
        if (m_settings) {
            if (m_settings->isActive()) {
                return;
            }
            QQuickWindow *old = m_settings;
            old->disconnect(this);
            m_settings = nullptr;
            old->close();
            old->deleteLater();
        }
        if (!activationToken.isEmpty()) {
            qputenv("XDG_ACTIVATION_TOKEN", activationToken);
        }
        QQmlComponent component(&m_engine);
        component.setData(QByteArray(m_params.settings_qml, qsizetype(m_params.settings_qml_len)),
                          QUrl(QStringLiteral("qrc:/wispx/Settings.qml")));
        QObject *object = component.create();
        m_settings = qobject_cast<QQuickWindow *>(object);
        if (!m_settings) {
            qWarning("WispX: cannot open the settings window: %s",
                     qPrintable(component.errorString()));
            delete object;
            return;
        }
        // It holds nothing that must survive, so it costs nothing while closed.
        connect(m_settings, &QWindow::visibleChanged, this, [this](bool visible) {
            if (!visible && m_settings) {
                m_settings->deleteLater();
                m_settings = nullptr;
            }
        });
        m_settings->show();
    }

    QByteArray inspect() const
    {
        QJsonObject windows;
        windows[QStringLiteral("platform")] = QGuiApplication::platformName();
        windows[QStringLiteral("overlay_available")] = m_overlayAvailable;

        QJsonObject overlay;
        if (m_overlayWindow) {
            overlay[QStringLiteral("visible")] = m_overlayWindow->isVisible();
            overlay[QStringLiteral("active")] = m_overlayWindow->isActive();
            overlay[QStringLiteral("width")] = m_overlayWindow->width();
            overlay[QStringLiteral("height")] = m_overlayWindow->height();
            overlay[QStringLiteral("flags")] = int(m_overlayWindow->flags());
            if (m_layer) {
                overlay[QStringLiteral("layer")] = int(m_layer->layer());
                overlay[QStringLiteral("keyboard")] = int(m_layer->keyboardInteractivity());
            }
            overlay[QStringLiteral("level")] = m_level;
            QJsonObject items;
            collectItems(m_overlayWindow->contentItem(), items);
            overlay[QStringLiteral("items")] = items;
        }
        windows[QStringLiteral("overlay")] = overlay;

        QJsonObject settings;
        settings[QStringLiteral("open")] = bool(m_settings);
        if (m_settings) {
            settings[QStringLiteral("active")] = m_settings->isActive();
            settings[QStringLiteral("width")] = m_settings->width();
            settings[QStringLiteral("height")] = m_settings->height();
            QJsonObject items;
            collectItems(m_settings->contentItem(), items);
            settings[QStringLiteral("items")] = items;
        }
        windows[QStringLiteral("settings")] = settings;

        windows[QStringLiteral("tray")] = QJsonObject{
            {QStringLiteral("available"), QSystemTrayIcon::isSystemTrayAvailable()},
            {QStringLiteral("visible"), m_tray && m_tray->isVisible()},
        };
        return QJsonDocument(windows).toJson(QJsonDocument::Compact);
    }

signals:
    void stateChanged();
    void overlayChanged();
    void levelChanged();
    void overlayAvailableChanged();

private:
    static bool isActive(const QVariantMap &overlay)
    {
        const QString mode = overlay.value(QStringLiteral("mode")).toString();
        return (!mode.isEmpty() && mode != QStringLiteral("idle"))
            || !overlay.value(QStringLiteral("message")).toString().isEmpty();
    }

    // Named objects with their place in the window, so tests can click real controls.
    static void collectItems(QQuickItem *item, QJsonObject &items)
    {
        const QString name = item->objectName();
        if (!name.isEmpty() && item->window()) {
            const QPointF at = item->mapToScene(QPointF(0, 0));
            items[name] = QJsonObject{
                {QStringLiteral("x"), at.x()},
                {QStringLiteral("y"), at.y()},
                {QStringLiteral("width"), item->width()},
                {QStringLiteral("height"), item->height()},
                {QStringLiteral("enabled"), item->isEnabled()},
                {QStringLiteral("visible"), item->isVisible()},
                {QStringLiteral("opacity"), item->opacity()},
            };
        }
        const QList<QQuickItem *> children = item->childItems();
        for (QQuickItem *child : children) {
            collectItems(child, items);
        }
    }

    void createOverlay()
    {
        const QString platform = QGuiApplication::platformName();
        const bool wayland = platform.startsWith(QStringLiteral("wayland"));
        if (wayland && !compositorHasLayerShell()) {
            // Without a layer surface the pill would be an ordinary window, in the
            // wrong place and in the taskbar. Popups say what it would have said.
            return;
        }
        if (!wayland && platform != QStringLiteral("xcb")) {
            return;
        }

        QQmlComponent component(&m_engine);
        component.setData(QByteArray(m_params.overlay_qml, qsizetype(m_params.overlay_qml_len)),
                          QUrl(QStringLiteral("qrc:/wispx/Overlay.qml")));
        QObject *object = component.create();
        m_overlayWindow = qobject_cast<QQuickWindow *>(object);
        if (!m_overlayWindow) {
            qWarning("WispX: cannot create the pill: %s", qPrintable(component.errorString()));
            delete object;
            return;
        }

        if (wayland) {
            // Must happen before the window has a surface, which is why QML waits
            // for overlayAvailable before showing it.
            m_layer = LayerShellQt::Window::get(m_overlayWindow);
            m_layer->setScope(QStringLiteral("wispx-pill"));
            m_layer->setAnchors(LayerShellQt::Window::Anchors(LayerShellQt::Window::AnchorBottom));
            // Zero keeps it inside the work area, clear of panels, without reserving any.
            m_layer->setExclusiveZone(0);
            m_layer->setMargins(QMargins());
            m_layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityNone);
            m_layer->setActivateOnShow(false);
            m_layer->setCloseOnDismissed(false);
            m_layer->setWantsToBeOnActiveScreen(true);
            m_layer->setLayer(LayerShellQt::Window::LayerTop);
        } else {
            m_overlayWindow->setFlags(m_overlayWindow->flags() | Qt::Tool
                                      | Qt::WindowStaysOnTopHint
                                      | Qt::BypassWindowManagerHint);
            placeOnX11();
        }

        m_idleLayer.setSingleShot(true);
        m_idleLayer.setInterval(kIdleLayerDelayMs);
        connect(&m_idleLayer, &QTimer::timeout, this, [this] {
            setLayer(LayerShellQt::Window::LayerTop);
        });

        m_overlayAvailable = true;
        emit overlayAvailableChanged();
    }

    void placeOnX11()
    {
        const QScreen *screen = QGuiApplication::primaryScreen();
        if (!screen || !m_overlayWindow) {
            return;
        }
        const QRect area = screen->availableGeometry();
        m_overlayWindow->setPosition(area.center().x() - m_overlayWindow->width() / 2,
                                     area.bottom() + 1 - m_overlayWindow->height());
    }

    // Above a fullscreen window only while it has something to say. At rest the
    // pill stays out of the way of a video or a game.
    void activeChanged(bool active)
    {
        if (!m_overlayWindow) {
            return;
        }
        if (!m_layer) {
            placeOnX11();
            return;
        }
        if (!active) {
            m_idleLayer.start();
            return;
        }
        m_idleLayer.stop();
        setLayer(LayerShellQt::Window::LayerOverlay);
        // A compositor picks the output when a surface maps, so to follow the
        // person to the screen they are using, the pill maps again.
        if (QGuiApplication::screens().size() > 1 && m_overlayWindow->isVisible()) {
            m_overlayWindow->hide();
            m_overlayWindow->show();
        }
    }

    void setLayer(LayerShellQt::Window::Layer layer)
    {
        if (m_layer && m_layer->layer() != layer) {
            m_layer->setLayer(layer);
            // It applies with the next frame, and a pill at rest draws none.
            if (m_overlayWindow) {
                QMetaObject::invokeMethod(m_overlayWindow, "drawOneFrame");
            }
        }
    }

    void createTray()
    {
        m_menu = std::make_unique<QMenu>();
        m_tray = std::make_unique<QSystemTrayIcon>(m_icon);
        m_tray->setContextMenu(m_menu.get());
        connect(m_tray.get(), &QSystemTrayIcon::activated, this,
                [this](QSystemTrayIcon::ActivationReason reason) {
                    if (reason == QSystemTrayIcon::Trigger) {
                        action(QStringLiteral("show-settings"));
                    }
                });
        m_tray->show();

        // A panel can arrive after login, or restart. Qt registers the icon by
        // itself when it does, so only the note in the window needs updating.
        m_trayWasAvailable = QSystemTrayIcon::isSystemTrayAvailable();
        m_trayPoll.setInterval(kTrayPollMs);
        connect(&m_trayPoll, &QTimer::timeout, this, [this] {
            const bool available = QSystemTrayIcon::isSystemTrayAvailable();
            if (available != m_trayWasAvailable) {
                m_trayWasAvailable = available;
                action(QStringLiteral("tray-status"),
                       QVariantMap{{QStringLiteral("available"), available}});
            }
        });
        m_trayPoll.start();
    }

    bool menuMatches(const QJsonArray &items) const
    {
        const QList<QAction *> actions = m_menu->actions();
        if (actions.size() != items.size()) {
            return false;
        }
        for (int i = 0; i < items.size(); ++i) {
            const QJsonObject item = items.at(i).toObject();
            if (actions.at(i)->isSeparator() != item.value(QStringLiteral("separator")).toBool()
                || actions.at(i)->isCheckable() != item.value(QStringLiteral("checkable")).toBool()
                || actions.at(i)->objectName() != item.value(QStringLiteral("id")).toString()) {
                return false;
            }
        }
        return true;
    }

    void buildMenu(const QJsonArray &items)
    {
        m_menu->clear();
        for (const QJsonValue &value : items) {
            const QJsonObject item = value.toObject();
            if (item.value(QStringLiteral("separator")).toBool()) {
                m_menu->addSeparator();
                continue;
            }
            const QString id = item.value(QStringLiteral("id")).toString();
            QAction *entry = m_menu->addAction(item.value(QStringLiteral("label")).toString());
            entry->setObjectName(id);
            entry->setCheckable(item.value(QStringLiteral("checkable")).toBool());
            connect(entry, &QAction::triggered, this, [this, id](bool checked) {
                action(id, QVariantMap{{QStringLiteral("checked"), checked}});
            });
        }
    }

    wispx_ui_params m_params;
    QQmlEngine m_engine;
    QIcon m_icon;
    QVariantMap m_state;
    QVariantMap m_overlay;
    double m_level = 0.0;
    bool m_overlayAvailable = false;
    bool m_trayWasAvailable = false;
    QPointer<QQuickWindow> m_overlayWindow;
    QPointer<QQuickWindow> m_settings;
    LayerShellQt::Window *m_layer = nullptr;
    std::unique_ptr<QMenu> m_menu;
    std::unique_ptr<QSystemTrayIcon> m_tray;
    QTimer m_idleLayer;
    QTimer m_trayPoll;
};

namespace {

// Calls can arrive from Rust's threads at any time, including while quitting.
// Keep the receiver alive until invokeMethod has finished queuing the call.
// Qt discards queued calls when their receiver is destroyed.
std::mutex g_bridgeMutex;
Bridge *g_bridge = nullptr;

template <typename Function>
void post(Function function)
{
    const std::lock_guard<std::mutex> guard(g_bridgeMutex);
    Bridge *bridge = g_bridge;
    if (bridge) {
        QMetaObject::invokeMethod(bridge, [bridge, function] { function(bridge); },
                                  Qt::QueuedConnection);
    }
}

} // namespace

extern "C" int wispx_ui_run(const wispx_ui_params *params)
{
    static int argc = 3;
    static char name[] = "wispx-gui";
    static char platformFlag[] = "-platform";
    // Wayland first. XCB is only for an X11 session, or a system missing the plugin.
    static char platform[] = "wayland;xcb";
    static char *argv[] = {name, platformFlag, platform, nullptr};
    if (qEnvironmentVariableIsEmpty("WAYLAND_DISPLAY")
        || !qEnvironmentVariableIsEmpty("QT_QPA_PLATFORM")) {
        argc = 1;
    }

    // Must come before anything is created, or the pill has an opaque background.
    QQuickWindow::setDefaultAlphaBuffer(true);
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("WispX"));
    QApplication::setApplicationDisplayName(QStringLiteral("WispX"));
    QApplication::setDesktopFileName(QString::fromUtf8(params->app_id));
    // The app lives in the tray, so closing its window doesn't end it.
    QApplication::setQuitOnLastWindowClosed(false);

    int result = 0;
    {
        Bridge bridge(*params);
        {
            const std::lock_guard<std::mutex> guard(g_bridgeMutex);
            g_bridge = &bridge;
        }
        QTimer::singleShot(0, &bridge, [&bridge] { bridge.start(); });
        result = QApplication::exec();
        {
            const std::lock_guard<std::mutex> guard(g_bridgeMutex);
            g_bridge = nullptr;
        }
    }
    return result;
}

extern "C" void wispx_ui_set_overlay(const char *json)
{
    const QVariantMap value = parseObject(json);
    post([value](Bridge *bridge) { bridge->setOverlay(value); });
}

extern "C" void wispx_ui_set_state(const char *json)
{
    const QVariantMap value = parseObject(json);
    post([value](Bridge *bridge) { bridge->setState(value); });
}

extern "C" void wispx_ui_set_tray(const char *json)
{
    const QJsonObject value = QJsonDocument::fromJson(QByteArray(json)).object();
    post([value](Bridge *bridge) { bridge->setTray(value); });
}

extern "C" void wispx_ui_set_level(float level)
{
    post([level](Bridge *bridge) { bridge->setLevel(level); });
}

extern "C" void wispx_ui_show_settings(const char *activation_token)
{
    const QByteArray token = activation_token ? QByteArray(activation_token) : QByteArray();
    post([token](Bridge *bridge) { bridge->showSettings(token); });
}

extern "C" void wispx_ui_quit(void)
{
    post([](Bridge *) { QApplication::quit(); });
}

extern "C" char *wispx_ui_inspect(void)
{
    auto promise = std::make_shared<std::promise<QByteArray>>();
    std::future<QByteArray> future = promise->get_future();
    post([promise](Bridge *bridge) { promise->set_value(bridge->inspect()); });
    QByteArray text("{}");
    if (future.wait_for(std::chrono::milliseconds(kInspectTimeoutMs)) == std::future_status::ready) {
        text = future.get();
    }
    return strdup(text.constData());
}

extern "C" void wispx_ui_free(char *text)
{
    std::free(text);
}

#include "bridge.moc"
