import 'package:flutter/material.dart';
import 'src/rust/frb_generated.dart';
import 'src/rust/api/simple.dart';

Future<void> main() async {
  await RustLib.init();
  runApp(const ParleyApp());
}

class ParleyApp extends StatelessWidget {
  const ParleyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Parley Dev',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.deepPurple),
        useMaterial3: true,
      ),
      home: const DevHomePage(),
    );
  }
}

class DevHomePage extends StatefulWidget {
  const DevHomePage({super.key});

  @override
  State<DevHomePage> createState() => _DevHomePageState();
}

class _DevHomePageState extends State<DevHomePage> {
  String _status = 'Bridge not tested';

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: const Text('Parley Dev Client'),
      ),
      body: Center(
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Text(_status),
            const SizedBox(height: 20),
            ElevatedButton(
              onPressed: () async {
                final result = await ping();
                setState(() => _status = 'Rust says: $result');
              },
              child: const Text('Test Rust Bridge'),
            ),
          ],
        ),
      ),
    );
  }
}
