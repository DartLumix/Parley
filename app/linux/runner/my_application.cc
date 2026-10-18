#include "my_application.h"

#include <flutter_linux/flutter_linux.h>

#include "flutter/generated_plugin_registrant.h"

struct _MyApplication {
  GtkApplication parent_instance;
  char** dart_entrypoint_arguments;
};

G_DEFINE_TYPE(MyApplication, my_application, GTK_TYPE_APPLICATION)

// Called when first Flutter frame received.
static void first_frame_cb(MyApplication* self, FlView* view) {
  gtk_widget_show(gtk_widget_get_toplevel(GTK_WIDGET(view)));
}

// Window control channel ("parley/window", Dart side app/lib/main.dart).
// The share viewer asks for REAL fullscreen here: a widget can fill the
// window, but only the window manager can take the window borderless and
// over the panel — which is what a fullscreen screen share means.
static FlMethodChannel* window_channel = nullptr;
/// Last fullscreen state we told Dart about (edge detection: the state
/// event fires for every window-state change, focus included).
static gboolean window_is_fullscreen = FALSE;

// The desktop can leave fullscreen on its own — its own shortcut, its
// own exit hint. Tell Dart so the viewer PAGE goes away with the window:
// staying in a fullscreen viewer inside a decorated window is worse than
// either state on its own.
static gboolean window_state_event_cb(GtkWidget* widget, GdkEvent* event,
                                      gpointer user_data) {
  GdkEventWindowState* state = (GdkEventWindowState*)event;
  gboolean is_fullscreen =
      (state->new_window_state & GDK_WINDOW_STATE_FULLSCREEN) != 0;
  if (is_fullscreen != window_is_fullscreen) {
    window_is_fullscreen = is_fullscreen;
    if (window_channel != nullptr) {
      g_autoptr(FlValue) value = fl_value_new_bool(is_fullscreen);
      fl_method_channel_invoke_method(window_channel, "fullscreenChanged", value,
                                      nullptr, nullptr, nullptr);
    }
  }
  return FALSE;
}

static void window_method_call_cb(FlMethodChannel* channel,
                                  FlMethodCall* method_call,
                                  gpointer user_data) {
  GtkWindow* window = GTK_WINDOW(user_data);
  const gchar* method = fl_method_call_get_name(method_call);
  g_autoptr(FlMethodResponse) response = nullptr;
  if (g_strcmp0(method, "setFullscreen") == 0) {
    FlValue* args = fl_method_call_get_args(method_call);
    gboolean on = TRUE;
    if (args != nullptr && fl_value_get_type(args) == FL_VALUE_TYPE_BOOL) {
      on = fl_value_get_bool(args);
    }
    // The native call the compositor understands (xdg_toplevel
    // set_fullscreen on Wayland, _NET_WM_STATE_FULLSCREEN on X11) —
    // this is what makes the desktop itself treat the window as
    // fullscreen and offer its own exit hint.
    if (on) {
      gtk_window_fullscreen(window);
    } else {
      gtk_window_unfullscreen(window);
    }
    // Read the state back so the Dart side can SEE whether the
    // compositor took it — "the fullscreen button does nothing" is
    // otherwise indistinguishable from "the compositor ignored it".
    GdkWindow* gdk_window = gtk_widget_get_window(GTK_WIDGET(window));
    gboolean is_fullscreen =
        gdk_window != nullptr &&
        (gdk_window_get_state(gdk_window) & GDK_WINDOW_STATE_FULLSCREEN) != 0;
    g_message("window: fullscreen %s (state now %s)", on ? "on" : "off",
              is_fullscreen ? "fullscreen" : "not fullscreen");
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(fl_value_new_bool(is_fullscreen)));
  } else if (g_strcmp0(method, "minimize") == 0) {
    gtk_window_iconify(window);
    g_message("window: minimize");
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(fl_value_new_bool(TRUE)));
  } else if (g_strcmp0(method, "toggleMaximize") == 0) {
    if (gtk_window_is_maximized(window)) {
      gtk_window_unmaximize(window);
    } else {
      gtk_window_maximize(window);
    }
    gboolean maximized = gtk_window_is_maximized(window);
    g_message("window: maximize %s", maximized ? "on" : "off");
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(fl_value_new_bool(maximized)));
  } else if (g_strcmp0(method, "close") == 0) {
    g_message("window: close requested");
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(fl_value_new_bool(TRUE)));
    // Respond before closing: a destroyed channel must not lose the
    // reply the Dart side is awaiting.
    fl_method_call_respond(method_call, response, nullptr);
    gtk_window_close(window);
    return;
  } else if (g_strcmp0(method, "startDrag") == 0) {
    // Best-effort window move. On X11 this is the real _NET_WM_MOVERESIZE
    // path; on Wayland GTK needs the seat's last button-press serial,
    // which Flutter owns, so the compositor may ignore it. Tiling
    // compositors (Hyprland, sway) move windows with their own binding
    // anyway — the toolbar's controls remain the guaranteed part.
    GdkDisplay* display = gtk_widget_get_display(GTK_WIDGET(window));
    GdkSeat* seat = display != nullptr ? gdk_display_get_default_seat(display) : nullptr;
    GdkDevice* pointer = seat != nullptr ? gdk_seat_get_pointer(seat) : nullptr;
    gboolean dispatched = FALSE;
    if (pointer != nullptr) {
      gint x = 0;
      gint y = 0;
      GdkScreen* screen = nullptr;
      gdk_device_get_position(pointer, &screen, &x, &y);
      gtk_window_begin_move_drag(window, 1, x, y, gtk_get_current_event_time());
      dispatched = TRUE;
    }
    g_message("window: move drag %s",
              dispatched ? "dispatched" : "unavailable (no pointer)");
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(fl_value_new_bool(dispatched)));
  } else {
    response = FL_METHOD_RESPONSE(fl_method_not_implemented_response_new());
  }
  fl_method_call_respond(method_call, response, nullptr);
}

// Implements GApplication::activate.
static void my_application_activate(GApplication* application) {
  MyApplication* self = MY_APPLICATION(application);
  GtkWindow* window =
      GTK_WINDOW(gtk_application_window_new(GTK_APPLICATION(application)));

  // No title bar at all. Flutter's template installs a GtkHeaderBar on
  // Wayland, which is the big white strip with the app name and an X that
  // a tiling desktop shows above the page. The app draws its own thin
  // hover toolbar instead (app/lib/src/window.dart) and calls the window
  // channel for min/max/close, so the native window is the page itself.
  // gtk_window_set_title stays: taskbars and window rules still read it.
  gtk_window_set_title(window, "Parley");
  gtk_window_set_decorated(window, FALSE);
  g_message("window: decorations off (titlebar %s, decorated %s, app id %s)",
            gtk_window_get_titlebar(window) == nullptr ? "none" : "custom",
            gtk_window_get_decorated(window) ? "on" : "off", APPLICATION_ID);

  gtk_window_set_default_size(window, 1280, 720);

  g_autoptr(FlDartProject) project = fl_dart_project_new();
  fl_dart_project_set_dart_entrypoint_arguments(
      project, self->dart_entrypoint_arguments);

  FlView* view = fl_view_new(project);
  GdkRGBA background_color;
  // Background defaults to black, override it here if necessary, e.g. #00000000
  // for transparent.
  gdk_rgba_parse(&background_color, "#000000");
  fl_view_set_background_color(view, &background_color);
  gtk_widget_show(GTK_WIDGET(view));
  gtk_container_add(GTK_CONTAINER(window), GTK_WIDGET(view));

  // Show the window when Flutter renders.
  // Requires the view to be realized so we can start rendering.
  g_signal_connect_swapped(view, "first-frame", G_CALLBACK(first_frame_cb),
                           self);
  gtk_widget_realize(GTK_WIDGET(view));

  fl_register_plugins(FL_PLUGIN_REGISTRY(view));

  // One channel for the process (kept alive in the static above).
  g_autoptr(FlStandardMethodCodec) codec = fl_standard_method_codec_new();
  window_channel = fl_method_channel_new(
      fl_engine_get_binary_messenger(fl_view_get_engine(view)),
      "parley/window", FL_METHOD_CODEC(codec));
  fl_method_channel_set_method_call_handler(window_channel,
                                            window_method_call_cb, window,
                                            nullptr);
  g_signal_connect(window, "window-state-event",
                   G_CALLBACK(window_state_event_cb), nullptr);

  gtk_widget_grab_focus(GTK_WIDGET(view));
}

// Implements GApplication::local_command_line.
static gboolean my_application_local_command_line(GApplication* application,
                                                  gchar*** arguments,
                                                  int* exit_status) {
  MyApplication* self = MY_APPLICATION(application);
  // Strip out the first argument as it is the binary name.
  self->dart_entrypoint_arguments = g_strdupv(*arguments + 1);

  g_autoptr(GError) error = nullptr;
  if (!g_application_register(application, nullptr, &error)) {
    g_warning("Failed to register: %s", error->message);
    *exit_status = 1;
    return TRUE;
  }

  g_application_activate(application);
  *exit_status = 0;

  return TRUE;
}

// Implements GApplication::startup.
static void my_application_startup(GApplication* application) {
  // MyApplication* self = MY_APPLICATION(object);

  // Perform any actions required at application startup.

  G_APPLICATION_CLASS(my_application_parent_class)->startup(application);
}

// Implements GApplication::shutdown.
static void my_application_shutdown(GApplication* application) {
  // MyApplication* self = MY_APPLICATION(object);

  // Perform any actions required at application shutdown.

  G_APPLICATION_CLASS(my_application_parent_class)->shutdown(application);
}

// Implements GObject::dispose.
static void my_application_dispose(GObject* object) {
  MyApplication* self = MY_APPLICATION(object);
  g_clear_pointer(&self->dart_entrypoint_arguments, g_strfreev);
  G_OBJECT_CLASS(my_application_parent_class)->dispose(object);
}

static void my_application_class_init(MyApplicationClass* klass) {
  G_APPLICATION_CLASS(klass)->activate = my_application_activate;
  G_APPLICATION_CLASS(klass)->local_command_line =
      my_application_local_command_line;
  G_APPLICATION_CLASS(klass)->startup = my_application_startup;
  G_APPLICATION_CLASS(klass)->shutdown = my_application_shutdown;
  G_OBJECT_CLASS(klass)->dispose = my_application_dispose;
}

static void my_application_init(MyApplication* self) {}

MyApplication* my_application_new() {
  // Set the program name to the application ID, which helps various systems
  // like GTK and desktop environments map this running application to its
  // corresponding .desktop file. This ensures better integration by allowing
  // the application to be recognized beyond its binary name.
  g_set_prgname(APPLICATION_ID);

  return MY_APPLICATION(g_object_new(my_application_get_type(),
                                     "application-id", APPLICATION_ID, "flags",
                                     G_APPLICATION_NON_UNIQUE, nullptr));
}
