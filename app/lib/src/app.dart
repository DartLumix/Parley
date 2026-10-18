import 'package:flutter/material.dart';

import 'home_page.dart';
import 'theme.dart';

class ParleyApp extends StatelessWidget {
  const ParleyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Parley',
      debugShowCheckedModeBanner: false,
      theme: buildParleyTheme(),
      home: const HomePage(),
    );
  }
}
