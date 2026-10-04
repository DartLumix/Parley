import 'dart:async';

import 'package:flutter/material.dart';

import 'src/rust/frb_generated.dart';
import 'src/rust/api/simple.dart';
import 'src/rust/api/session.dart';

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
        brightness: Brightness.dark,
        scaffoldBackgroundColor: Colors.black,
        appBarTheme: const AppBarTheme(
          backgroundColor: Colors.black,
          foregroundColor: Colors.white,
        ),
        colorScheme: ColorScheme.fromSeed(
          seedColor: Colors.deepPurple,
          brightness: Brightness.dark,
        ),
        inputDecorationTheme: const InputDecorationTheme(
          labelStyle: TextStyle(color: Colors.white70),
          enabledBorder: OutlineInputBorder(
            borderSide: BorderSide(color: Colors.white24),
          ),
          focusedBorder: OutlineInputBorder(
            borderSide: BorderSide(color: Colors.deepPurpleAccent),
          ),
        ),
        elevatedButtonTheme: ElevatedButtonThemeData(
          style: ElevatedButton.styleFrom(
            backgroundColor: Colors.deepPurple,
            foregroundColor: Colors.white,
            disabledBackgroundColor: Colors.grey.shade900,
            disabledForegroundColor: Colors.grey.shade600,
          ),
        ),
        dividerColor: Colors.white24,
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
  final _server = TextEditingController(text: '127.0.0.1:4443');
  final _user = TextEditingController();
  final _password = TextEditingController();

  String _status = 'disconnected';
  SessionInfo? _session;
  bool _connecting = false;

  /// 1 Hz liveness poll: Rust's drain reader flips `alive` false when
  /// the control stream dies (QUIC idle timeout ~10 s after the server
  /// vanishes). Poll it — the UI must reflect connection truth, not
  /// the last button press. Step 2 replaces polling with the real
  /// event stream for chat/presence/voice-state.
  late final Timer _liveness;

  @override
  void initState() {
    super.initState();
    _liveness = Timer.periodic(
      const Duration(seconds: 1),
      (_) => _checkAlive(),
    );
  }

  @override
  void dispose() {
    _liveness.cancel();
    _server.dispose();
    _user.dispose();
    _password.dispose();
    super.dispose();
  }

  Future<void> _checkAlive() async {
    if (_session == null || _connecting) return;
    final id = _session!.sessionId;
    final alive = await isConnected(sessionId: id);
    // Guard: the session may have changed/disconnected while awaiting.
    if (_session?.sessionId != id) return;
    if (!alive && mounted) {
      // Clean the dead entry out of the Rust registry (ignore errors:
      // a manual Disconnect may have raced us to it).
      try {
        await disconnect(sessionId: id);
      } catch (_) {}
      setState(() {
        _session = null;
        _status = 'connection lost';
      });
    }
  }

  Future<void> _connect() async {
    setState(() => _connecting = true);
    try {
      final s = await connect(
        server: _server.text,
        user: _user.text,
        password: _password.text,
      );
      setState(() {
        _session = s;
        _status = 'connected: user ${s.userId} @ ${s.server}';
      });
    } catch (e) {
      setState(() => _status = 'connect failed: $e');
    } finally {
      if (mounted) setState(() => _connecting = false);
    }
  }

  Future<void> _disconnect() async {
    if (_session == null) return;
    await disconnect(sessionId: _session!.sessionId);
    setState(() {
      _session = null;
      _status = 'disconnected';
    });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: const Text('Parley Dev Client'),
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          TextField(
            controller: _server,
            enabled: _session == null,
            decoration: const InputDecoration(labelText: 'Server (IP:port)'),
          ),
          TextField(
            controller: _user,
            enabled: _session == null,
            decoration: const InputDecoration(labelText: 'Username'),
          ),
          TextField(
            controller: _password,
            enabled: _session == null,
            obscureText: true,
            decoration: const InputDecoration(labelText: 'Password'),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              ElevatedButton(
                onPressed: _connecting || _session != null ? null : _connect,
                child: const Text('Connect'),
              ),
              const SizedBox(width: 12),
              ElevatedButton(
                onPressed: _session == null ? null : _disconnect,
                child: const Text('Disconnect'),
              ),
            ],
          ),
          const SizedBox(height: 12),
          Text(_status),
          if (_session != null) ...[
            const SizedBox(height: 8),
            Text('user id: ${_session!.userId}'),
            Text('token: ${_session!.sessionToken}'),
            Text('fingerprint: ${_session!.fingerprint}'),
          ],
          const Divider(height: 32),
          ElevatedButton(
            onPressed: () async {
              final result = await ping();
              setState(() => _status = 'Rust says: $result');
            },
            child: const Text('Test Rust Bridge'),
          ),
        ],
      ),
    );
  }
}
