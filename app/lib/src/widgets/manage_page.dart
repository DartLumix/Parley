import 'dart:async';

import 'package:flutter/material.dart';

import '../rust/api/session.dart';
import '../theme.dart';
import 'pill.dart';

/// Admin/mod server management: the users and channels tables, with the
/// writes the `/manage` commands expose. Reachable only from the app bar
/// of a session whose role is not `user`.
///
/// Every action is a request; the tables are replaced by the `manage`
/// event that follows (the same push broadcasts to every open page, so
/// two admins never race on stale rows).
class ManagePage extends StatefulWidget {
  const ManagePage({
    super.key,
    required this.sessionId,
    required this.userId,
    required this.role,
  });

  final int sessionId;

  /// The signed-in account, so the page can keep it out of the actions
  /// (no self-remove, no self-kick, no self-demotion).
  final int userId;
  final String role;

  @override
  State<ManagePage> createState() => _ManagePageState();
}

class _ManagePageState extends State<ManagePage> {
  static const _roles = ['user', 'mod', 'admin'];

  StreamSubscription<SessionEvent>? _events;
  Timer? _answerTimeout;
  ManageStateInfo? _state;
  bool _busy = false;
  String? _error;

  bool get _isAdmin => widget.role == 'admin';

  @override
  void initState() {
    super.initState();
    _events = events(sessionId: widget.sessionId).listen(_onEvent);
    _refresh();
  }

  @override
  void dispose() {
    _events?.cancel();
    _answerTimeout?.cancel();
    super.dispose();
  }

  void _onEvent(SessionEvent event) {
    if (!mounted) return;
    if (event.kind == 'session_closed') {
      Navigator.of(context).maybePop();
      return;
    }
    if (event.kind != 'manage') return;
    _answerTimeout?.cancel();
    setState(() {
      _busy = false;
      if (event.manage != null) _state = event.manage;
      _error = event.manageError.isEmpty ? null : event.manageError;
    });
  }

  void _refresh() {
    if (_busy) return;
    _send(() => manageState(sessionId: widget.sessionId));
  }

  /// Send one management request and wait for its `manage` event. The
  /// timeout is for a server that predates the command: it ignores the
  /// frame, and without this the page would sit disabled forever.
  Future<void> _send(Future<void> Function() request) async {
    if (_busy) return;
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await request();
    } catch (e) {
      if (mounted) {
        setState(() {
          _busy = false;
          _error = '$e';
        });
      }
      return;
    }
    if (!mounted) return;
    _answerTimeout?.cancel();
    _answerTimeout = Timer(const Duration(seconds: 6), () {
      if (!mounted || !_busy) return;
      setState(() {
        _busy = false;
        _error =
            'the server did not answer — it may predate the management '
            'commands';
      });
    });
  }

  String _channelName(BigInt id) {
    for (final channel in _state?.channels ?? const <ManagedChannelInfo>[]) {
      if (channel.id == id) return channel.name;
    }
    return 'channel';
  }

  Future<bool> _confirm({
    required String title,
    required String body,
    required String action,
  }) async {
    final answer = await showDialog<bool>(
      context: context,
      builder: (dialogContext) => AlertDialog(
        title: Text(title),
        content: Text(body),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(dialogContext).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: FilledButton.styleFrom(backgroundColor: Colors.redAccent),
            onPressed: () => Navigator.of(dialogContext).pop(true),
            child: Text(action),
          ),
        ],
      ),
    );
    return answer ?? false;
  }

  Future<void> _editUser([ManagedUserInfo? target]) async {
    final editing = target != null;
    final username = TextEditingController(text: target?.username ?? '');
    final password = TextEditingController();
    final formKey = GlobalKey<FormState>();
    var role = target?.role ?? 'user';
    // A mod keeps the row's role untouched. An admin can promote and
    // demote, except themself (the server refuses that too — a lone admin
    // must not lock the server's own management out).
    final canChangeRole =
        _isAdmin && (target == null || target.userId != widget.userId);
    final result = await showDialog<(String, String, String)>(
      context: context,
      builder: (dialogContext) => StatefulBuilder(
        builder: (dialogContext, setDialogState) => AlertDialog(
          scrollable: true,
          title: Text(editing ? 'Edit ${target.username}' : 'Add user'),
          content: SizedBox(
            width: 380,
            child: Form(
              key: formKey,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  TextFormField(
                    controller: username,
                    autofocus: target == null,
                    decoration: const InputDecoration(labelText: 'Username'),
                    validator: (value) => value == null || value.trim().isEmpty
                        ? 'Required'
                        : null,
                  ),
                  const SizedBox(height: 12),
                  TextFormField(
                    controller: password,
                    obscureText: true,
                    decoration: InputDecoration(
                      labelText: editing ? 'New password' : 'Password',
                      helperText: editing
                          ? 'Leave blank to keep the current one'
                          : null,
                    ),
                    validator: (value) {
                      if (!editing && (value == null || value.isEmpty)) {
                        return 'Required';
                      }
                      return null;
                    },
                  ),
                  const SizedBox(height: 16),
                  Row(
                    children: [
                      const Text('Permissions'),
                      const Spacer(),
                      DropdownButton<String>(
                        value: role,
                        underline: const SizedBox.shrink(),
                        onChanged: canChangeRole
                            ? (value) {
                                if (value != null) {
                                  setDialogState(() => role = value);
                                }
                              }
                            : null,
                        items: [
                          for (final value in _roles)
                            DropdownMenuItem(value: value, child: Text(value)),
                        ],
                      ),
                    ],
                  ),
                  if (!canChangeRole)
                    Padding(
                      padding: const EdgeInsets.only(top: 4),
                      child: Text(
                        editing
                            ? (target.userId == widget.userId
                                  ? 'Your own role cannot be changed.'
                                  : 'Only an admin can change permissions.')
                            : 'New accounts start as users.',
                        style: const TextStyle(
                          color: Colors.white38,
                          fontSize: 11,
                        ),
                      ),
                    ),
                  if (editing)
                    const Padding(
                      padding: EdgeInsets.only(top: 6),
                      child: Text(
                        'A new name or permission signs them out; their saved '
                        'login still works.',
                        style: TextStyle(color: Colors.white38, fontSize: 11),
                      ),
                    ),
                ],
              ),
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(dialogContext).pop(),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () {
                if (!(formKey.currentState?.validate() ?? false)) return;
                Navigator.of(dialogContext)
                    .pop((username.text.trim(), password.text, role));
              },
              child: Text(editing ? 'Save' : 'Add'),
            ),
          ],
        ),
      ),
    );
    username.dispose();
    password.dispose();
    if (result == null || !mounted) return;
    final (nextName, nextPassword, nextRole) = result;
    if (editing) {
      await _send(
        () => manageUpdateUser(
          sessionId: widget.sessionId,
          userId: target.userId,
          username: nextName,
          password: nextPassword,
          role: nextRole,
        ),
      );
    } else {
      await _send(
        () => manageAddUser(
          sessionId: widget.sessionId,
          username: nextName,
          password: nextPassword,
          role: nextRole,
        ),
      );
    }
  }

  Future<void> _removeUser(ManagedUserInfo target) async {
    final ok = await _confirm(
      title: 'Remove ${target.username}?',
      body:
          'The account is deleted for good. If they are connected they '
          'are disconnected.',
      action: 'Remove',
    );
    if (!ok || !mounted) return;
    await _send(
      () =>
          manageRemoveUser(sessionId: widget.sessionId, userId: target.userId),
    );
  }

  Future<void> _kickUser(ManagedUserInfo target) async {
    final channel = _channelName(target.channelId);
    final ok = await _confirm(
      title: 'Kick ${target.username}?',
      body:
          'They are dropped from #$channel right now. They can re-join '
          'immediately — this is not a ban.',
      action: 'Kick',
    );
    if (!ok || !mounted) return;
    await _send(
      () => manageKick(
        sessionId: widget.sessionId,
        userId: target.userId,
        channelId: target.channelId,
      ),
    );
  }

  Future<void> _editChannel([ManagedChannelInfo? target]) async {
    final editing = target != null;
    final name = TextEditingController(text: target?.name ?? '');
    final formKey = GlobalKey<FormState>();
    var mode = target?.mode ?? 'speech';
    final result = await showDialog<(String, String)>(
      context: context,
      builder: (dialogContext) => StatefulBuilder(
        builder: (dialogContext, setDialogState) => AlertDialog(
          scrollable: true,
          title: Text(editing ? 'Edit #${target.name}' : 'Add channel'),
          content: SizedBox(
            width: 380,
            child: Form(
              key: formKey,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  TextFormField(
                    controller: name,
                    autofocus: target == null,
                    decoration: const InputDecoration(labelText: 'Name'),
                    validator: (value) => value == null || value.trim().isEmpty
                        ? 'Required'
                        : null,
                  ),
                  const SizedBox(height: 16),
                  Row(
                    children: [
                      const Text('Audio mode'),
                      const Spacer(),
                      DropdownButton<String>(
                        value: mode,
                        underline: const SizedBox.shrink(),
                        onChanged: (value) {
                          if (value != null) setDialogState(() => mode = value);
                        },
                        items: const [
                          DropdownMenuItem(
                            value: 'speech',
                            child: Text('Speech'),
                          ),
                          DropdownMenuItem(
                            value: 'music',
                            child: Text('Music'),
                          ),
                        ],
                      ),
                    ],
                  ),
                  const Padding(
                    padding: EdgeInsets.only(top: 4),
                    child: Text(
                      'Music sends stereo at the higher voice bitrate; speech '
                      'is mono.',
                      style: TextStyle(color: Colors.white38, fontSize: 11),
                    ),
                  ),
                ],
              ),
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(dialogContext).pop(),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () {
                if (!(formKey.currentState?.validate() ?? false)) return;
                Navigator.of(dialogContext).pop((name.text.trim(), mode));
              },
              child: Text(editing ? 'Save' : 'Add'),
            ),
          ],
        ),
      ),
    );
    name.dispose();
    if (result == null || !mounted) return;
    final (nextName, nextMode) = result;
    if (editing) {
      await _send(
        () => manageUpdateChannel(
          sessionId: widget.sessionId,
          channelId: target.id,
          name: nextName,
          mode: nextMode,
        ),
      );
    } else {
      await _send(
        () => manageCreateChannel(
          sessionId: widget.sessionId,
          name: nextName,
          mode: nextMode,
        ),
      );
    }
  }

  Future<void> _deleteChannel(ManagedChannelInfo target) async {
    final ok = await _confirm(
      title: 'Delete #${target.name}?',
      body: target.members > 0
          ? 'Its ${target.members} member(s) are dropped from voice. The '
                'channel is gone for good.'
          : 'The channel is gone for good.',
      action: 'Delete',
    );
    if (!ok || !mounted) return;
    await _send(
      () => manageDeleteChannel(
        sessionId: widget.sessionId,
        channelId: target.id,
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final state = _state;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Server management'),
        actions: [
          if (_busy)
            const Padding(
              padding: EdgeInsets.symmetric(horizontal: 12),
              child: Center(
                child: SizedBox(
                  width: 16,
                  height: 16,
                  child: CircularProgressIndicator(strokeWidth: 2),
                ),
              ),
            ),
          IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh, size: 20),
            onPressed: _busy ? null : _refresh,
          ),
          const SizedBox(width: 4),
        ],
      ),
      body: state == null
          ? Center(
              child: _error == null
                  ? const CircularProgressIndicator()
                  : Padding(
                      padding: const EdgeInsets.all(24),
                      child: Column(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Text(
                            _error!,
                            textAlign: TextAlign.center,
                            style: const TextStyle(color: Colors.white70),
                          ),
                          const SizedBox(height: 12),
                          FilledButton(
                            onPressed: _refresh,
                            child: const Text('Retry'),
                          ),
                        ],
                      ),
                    ),
            )
          : Column(
              children: [
                if (_error != null)
                  Container(
                    width: double.infinity,
                    color: Colors.redAccent.withValues(alpha: 0.12),
                    padding: const EdgeInsets.symmetric(
                      horizontal: 16,
                      vertical: 8,
                    ),
                    child: Row(
                      children: [
                        const Icon(
                          Icons.error_outline,
                          size: 16,
                          color: Colors.redAccent,
                        ),
                        const SizedBox(width: 8),
                        Expanded(
                          child: Text(
                            _error!,
                            style: const TextStyle(
                              color: Colors.redAccent,
                              fontSize: 12,
                            ),
                          ),
                        ),
                      ],
                    ),
                  ),
                Expanded(
                  child: ManageView(
                    state: state,
                    isAdmin: _isAdmin,
                    ownUserId: widget.userId,
                    busy: _busy,
                    onAddUser: () => _editUser(),
                    onEditUser: _editUser,
                    onRemoveUser: _removeUser,
                    onKickUser: _kickUser,
                    onAddChannel: () => _editChannel(),
                    onEditChannel: _editChannel,
                    onDeleteChannel: _deleteChannel,
                  ),
                ),
              ],
            ),
    );
  }
}

/// The tables and their action menus. Split from [ManagePage] so the
/// permission gating and the rows can be exercised without a live
/// session (see `manage_page_test.dart`).
class ManageView extends StatelessWidget {
  const ManageView({
    super.key,
    required this.state,
    required this.isAdmin,
    required this.ownUserId,
    required this.busy,
    required this.onAddUser,
    required this.onEditUser,
    required this.onRemoveUser,
    required this.onKickUser,
    required this.onAddChannel,
    required this.onEditChannel,
    required this.onDeleteChannel,
  });

  final ManageStateInfo state;
  final bool isAdmin;
  final int ownUserId;
  final bool busy;
  final VoidCallback onAddUser;
  final void Function(ManagedUserInfo) onEditUser;
  final void Function(ManagedUserInfo) onRemoveUser;
  final void Function(ManagedUserInfo) onKickUser;
  final VoidCallback onAddChannel;
  final void Function(ManagedChannelInfo) onEditChannel;
  final void Function(ManagedChannelInfo) onDeleteChannel;

  /// Mods can manage users and channels, but not another admin (and not
  /// their own role). The server enforces the same rule; hiding it here
  /// keeps the tables honest about what a click would do.
  bool _canActOn(ManagedUserInfo user) =>
      isAdmin || user.role != 'admin' || user.userId == ownUserId;

  String _channelName(BigInt id) {
    for (final channel in state.channels) {
      if (channel.id == id) return channel.name;
    }
    return 'channel';
  }

  @override
  Widget build(BuildContext context) {
    return DefaultTabController(
      length: 2,
      child: Column(
        children: [
          const TabBar(
            labelColor: Colors.white,
            unselectedLabelColor: Colors.white54,
            indicatorColor: accent,
            dividerColor: Colors.white12,
            tabs: [
              Tab(text: 'Users'),
              Tab(text: 'Channels'),
            ],
          ),
          Expanded(
            child: TabBarView(children: [_users(context), _channels(context)]),
          ),
        ],
      ),
    );
  }

  Widget _users(BuildContext context) {
    return Column(
      children: [
        _tableHeader(
          context,
          label: '${state.users.length} accounts',
          action: FilledButton.tonalIcon(
            onPressed: busy ? null : onAddUser,
            icon: const Icon(Icons.person_add_alt_1, size: 18),
            label: const Text('Add user'),
          ),
        ),
        Expanded(
          child: state.users.isEmpty
              ? const _Empty(text: 'No accounts')
              : ListView.separated(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  itemCount: state.users.length,
                  separatorBuilder: (_, _) =>
                      const Divider(height: 1, color: Colors.white10),
                  itemBuilder: (context, index) =>
                      _userRow(context, state.users[index]),
                ),
        ),
      ],
    );
  }

  Widget _userRow(BuildContext context, ManagedUserInfo user) {
    final self = user.userId == ownUserId;
    final canAct = _canActOn(user);
    final canKick = canAct && !self && user.channelId > BigInt.zero;
    final canRemove = canAct && !self;
    return ListTile(
      dense: true,
      leading: CircleAvatar(
        radius: 16,
        backgroundColor: barSurface,
        child: Text(
          user.username.isEmpty ? '?' : user.username[0].toUpperCase(),
          style: const TextStyle(color: Colors.white, fontSize: 13),
        ),
      ),
      title: Row(
        children: [
          Flexible(
            child: Text(
              user.username,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(fontSize: 14),
            ),
          ),
          if (self)
            const Padding(
              padding: EdgeInsets.only(left: 6),
              child: Text(
                'you',
                style: TextStyle(color: Colors.white38, fontSize: 11),
              ),
            ),
        ],
      ),
      subtitle: Padding(
        padding: const EdgeInsets.only(top: 2),
        child: Wrap(
          spacing: 6,
          runSpacing: 2,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            _roleChip(user.role),
            Pill(
              text: user.online ? 'online' : 'offline',
              color: user.online ? Colors.greenAccent : Colors.white38,
            ),
            if (user.channelId > BigInt.zero)
              Text(
                '#${_channelName(user.channelId)}',
                style: const TextStyle(color: Colors.white54, fontSize: 11),
              ),
          ],
        ),
      ),
      trailing: canAct
          ? PopupMenuButton<String>(
              tooltip: 'Actions for ${user.username}',
              enabled: !busy,
              onSelected: (value) {
                switch (value) {
                  case 'edit':
                    onEditUser(user);
                  case 'remove':
                    onRemoveUser(user);
                  case 'kick':
                    onKickUser(user);
                }
              },
              itemBuilder: (context) => [
                const PopupMenuItem(
                  value: 'edit',
                  child: _MenuRow(icon: Icons.edit_outlined, label: 'Edit'),
                ),
                if (canRemove)
                  const PopupMenuItem(
                    value: 'remove',
                    child: _MenuRow(
                      icon: Icons.person_remove_outlined,
                      label: 'Remove',
                      danger: true,
                    ),
                  ),
                if (canKick)
                  PopupMenuItem(
                    value: 'kick',
                    child: _MenuRow(
                      icon: Icons.logout,
                      label: 'Kick from #${_channelName(user.channelId)}',
                      danger: true,
                    ),
                  ),
              ],
            )
          : const Icon(Icons.lock_outline, size: 16, color: Colors.white38),
    );
  }

  Widget _channels(BuildContext context) {
    return Column(
      children: [
        _tableHeader(
          context,
          label: '${state.channels.length} channels',
          action: FilledButton.tonalIcon(
            onPressed: busy ? null : onAddChannel,
            icon: const Icon(Icons.add, size: 18),
            label: const Text('Add channel'),
          ),
        ),
        Expanded(
          child: state.channels.isEmpty
              ? const _Empty(text: 'No channels')
              : ListView.separated(
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  itemCount: state.channels.length,
                  separatorBuilder: (_, _) =>
                      const Divider(height: 1, color: Colors.white10),
                  itemBuilder: (context, index) =>
                      _channelRow(context, state.channels[index]),
                ),
        ),
      ],
    );
  }

  Widget _channelRow(BuildContext context, ManagedChannelInfo channel) {
    return ListTile(
      dense: true,
      leading: const Icon(Icons.tag, size: 18, color: Colors.white54),
      title: Text(
        channel.name,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(fontSize: 14),
      ),
      subtitle: Padding(
        padding: const EdgeInsets.only(top: 2),
        child: Wrap(
          spacing: 6,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Pill(
              text: channel.mode,
              color: channel.mode == 'music' ? accentSoft : Colors.white54,
            ),
            Text(
              '${channel.members} in voice',
              style: const TextStyle(color: Colors.white54, fontSize: 11),
            ),
          ],
        ),
      ),
      trailing: PopupMenuButton<String>(
        tooltip: 'Actions for #${channel.name}',
        enabled: !busy,
        onSelected: (value) {
          switch (value) {
            case 'edit':
              onEditChannel(channel);
            case 'delete':
              onDeleteChannel(channel);
          }
        },
        itemBuilder: (context) => const [
          PopupMenuItem(
            value: 'edit',
            child: _MenuRow(icon: Icons.edit_outlined, label: 'Edit'),
          ),
          PopupMenuItem(
            value: 'delete',
            child: _MenuRow(
              icon: Icons.delete_outline,
              label: 'Delete',
              danger: true,
            ),
          ),
        ],
      ),
    );
  }

  Widget _tableHeader(
    BuildContext context, {
    required String label,
    required Widget action,
  }) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 10, 16, 10),
      child: Row(
        children: [
          Text(
            label,
            style: const TextStyle(color: Colors.white54, fontSize: 12),
          ),
          const Spacer(),
          action,
        ],
      ),
    );
  }

  Widget _roleChip(String role) {
    final color = switch (role) {
      'admin' => accentSoft,
      'mod' => Colors.amber,
      _ => Colors.white54,
    };
    return Pill(text: role, color: color);
  }
}

class _MenuRow extends StatelessWidget {
  const _MenuRow({
    required this.icon,
    required this.label,
    this.danger = false,
  });

  final IconData icon;
  final String label;
  final bool danger;

  @override
  Widget build(BuildContext context) {
    final color = danger ? Colors.redAccent : Colors.white70;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(icon, size: 18, color: color),
        const SizedBox(width: 8),
        Flexible(
          child: Text(
            label,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(color: color, fontSize: 13),
          ),
        ),
      ],
    );
  }
}

class _Empty extends StatelessWidget {
  const _Empty({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Text(
        text,
        style: const TextStyle(color: Colors.white38, fontSize: 13),
      ),
    );
  }
}
