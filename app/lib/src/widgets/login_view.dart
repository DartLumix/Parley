import 'package:flutter/material.dart';

import '../version.dart';

/// Disconnected state: the only thing on screen. Once a session is up
/// this whole view (fields + Connect) goes away; the app bar keeps the
/// account and a Disconnect action.
class LoginView extends StatelessWidget {
  const LoginView({
    super.key,
    required this.server,
    required this.user,
    required this.password,
    required this.connecting,
    required this.status,
    required this.onConnect,
  });

  final TextEditingController server;
  final TextEditingController user;
  final TextEditingController password;
  final bool connecting;
  final String status;
  final VoidCallback onConnect;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(24),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 380),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              const Text(
                'Sign in',
                style: TextStyle(fontSize: 22, fontWeight: FontWeight.w600),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: server,
                decoration: const InputDecoration(
                  labelText: 'Server',
                  hintText: 'host or IP, port optional (default 4443)',
                ),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: user,
                textInputAction: TextInputAction.next,
                decoration: const InputDecoration(labelText: 'Username'),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: password,
                obscureText: true,
                onSubmitted: (_) => onConnect(),
                decoration: const InputDecoration(labelText: 'Password'),
              ),
              const SizedBox(height: 16),
              ElevatedButton(
                onPressed: connecting ? null : onConnect,
                style: ElevatedButton.styleFrom(
                  padding: const EdgeInsets.symmetric(vertical: 14),
                ),
                child: Text(connecting ? 'Connecting…' : 'Connect'),
              ),
              const SizedBox(height: 12),
              Text(
                status,
                textAlign: TextAlign.center,
                style: const TextStyle(color: Colors.white54, fontSize: 12),
              ),
              const SizedBox(height: 24),
              Text(
                'Parley $parleyVersion',
                textAlign: TextAlign.center,
                style: const TextStyle(color: Colors.white24, fontSize: 11),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
