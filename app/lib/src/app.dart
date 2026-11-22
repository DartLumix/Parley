import 'package:flutter/material.dart';

import 'home_page.dart';
import 'report.dart';
import 'theme.dart';

class ParleyApp extends StatelessWidget {
  const ParleyApp({super.key});

  @override
  Widget build(BuildContext context) {
    // The root boundary is the screenshot source for Support → Report a
    // problem; it includes the open settings page and any dialog.
    return RepaintBoundary(
      key: reportBoundaryKey,
      child: MaterialApp(
        title: 'Parley',
        debugShowCheckedModeBanner: false,
        theme: buildParleyTheme(),
        home: const HomePage(),
      ),
    );
  }
}
