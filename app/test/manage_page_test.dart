import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/rust/api/session.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/manage_page.dart';

/// The management table's contract: admins get the full menu, mods keep
/// their hands off admins and roles, and nobody can kick or remove
/// themselves. The server enforces the same rules; the UI must not offer
/// a button that would just be refused.
void main() {
  final edited = <int>[];
  final removed = <int>[];
  final kicked = <(int, BigInt)>[];
  final editedChannels = <BigInt>[];
  final deletedChannels = <BigInt>[];
  var addedUsers = 0;
  var addedChannels = 0;

  setUp(() {
    edited.clear();
    removed.clear();
    kicked.clear();
    editedChannels.clear();
    deletedChannels.clear();
    addedUsers = 0;
    addedChannels = 0;
  });

  final state = ManageStateInfo(
    users: [
      ManagedUserInfo(
        userId: 1,
        username: 'alice',
        role: 'admin',
        online: true,
        channelId: BigInt.zero,
      ),
      ManagedUserInfo(
        userId: 2,
        username: 'bob',
        role: 'mod',
        online: true,
        channelId: BigInt.from(5),
      ),
      ManagedUserInfo(
        userId: 3,
        username: 'carol',
        role: 'user',
        online: false,
        channelId: BigInt.zero,
      ),
    ],
    channels: [
      ManagedChannelInfo(
        id: BigInt.from(5),
        name: 'general',
        mode: 'speech',
        members: 2,
        ephemeral: true,
      ),
      ManagedChannelInfo(
        id: BigInt.from(6),
        name: 'jam',
        mode: 'music',
        members: 0,
        ephemeral: false,
      ),
    ],
  );

  Future<void> pump(
    WidgetTester tester, {
    required bool isAdmin,
    required int ownUserId,
  }) async {
    await tester.pumpWidget(
      MaterialApp(
        theme: buildParleyTheme(),
        home: Scaffold(
          body: ManageView(
            state: state,
            isAdmin: isAdmin,
            ownUserId: ownUserId,
            busy: false,
            onAddUser: () => addedUsers++,
            onEditUser: (user) => edited.add(user.userId),
            onRemoveUser: (user) => removed.add(user.userId),
            onKickUser: (user) => kicked.add((user.userId, user.channelId)),
            onAddChannel: () => addedChannels++,
            onEditChannel: (channel) => editedChannels.add(channel.id),
            onDeleteChannel: (channel) => deletedChannels.add(channel.id),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
  }

  testWidgets('admin sees edit, remove and kick on a connected user', (
    tester,
  ) async {
    await pump(tester, isAdmin: true, ownUserId: 1);

    expect(find.text('alice'), findsOneWidget);
    expect(find.text('bob'), findsOneWidget);
    expect(find.text('carol'), findsOneWidget);
    expect(find.text('general'), findsNothing);

    await tester.tap(find.byTooltip('Actions for bob'));
    await tester.pumpAndSettle();
    expect(find.text('Edit'), findsOneWidget);
    expect(find.text('Remove'), findsOneWidget);
    expect(find.text('Kick from #general'), findsOneWidget);

    await tester.tap(find.text('Kick from #general'));
    await tester.pumpAndSettle();
    expect(kicked, [(2, BigInt.from(5))]);

    // Carol is offline: there is no channel to kick her out of.
    await tester.tap(find.byTooltip('Actions for carol'));
    await tester.pumpAndSettle();
    expect(find.text('Remove'), findsOneWidget);
    expect(find.textContaining('Kick from'), findsNothing);
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(removed, [3]);
  });

  testWidgets('nobody gets remove or kick on their own row', (tester) async {
    await pump(tester, isAdmin: true, ownUserId: 1);

    await tester.tap(find.byTooltip('Actions for alice'));
    await tester.pumpAndSettle();
    expect(find.text('Edit'), findsOneWidget);
    expect(find.text('Remove'), findsNothing);
    expect(find.textContaining('Kick from'), findsNothing);
    expect(find.text('you'), findsOneWidget);
  });

  testWidgets('a mod cannot touch an admin', (tester) async {
    await pump(tester, isAdmin: false, ownUserId: 2);

    expect(find.byTooltip('Actions for alice'), findsNothing);
    expect(find.byIcon(Icons.lock_outline), findsOneWidget);

    // A mod still manages ordinary users.
    await tester.tap(find.byTooltip('Actions for carol'));
    await tester.pumpAndSettle();
    expect(find.text('Edit'), findsOneWidget);
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(edited, [3]);
  });

  testWidgets('the plus buttons add users and channels', (tester) async {
    await pump(tester, isAdmin: false, ownUserId: 2);

    await tester.tap(find.text('Add user'));
    await tester.pumpAndSettle();
    expect(addedUsers, 1);

    await tester.tap(find.text('Channels'));
    await tester.pumpAndSettle();
    expect(find.text('2 channels'), findsOneWidget);
    expect(find.text('general'), findsOneWidget);
    expect(find.text('2 in voice'), findsOneWidget);
    expect(find.text('music'), findsOneWidget);

    await tester.tap(find.text('Add channel'));
    await tester.pumpAndSettle();
    expect(addedChannels, 1);

    await tester.tap(find.byTooltip('Actions for #jam'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Delete'));
    await tester.pumpAndSettle();
    expect(deletedChannels, [BigInt.from(6)]);
  });
}
