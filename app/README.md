# Parley desktop (Linux)

The Flutter UI. It owns presentation only: connection, voice, video and
screen capture live in Rust behind `flutter_rust_bridge`.

```sh
# From the repository root:
cargo build --release -p parley_flutter \
    --features audio-device,aec,video,screen-capture,hw-encode

cd app
flutter pub get
flutter run --release -d linux
```

The bridge must match the Flutter build mode: the runner installs the
cargo `DEBUG` cdylib for a debug Flutter build and the `release` one
otherwise. Mixing them bundles the wrong library.

Useful checks:

```sh
flutter analyze
flutter test
flutter build linux --release --dart-define=PARLEY_VERSION=dev
```

API changes: run `flutter_rust_bridge_codegen generate` after editing
`rust/src/api/` and commit the generated Dart. The bridge surface takes
plain types only — opaque Rust types in a signature break the generated
code (the 7d lesson).

Source layout:

```
lib/main.dart          bootstrap (bridge init, engine handle, runApp)
lib/src/app.dart       MaterialApp + theme
lib/src/home_page.dart session state machine and every session view
lib/src/window.dart    GTK window channel + the top hover toolbar
lib/src/theme.dart     palette + ThemeData
lib/src/widgets/       leaf widgets (login, fullscreen viewer, pill)
rust/src/api/          the exposed bridge functions
linux/runner/          GTK window setup (no title bar; channel methods)
```

`compose`/deploy/client documentation lives in [../docs](../docs).
