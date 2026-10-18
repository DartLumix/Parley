import 'package:flutter/material.dart';

/// Accent: a dark electric blue. Bright enough to read on black, dark
/// enough to stay electric rather than neon.
const accent = Color(0xFF1E6BFF);

/// The same hue lifted for icons, dots and names on black.
const accentSoft = Color(0xFF4D93FF);

/// App-bar surface: black with a blue cast, so the accent reads as the
/// app's colour rather than a pale title bar.
const barSurface = Color(0xFF0B1424);

/// One theme for the whole app. Widgets read the accent through the
/// scheme, not through the constants, so a future re-skin touches this
/// file only.
ThemeData buildParleyTheme() {
  return ThemeData(
    brightness: Brightness.dark,
    scaffoldBackgroundColor: Colors.black,
    appBarTheme: const AppBarTheme(
      backgroundColor: barSurface,
      foregroundColor: Colors.white,
    ),
    colorScheme: ColorScheme.fromSeed(
      seedColor: accent,
      brightness: Brightness.dark,
    ).copyWith(primary: accent, secondary: accentSoft),
    inputDecorationTheme: const InputDecorationTheme(
      labelStyle: TextStyle(color: Colors.white70),
      enabledBorder: OutlineInputBorder(
        borderSide: BorderSide(color: Colors.white24),
      ),
      focusedBorder: OutlineInputBorder(
        borderSide: BorderSide(color: accentSoft),
      ),
    ),
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        backgroundColor: accent,
        foregroundColor: Colors.white,
        disabledBackgroundColor: Colors.grey.shade900,
        disabledForegroundColor: Colors.grey.shade600,
      ),
    ),
    dividerColor: Colors.white24,
    useMaterial3: true,
  );
}
