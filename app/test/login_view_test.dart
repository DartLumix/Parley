import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/widgets/login_view.dart';

void main() {
  testWidgets('submits the typed server and account', (tester) async {
    final server = TextEditingController(text: 'parley.example.com:443');
    final user = TextEditingController();
    final password = TextEditingController();
    addTearDown(server.dispose);
    addTearDown(user.dispose);
    addTearDown(password.dispose);

    var connected = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: LoginView(
            server: server,
            user: user,
            password: password,
            connecting: false,
            status: 'disconnected',
            onConnect: () => connected++,
          ),
        ),
      ),
    );

    await tester.enterText(find.byType(TextField).at(1), 'alice');
    await tester.enterText(find.byType(TextField).at(2), 's3cret');
    await tester.tap(find.text('Connect'));
    await tester.pump();

    expect(connected, 1);
    expect(find.text('Parley dev'), findsOneWidget);
  });

  testWidgets('offers the remembered session above the form', (tester) async {
    final server = TextEditingController();
    final user = TextEditingController(text: 'alice');
    final password = TextEditingController();
    addTearDown(server.dispose);
    addTearDown(user.dispose);
    addTearDown(password.dispose);

    var reconnected = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: LoginView(
            server: server,
            user: user,
            password: password,
            connecting: false,
            status: 'connection lost — press Reconnect',
            onConnect: () {},
            reconnectLabel: 'Reconnect as alice',
            onReconnect: () => reconnected++,
          ),
        ),
      ),
    );

    await tester.tap(find.text('Reconnect as alice'));
    await tester.pump();
    expect(reconnected, 1);
  });

  testWidgets('the button is disabled while connecting', (tester) async {
    final server = TextEditingController();
    final user = TextEditingController();
    final password = TextEditingController();
    addTearDown(server.dispose);
    addTearDown(user.dispose);
    addTearDown(password.dispose);

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: LoginView(
            server: server,
            user: user,
            password: password,
            connecting: true,
            status: 'connecting…',
            onConnect: () {},
          ),
        ),
      ),
    );

    expect(find.text('Connecting…'), findsOneWidget);
    final button = tester.widget<ElevatedButton>(find.byType(ElevatedButton));
    expect(button.onPressed, isNull);
  });
}
