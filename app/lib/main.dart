import 'dart:async';

import 'package:flutter/material.dart';
import 'package:irondash_engine_context/irondash_engine_context.dart';

import 'src/rust/frb_generated.dart';
import 'src/rust/api/session.dart';
import 'src/rust/api/video.dart' as video_api;

Future<void> main() async {
  // Platform channels (EngineContext.getEngineHandle) need the services
  // binding before runApp initializes it for us.
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  // 7f: hand the Rust texture bridge the engine handle BEFORE any
  // session can start receiving shares. A failure is non-fatal for
  // the dev client (voice/chat still work; video will error later).
  try {
    final handle = await EngineContext.instance.getEngineHandle();
    await video_api.videoPrepare(engineHandle: handle);
    debugPrint('video: texture bridge ready (engine handle $handle)');
  } catch (e, st) {
    debugPrint('video prepare failed (video disabled): $e\n$st');
  }
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
  final _chatInput = TextEditingController();
  final _chatScroll = ScrollController();

  String _status = 'disconnected';
  SessionInfo? _session;
  bool _connecting = false;
  StreamSubscription<SessionEvent>? _events;

  // Session state assembled from events:
  final List<ChannelInfo> _channels = [];
  final List<PresenceEntry> _presence = [];
  final Map<int, String> _usernames = {}; // user_id → username
  final List<ChatEntry> _chat = [];
  BigInt? _joinedChannelId;
  // 7e: join = listen + transmit (TeamSpeak shape) — the Rust side
  // opens/closes the devices on the own join/leave events. The UI only
  // reflects membership; there is no listen toggle.
  bool _voiceUp = false;
  // 7f: active remote shares — user_id → (texture_id, w, h). Driven by
  // the video_started / video_stopped events; textures are owned by
  // the Rust bridge (one per sender, stable across stop/re-share).
  final Map<int, (int, int, int)> _videos = {};

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
    _events?.cancel();
    _server.dispose();
    _user.dispose();
    _password.dispose();
    _chatInput.dispose();
    _chatScroll.dispose();
    super.dispose();
  }

  // Belt-and-suspenders with the session_closed event: whichever
  // notices first (event: ~reader death; poll: 1 Hz heartbeat of the
  // alive flag) cleans up.
  Future<void> _checkAlive() async {
    if (_session == null || _connecting) return;
    final id = _session!.sessionId;
    final alive = await isConnected(sessionId: id);
    if (_session?.sessionId != id) return;
    if (!alive && mounted) _handleClosed();
  }

  void _handleClosed() {
    _events?.cancel();
    _events = null;
    setState(() {
      _session = null;
      _status = 'connection lost';
      _joinedChannelId = null;
      _voiceUp = false;
      _channels.clear();
      _presence.clear();
      _chat.clear();
      _usernames.clear();
      _videos.clear();
    });
    // Clean the dead registry entry (a manual Disconnect may have
    // raced us — ignore).
    try {
      // best-effort; session may already be gone
      if (!_isDisconnecting) {
        final s = _lastSessionId;
        if (s != null) disconnect(sessionId: s);
      }
    } catch (_) {}
  }

  int? _lastSessionId;
  bool _isDisconnecting = false;

  Future<void> _connect() async {
    setState(() => _connecting = true);
    try {
      final s = await connect(
        server: _server.text,
        user: _user.text,
        password: _password.text,
      );
      _lastSessionId = s.sessionId;
      _events = events(sessionId: s.sessionId).listen(
        _onEvent,
        onError: (e) {
          if (mounted) setState(() => _status = 'event stream error: $e');
        },
      );
      setState(() {
        _session = s;
        _status = 'connected: user ${s.userId} @ ${s.server}';
      });
      // Bootstrap AFTER subscribing (deterministic ordering — the Rust
      // side no longer issues these itself).
      requestPresence(sessionId: s.sessionId);
      listChannels(sessionId: s.sessionId);
    } catch (e) {
      setState(() => _status = 'connect failed: $e');
    } finally {
      if (mounted) setState(() => _connecting = false);
    }
  }

  Future<void> _disconnect() async {
    if (_session == null) return;
    _isDisconnecting = true;
    try {
      await disconnect(sessionId: _session!.sessionId);
    } finally {
      _isDisconnecting = false;
    }
    _events?.cancel();
    _events = null;
    setState(() {
      _session = null;
      _status = 'disconnected';
      _joinedChannelId = null;
      _voiceUp = false;
      _channels.clear();
      _presence.clear();
      _chat.clear();
      _usernames.clear();
      _videos.clear();
    });
  }

  void _onEvent(SessionEvent ev) {
    switch (ev.kind) {
      case 'channel_list':
        setState(
          () => _channels
            ..clear()
            ..addAll(ev.channels),
        );
      case 'presence':
        setState(() {
          _presence
            ..clear()
            ..addAll(ev.users);
          for (final u in ev.users) {
            _usernames[u.userId] = u.username;
          }
        });
      case 'chat':
        final m = ev.messages.firstOrNull;
        if (m != null) {
          setState(() => _chat.add(m));
          _scrollChat();
        }
      case 'history':
        // Newest-last batch: PREPEND so live messages append after it.
        setState(() => _chat.insertAll(0, ev.messages));
      case 'voice_state':
        setState(() {
          if (ev.userId == _session?.userId) {
            // 7e: the own join/leave IS the voice lifecycle — the Rust
            // side already opened/closed the devices on this event.
            _joinedChannelId = ev.joined ? ev.channelId : null;
            _voiceUp = ev.joined;
            _status = ev.joined
                ? 'in voice — mic + speakers live'
                : 'connected';
          }
        });
      case 'voice_states':
        setState(() {
          for (final st in ev.states) {
            if (st.userId == _session?.userId) {
              _joinedChannelId = st.joined ? st.channelId : null;
              _voiceUp = st.joined;
            }
          }
        });
      case 'video_started':
        final s0 = ev.surface;
        if (s0 != null) {
          setState(() {
            // FRB maps u64 → BigInt and i64 → PlatformInt64: narrow
            // for display (dev-client resolutions fit int).
            _videos[s0.userId] = (
              s0.textureId,
              s0.width.toInt(),
              s0.height.toInt(),
            );
            final name = _usernames[s0.userId] ?? 'user ${s0.userId}';
            _status = '$name is sharing ${s0.width}×${s0.height}';
          });
        }
      case 'video_stopped':
        final s1 = ev.surface;
        if (s1 != null) {
          setState(() => _videos.remove(s1.userId));
        }
      case 'session_closed':
        if (mounted) _handleClosed();
      default:
        break;
    }
  }

  void _scrollChat() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_chatScroll.hasClients) {
        _chatScroll.animateTo(
          _chatScroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 150),
          curve: Curves.easeOut,
        );
      }
    });
  }

  Future<void> _joinChannel(ChannelInfo c) async {
    if (_session == null || _joinedChannelId == c.id) return;
    await joinVoice(sessionId: _session!.sessionId, channelId: c.id);
    // History preload for the joined channel (chat context).
    await fetchHistory(
      sessionId: _session!.sessionId,
      channelId: c.id,
      limit: 50,
      beforeMessageId: BigInt.zero,
    );
  }

  Future<void> _leaveChannel() async {
    if (_session == null || _joinedChannelId == null) return;
    await leaveVoice(sessionId: _session!.sessionId);
    setState(() => _chat.clear());
  }

  Future<void> _sendChat() async {
    if (_session == null || _joinedChannelId == null) return;
    final text = _chatInput.text.trim();
    if (text.isEmpty) return;
    _chatInput.clear();
    await sendChat(
      sessionId: _session!.sessionId,
      channelId: _joinedChannelId!,
      text: text,
    );
  }

  @override
  Widget build(BuildContext context) {
    final joinedName = _joinedChannelId == null
        ? null
        : _channels
              .where((c) => c.id == _joinedChannelId)
              .map((c) => c.name)
              .firstOrNull;
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: const Text('Parley Dev Client'),
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          // --- connection ---
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
          const SizedBox(height: 8),
          Text(_status),
          if (_session != null)
            Text(
              'fingerprint: ${_session!.fingerprint}',
              style: const TextStyle(color: Colors.white54, fontSize: 12),
            ),
          const Divider(height: 32),

          // --- channels ---
          if (_session != null) ...[
            Text('Channels', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            for (final c in _channels)
              ListTile(
                dense: true,
                contentPadding: EdgeInsets.zero,
                leading: Icon(
                  _joinedChannelId == c.id
                      ? Icons.graphic_eq
                      : (c.mode == 'music' ? Icons.music_note : Icons.mic),
                  color: _joinedChannelId == c.id
                      ? Colors.deepPurpleAccent
                      : Colors.white54,
                ),
                title: Text(c.name),
                subtitle: Text('id ${c.id.toInt()} · ${c.mode}'),
                trailing: _joinedChannelId == c.id
                    ? TextButton(
                        onPressed: _leaveChannel,
                        child: const Text('Leave'),
                      )
                    : TextButton(
                        onPressed: () => _joinChannel(c),
                        child: const Text('Join'),
                      ),
              ),
            const Divider(height: 32),

            // --- voice (7e: join-driven; no listen toggle) ---
            Row(
              children: [
                Icon(
                  _voiceUp ? Icons.mic : Icons.mic_off,
                  size: 18,
                  color: _voiceUp ? Colors.deepPurpleAccent : Colors.white38,
                ),
                const SizedBox(width: 8),
                Text(
                  _voiceUp
                      ? 'voice live${joinedName != null ? ' · $joinedName' : ''}'
                      : 'voice idle (join a channel to talk & listen)',
                  style: const TextStyle(color: Colors.white70, fontSize: 13),
                ),
              ],
            ),
            const Divider(height: 32),

            // --- 7f: remote shares (Rust-owned textures) ---
            if (_videos.isNotEmpty) ...[
              Text('Screens', style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 8),
              for (final entry in _videos.entries)
                Padding(
                  padding: const EdgeInsets.only(bottom: 8),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        '${_usernames[entry.key] ?? 'user ${entry.key}'} · '
                        '${entry.value.$2}×${entry.value.$3}',
                        style: const TextStyle(color: Colors.white54, fontSize: 12),
                      ),
                      const SizedBox(height: 4),
                      AspectRatio(
                        aspectRatio: 16 / 9,
                        child: Container(
                          decoration: BoxDecoration(
                            border: Border.all(color: Colors.white24),
                          ),
                          child: Texture(textureId: entry.value.$1),
                        ),
                      ),
                    ],
                  ),
                ),
              const Divider(height: 32),
            ],

            // --- chat ---
            Text(
              'Chat ${joinedName != null ? '· $joinedName' : '(join a channel)'}',
              style: Theme.of(context).textTheme.titleMedium,
            ),
            const SizedBox(height: 8),
            if (_joinedChannelId == null)
              const Text(
                'Join a channel to chat.',
                style: TextStyle(color: Colors.white38),
              )
            else ...[
              Container(
                constraints: const BoxConstraints(maxHeight: 240),
                decoration: BoxDecoration(
                  border: Border.all(color: Colors.white24),
                  borderRadius: BorderRadius.circular(4),
                ),
                child: ListView.builder(
                  controller: _chatScroll,
                  padding: const EdgeInsets.all(8),
                  itemCount: _chat.length,
                  itemBuilder: (_, i) {
                    final m = _chat[i];
                    final own = m.userId == _session!.userId;
                    return Padding(
                      padding: const EdgeInsets.symmetric(vertical: 2),
                      child: Text.rich(
                        TextSpan(
                          style: const TextStyle(fontSize: 13),
                          children: [
                            TextSpan(
                              text: '${m.username}: ',
                              style: TextStyle(
                                color: own
                                    ? Colors.deepPurpleAccent
                                    : Colors.white70,
                                fontWeight: FontWeight.bold,
                              ),
                            ),
                            TextSpan(
                              text: m.text,
                              style: const TextStyle(color: Colors.white),
                            ),
                          ],
                        ),
                      ),
                    );
                  },
                ),
              ),
              const SizedBox(height: 8),
              Row(
                children: [
                  Expanded(
                    child: TextField(
                      controller: _chatInput,
                      enabled: _joinedChannelId != null,
                      onSubmitted: (_) => _sendChat(),
                      decoration: const InputDecoration(
                        labelText: 'Message',
                        isDense: true,
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  ElevatedButton(
                    onPressed: _joinedChannelId == null ? null : _sendChat,
                    child: const Text('Send'),
                  ),
                ],
              ),
            ],
          ],
        ],
      ),
    );
  }
}
