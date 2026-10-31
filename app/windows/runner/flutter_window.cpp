#include "flutter_window.h"

#include <flutter/standard_method_codec.h>

#include <optional>
#include <string>
#include <variant>

#include "flutter/generated_plugin_registrant.h"

FlutterWindow::FlutterWindow(const flutter::DartProject& project)
    : project_(project) {}

FlutterWindow::~FlutterWindow() {}

bool FlutterWindow::OnCreate() {
  if (!Win32Window::OnCreate()) {
    return false;
  }

  RECT frame = GetClientArea();

  // The size here must match the window dimensions to avoid unnecessary surface
  // creation / destruction in the startup path.
  flutter_controller_ = std::make_unique<flutter::FlutterViewController>(
      frame.right - frame.left, frame.bottom - frame.top, project_);
  // Ensure that basic setup of the controller was successful.
  if (!flutter_controller_->engine() || !flutter_controller_->view()) {
    return false;
  }
  RegisterPlugins(flutter_controller_->engine());
  SetChildContent(flutter_controller_->view()->GetNativeWindow());

  // The same method names the GTK runner serves (linux/runner/
  // my_application.cc). The page itself covers the window already; this
  // channel is what makes the fullscreen button take the WINDOW out of
  // the desktop, not just draw a page over it.
  window_channel_ =
      std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
          flutter_controller_->engine()->messenger(), "parley/window",
          &flutter::StandardMethodCodec::GetInstance());
  window_channel_->SetMethodCallHandler(
      [this](const flutter::MethodCall<flutter::EncodableValue>& call,
             std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>>
                 result) {
        const std::string& method = call.method_name();
        HWND hwnd = GetHandle();
        if (method == "setFullscreen") {
          bool on = false;
          if (const auto* value =
                  std::get_if<bool>(call.arguments())) {
            on = *value;
          }
          SetFullscreen(on);
          result->Success(flutter::EncodableValue(fullscreen_));
        } else if (method == "minimize") {
          ShowWindow(hwnd, SW_MINIMIZE);
          result->Success(flutter::EncodableValue(true));
        } else if (method == "toggleMaximize") {
          if (IsZoomed(hwnd)) {
            ShowWindow(hwnd, SW_RESTORE);
          } else {
            ShowWindow(hwnd, SW_MAXIMIZE);
          }
          result->Success(flutter::EncodableValue(IsZoomed(hwnd) != 0));
        } else if (method == "close") {
          PostMessage(hwnd, WM_CLOSE, 0, 0);
          result->Success(flutter::EncodableValue(true));
        } else if (method == "startDrag") {
          // The native frame already drags. The custom title bar (the
          // caller of this) is Linux-only.
          result->Success(flutter::EncodableValue(false));
        } else {
          result->NotImplemented();
        }
      });

  flutter_controller_->engine()->SetNextFrameCallback([&]() {
    this->Show();
  });

  // Flutter can complete the first frame before the "show window" callback is
  // registered. The following call ensures a frame is pending to ensure the
  // window is shown. It is a no-op if the first frame hasn't completed yet.
  flutter_controller_->ForceRedraw();

  return true;
}

void FlutterWindow::OnDestroy() {
  // The channel holds a pointer into the engine's messenger: drop it
  // before the controller that owns the engine.
  window_channel_.reset();
  if (flutter_controller_) {
    flutter_controller_ = nullptr;
  }

  Win32Window::OnDestroy();
}

LRESULT
FlutterWindow::MessageHandler(HWND hwnd, UINT const message,
                              WPARAM const wparam,
                              LPARAM const lparam) noexcept {
  // Give Flutter, including plugins, an opportunity to handle window messages.
  if (flutter_controller_) {
    std::optional<LRESULT> result =
        flutter_controller_->HandleTopLevelWindowProc(hwnd, message, wparam,
                                                      lparam);
    if (result) {
      return *result;
    }
  }

  switch (message) {
    case WM_FONTCHANGE:
      flutter_controller_->engine()->ReloadSystemFonts();
      break;
  }

  return Win32Window::MessageHandler(hwnd, message, wparam, lparam);
}

void FlutterWindow::SetFullscreen(bool on) {
  HWND hwnd = GetHandle();
  if (!hwnd || on == fullscreen_) {
    return;
  }
  if (on) {
    saved_placement_.length = sizeof(saved_placement_);
    if (!GetWindowPlacement(hwnd, &saved_placement_)) {
      return;
    }
    saved_style_ = GetWindowLongPtr(hwnd, GWL_STYLE);
    if (!GetWindowRect(hwnd, &saved_rect_)) {
      return;
    }
    MONITORINFO monitor = {};
    monitor.cbSize = sizeof(monitor);
    if (!GetMonitorInfo(MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY),
                        &monitor)) {
      return;
    }
    // Drop the frame and cover the monitor's FULL bounds (a maximized
    // window stops at the work area, leaving the taskbar visible).
    SetWindowLongPtr(hwnd, GWL_STYLE, saved_style_ & ~WS_OVERLAPPEDWINDOW);
    SetWindowPos(hwnd, HWND_TOP, monitor.rcMonitor.left,
                 monitor.rcMonitor.top,
                 monitor.rcMonitor.right - monitor.rcMonitor.left,
                 monitor.rcMonitor.bottom - monitor.rcMonitor.top,
                 SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
    fullscreen_ = true;
  } else {
    SetWindowLongPtr(hwnd, GWL_STYLE, saved_style_);
    SetWindowPos(hwnd, nullptr, saved_rect_.left, saved_rect_.top,
                 saved_rect_.right - saved_rect_.left,
                 saved_rect_.bottom - saved_rect_.top,
                 SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
    // Placement last: it restores the original show state (maximized
    // stays maximized), which a bare SetWindowPos would lose.
    SetWindowPlacement(hwnd, &saved_placement_);
    fullscreen_ = false;
  }
}
