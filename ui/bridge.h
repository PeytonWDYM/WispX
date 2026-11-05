// The boundary between WispX's Rust code and Qt. Rust decides everything; this
// side only draws. All functions but wispx_ui_run may be called from any thread.
#ifndef WISPX_BRIDGE_H
#define WISPX_BRIDGE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*wispx_action_fn)(const char *name, const char *args_json, void *user);
typedef void (*wispx_ready_fn)(int overlay_available, int tray_available, void *user);

typedef struct {
    const char *overlay_qml;
    size_t overlay_qml_len;
    const char *settings_qml;
    size_t settings_qml_len;
    const unsigned char *icon_png;
    size_t icon_png_len;
    const char *app_id;
    wispx_action_fn on_action;
    wispx_ready_fn on_ready;
    void *user;
} wispx_ui_params;

// Runs on the calling thread, which must be the main one, until wispx_ui_quit.
int wispx_ui_run(const wispx_ui_params *params);
void wispx_ui_set_overlay(const char *json);
void wispx_ui_set_state(const char *json);
void wispx_ui_set_tray(const char *json);
void wispx_ui_set_level(float level);
// `activation_token` may be null. It lets the desktop hand focus to the window.
void wispx_ui_show_settings(const char *activation_token);
void wispx_ui_quit(void);
// Returns JSON describing what is on screen. Free it with wispx_ui_free.
char *wispx_ui_inspect(void);
void wispx_ui_free(char *text);

#ifdef __cplusplus
}
#endif

#endif
