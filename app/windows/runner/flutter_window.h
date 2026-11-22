#ifndef RUNNER_FLUTTER_WINDOW_H_
#define RUNNER_FLUTTER_WINDOW_H_

#include <flutter/dart_project.h>
#include <flutter/encodable_value.h>
#include <flutter/flutter_view_controller.h>
#include <flutter/method_channel.h>

#include <memory>
#include <string>

#include "win32_window.h"
#include <shellapi.h>

// A window that does nothing but host a Flutter view.
class FlutterWindow : public Win32Window {
 public:
  // Creates a new FlutterWindow hosting a Flutter view running |project|.
  explicit FlutterWindow(const flutter::DartProject& project);
  virtual ~FlutterWindow();

 protected:
  // Win32Window:
  bool OnCreate() override;
  void OnDestroy() override;
  LRESULT MessageHandler(HWND window, UINT const message, WPARAM const wparam,
                         LPARAM const lparam) noexcept override;

 private:
  // The Dart side's `parley/window` channel (app/lib/src/window.dart).
  // Windows keeps its native frame, so only fullscreen and the native
  // controls are served here; the custom title bar stays Linux-only.
  std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>>
      window_channel_;

  // The Dart side's `parley/notifications` channel. A Win32 app without
  // an MSIX identity can only raise a notification-area balloon, which
  // needs a tray icon: one is added on the first poke notification and
  // kept (clicking it restores the window), then removed with the process.
  std::unique_ptr<flutter::MethodChannel<flutter::EncodableValue>>
      notifications_channel_;
  NOTIFYICONDATAW tray_icon_{};
  bool tray_icon_added_ = false;
  void ShowNotification(const std::wstring& title, const std::wstring& body);

  // Borderless fullscreen state, restored exactly on exit.
  bool fullscreen_ = false;
  LONG_PTR saved_style_ = 0;
  WINDOWPLACEMENT saved_placement_{};
  RECT saved_rect_{};
  void SetFullscreen(bool on);

  // The project to run.
  flutter::DartProject project_;

  // The Flutter instance hosted by this window.
  std::unique_ptr<flutter::FlutterViewController> flutter_controller_;
};

#endif  // RUNNER_FLUTTER_WINDOW_H_
