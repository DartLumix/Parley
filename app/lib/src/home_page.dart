import 'dart:async';

import 'package:flutter/material.dart';

import 'rust/api/session.dart';
import 'theme.dart';
import 'widgets/fullscreen_share.dart';
import 'widgets/login_view.dart';
import 'widgets/pill.dart';
import 'window.dart';

/// The whole client: login, channels, chat, voice membership, share
/// watching and the fullscreen viewer. One stateful page on purpose —
/// a share ending, a disconnect and a fullscreen exit all have to be
/// handled by the same state that holds the session.
class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
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
  // Voice membership (who is in which channel): the snapshot at connect
  // plus every join/leave broadcast since.
  final Map<int, BigInt> _voiceChannel = {}; // user_id → channel_id
  // 7e: join = listen + transmit (TeamSpeak shape) — the Rust side
  // opens/closes the devices on the own join/leave events. The UI only
  // reflects membership; there is no listen toggle.
  bool _voiceUp = false;
  // 7f: active remote shares — user_id → (texture_id, w, h). Driven by
  // the video_started / video_stopped events; textures are owned by
  // the Rust bridge (one per sender, stable across stop/re-share).
  final Map<int, (int, int, int)> _videos = {};
  // Which share the right-hand pane is showing. Starting a share no
  // longer throws video onto the page: the pane offers a "Watch"
  // button and the stream is only rendered once chosen.
  int? _watching;
  // Fullscreen viewer (a whole-page mode, not a route: a share ending
  // has to close it from the same state that receives the event). The
  // GTK window goes genuinely fullscreen alongside it.
  int? _fullscreen;
  // 7g: our own share state (start/stop button) + the hw-encode toggle.
  bool _sharing = false;
  bool _hwEncode = false;
  bool _hwEncodeSupported = false;
  // Mic mute (the button next to Share): while set, the Rust capture
  // callback sends nothing at all. Session-scoped; a new login starts
  // unmuted.
  bool _micMuted = false;

  late final Timer _liveness;

  @override
  void initState() {
    super.initState();
    // The desktop can take the window out of fullscreen itself (its own
    // shortcut, its own "how to exit" hint). The viewer page follows, so
    // the UI is never a fullscreen view inside a windowed window.
    windowChannel.setMethodCallHandler((call) async {
      if (call.method == 'fullscreenChanged' && call.arguments == false) {
        if (_fullscreen != null) _exitFullscreen();
      }
      return null;
    });
    _liveness = Timer.periodic(
      const Duration(seconds: 1),
      (_) => _checkAlive(),
    );
    // 7g: the toggle's initial state (persisted process-wide in Rust).
    // Best-effort: featureless builds error and stay false. The toggle
    // only renders when this build can hardware-encode at all.
    try {
      hwEncodeSupported().then((supported) {
        if (!mounted || !supported) return;
        hwEncodeEnabled().then((v) {
          if (!mounted) return;
          setState(() {
            _hwEncodeSupported = true;
            _hwEncode = v;
          });
        });
      });
    } catch (_) {}
  }

  @override
  void dispose() {
    windowChannel.setMethodCallHandler(null);
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
    if (!alive && mounted) {
      _handleClosed();
      return;
    }
    // Roster heal: presence and voice membership are event-driven, so
    // a session that connects after our snapshot arrives with a name
    // we have never seen. A slow re-poll keeps the channel cards
    // honest without polling on the media path.
    if (++_pollTick % 10 == 0) {
      requestPresence(sessionId: id);
      requestVoiceStates(sessionId: id);
    }
  }

  int _pollTick = 0;

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
      _voiceChannel.clear();
      _watching = null;
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
    });
    setWindowFullscreen(false);
    _muteAllShares();
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
      // Who was already sitting in a channel before we connected —
      // no broadcast replays that, only the snapshot has it.
      requestVoiceStates(sessionId: s.sessionId);
    } catch (e) {
      setState(() => _status = 'connect failed: $e');
    } finally {
      if (mounted) setState(() => _connecting = false);
    }
  }

  Future<void> _disconnect() async {
    if (_session == null) return;
    await _muteAllShares();
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
      _voiceChannel.clear();
      _watching = null;
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
    });
    setWindowFullscreen(false);
  }

  // --- fullscreen: a real window mode, with only the two controls the
  // viewer needs (exit, stop watching) over the picture.

  void _enterFullscreen(int userId) {
    _watch(userId);
    setState(() => _fullscreen = userId);
    setWindowFullscreen(true);
  }

  /// Which share is on screen, and therefore audible. A screen share's
  /// system audio is silent until its card is opened: hearing someone's
  /// music the instant they start sharing is not the same thing as
  /// watching the share. Their VOICE is a separate stream and keeps
  /// coming through either way.
  void _watch(int? userId) {
    final previous = _watching;
    setState(() => _watching = userId);
    if (previous != null && previous != userId) {
      _setShareAudio(previous, false);
    }
    if (userId != null) _setShareAudio(userId, true);
  }

  /// Sharers whose share audio the Rust side is currently playing —
  /// muted again on disconnect so a later session starts silent.
  final Set<int> _audibleShares = {};

  Future<void> _setShareAudio(int userId, bool audible) async {
    final sessionId = _session?.sessionId;
    if (sessionId == null) return;
    if (audible) {
      _audibleShares.add(userId);
    } else {
      _audibleShares.remove(userId);
    }
    try {
      await setShareAudio(
        sessionId: sessionId,
        userId: userId,
        audible: audible,
      );
    } catch (e) {
      debugPrint('share audio: $e');
    }
  }

  Future<void> _muteAllShares() async {
    final sessionId = _session?.sessionId;
    final ids = _audibleShares.toList();
    _audibleShares.clear();
    if (sessionId == null) return;
    for (final id in ids) {
      try {
        await setShareAudio(
          sessionId: sessionId,
          userId: id,
          audible: false,
        );
      } catch (_) {}
    }
  }

  void _exitFullscreen() {
    setWindowFullscreen(false);
    if (mounted) setState(() => _fullscreen = null);
  }

  /// Drop share cards this client can no longer legitimately hold.
  ///
  /// A share is only visible to the sharer's voice-channel members (the
  /// server routes video that way) and dies with the sharer's
  /// membership or session. The server now announces that; this is the
  /// backstop for a notice that never arrived — the "card that stays
  /// there forever, Watch shows the last frame" bug.
  void _pruneShares() {
    if (_videos.isEmpty) return;
    final me = _session?.userId;
    final myChannel = _joinedChannelId;
    final online = {for (final u in _presence) u.userId};
    _videos.removeWhere((id, _) {
      if (id == me) return true; // our own share is not a remote pane
      if (online.isNotEmpty && !online.contains(id)) return true;
      if (myChannel == null) return true; // not in voice: nothing routed
      if (_voiceChannel.isNotEmpty) {
        final theirs = _voiceChannel[id];
        if (theirs == null || theirs != myChannel) return true;
      }
      return false;
    });
    if (_watching != null && !_videos.containsKey(_watching)) {
      _setShareAudio(_watching!, false);
      _watching = null;
    }
    for (final id in _audibleShares.toList()) {
      if (!_videos.containsKey(id)) _setShareAudio(id, false);
    }
    if (_fullscreen != null && !_videos.containsKey(_fullscreen)) {
      _fullscreen = null;
      setWindowFullscreen(false);
    }
  }

  // 7g: share lifecycle. Start: portal dialog opens (compositor's
  // picker); share_started/share_ended events flip the button.
  // FRB exposes these as async (even though the Rust side returns
  // instantly) — the portal dialog blocks start for seconds.
  Future<void> _startShare() async {
    if (_session == null) return;
    try {
      await startScreenShare(sessionId: _session!.sessionId);
      // share_started arrives as an event; the button flips there.
    } catch (e) {
      setState(() => _status = 'share failed: $e');
    }
  }

  Future<void> _stopShare() async {
    if (_session == null) return;
    try {
      // Returns void (graceful teardown); share_ended arrives as an
      // event when the supervisor finishes (may take a second).
      stopScreenShare(sessionId: _session!.sessionId);
    } catch (e) {
      setState(() => _status = 'share stop failed: $e');
    }
  }

  /// Latency probing and other harness chatter rides the chat
  /// transport (`PARLEY_LATENCY <ns>` from the --measure client). It
  /// is diagnostic traffic, never conversation — keep it out of both
  /// live and replayed chat (the rows are still in the server's log).
  static bool _isHarnessNoise(String text) =>
      text.startsWith('PARLEY_LATENCY ') || text.startsWith('PARLEY_HELLO ');

  void _ingestChat(Iterable<ChatEntry> rows, {required bool prepend}) {
    final clean = rows.where((m) => !_isHarnessNoise(m.text)).toList();
    if (clean.isEmpty) return;
    setState(() {
      if (prepend) {
        _chat.insertAll(0, clean);
      } else {
        _chat.addAll(clean);
      }
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
          _pruneShares();
        });
      case 'chat':
        _ingestChat(ev.messages, prepend: false);
        _scrollChat();
      case 'history':
        // Newest-last batch: PREPEND so live messages append after it.
        _ingestChat(ev.messages, prepend: true);
        _scrollChat();
      case 'voice_state':
        setState(() {
          if (ev.joined) {
            _voiceChannel[ev.userId] = ev.channelId;
          } else {
            _voiceChannel.remove(ev.userId);
          }
          if (ev.userId == _session?.userId) {
            // 7e: the own join/leave IS the voice lifecycle — the Rust
            // side already opened/closed the devices on this event.
            _joinedChannelId = ev.joined ? ev.channelId : null;
            _voiceUp = ev.joined;
            _status = ev.joined
                ? 'in voice — mic + speakers live'
                : 'connected';
          }
          _pruneShares();
        });
      case 'voice_states':
        setState(() {
          _voiceChannel.clear();
          for (final st in ev.states) {
            if (st.joined) _voiceChannel[st.userId] = st.channelId;
          }
          for (final st in ev.states) {
            if (st.userId == _session?.userId) {
              _joinedChannelId = st.joined ? st.channelId : null;
              _voiceUp = st.joined;
            }
          }
          _pruneShares();
        });
      case 'video_started':
        final s0 = ev.surface;
        if (s0 != null) {
          // A share starts silent. Watching it (the card, fullscreen)
          // turns its sound on; the sharer's voice is unaffected.
          _setShareAudio(s0.userId, _watching == s0.userId);
          setState(() {
            // FRB maps u64 → BigInt and i64 → PlatformInt64: narrow
            // for display (dev-client resolutions fit int).
            _videos[s0.userId] = (
              s0.textureId,
              s0.width.toInt(),
              s0.height.toInt(),
            );
            final name = _nameOf(s0.userId);
            _status = '$name is sharing ${s0.width}×${s0.height}';
          });
        }
      case 'video_stopped':
        final s1 = ev.surface;
        if (s1 != null) {
          _setShareAudio(s1.userId, false);
          setState(() {
            _videos.remove(s1.userId);
            if (_watching == s1.userId) _watching = null;
            if (_fullscreen == s1.userId) {
              _fullscreen = null;
              setWindowFullscreen(false);
            }
          });
        }
      case 'share_started':
        if (mounted) {
          setState(() => _sharing = true);
          _status = 'sharing this screen';
        }
      case 'share_ended':
        if (mounted) {
          setState(() => _sharing = false);
          _status = 'share ended';
        }
      case 'share_failed':
        if (mounted) {
          setState(() => _sharing = false);
          _status = 'share failed: ${ev.username}';
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

  String _nameOf(int userId) => _usernames[userId] ?? 'user $userId';

  /// Display name for the connected account: presence is the authority
  /// once it arrives, the login field before that.
  String get _ownName {
    final id = _session?.userId;
    if (id == null) return '';
    return _usernames[id] ?? (_user.text.isEmpty ? 'user $id' : _user.text);
  }

  /// Users in voice channel [channelId], in a stable order (self first,
  /// then alphabetical — the roster should not jump around on refresh).
  List<int> _membersOf(BigInt channelId) {
    final ids = [
      for (final e in _voiceChannel.entries)
        if (e.value == channelId) e.key,
    ];
    ids.sort((a, b) {
      if (a == _session?.userId) return -1;
      if (b == _session?.userId) return 1;
      return _nameOf(a).toLowerCase().compareTo(_nameOf(b).toLowerCase());
    });
    return ids;
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final fullscreenId =
        (_fullscreen != null && _videos.containsKey(_fullscreen))
        ? _fullscreen
        : null;
    // Fullscreen is its own page, not a layer over the session view: no
    // app bar, no floating share/HW bar, no padding, no window toolbar.
    // The GTK window is in real fullscreen behind it.
    if (fullscreenId != null) {
      final v = _videos[fullscreenId]!;
      return Scaffold(
        backgroundColor: Colors.black,
        body: FullscreenShare(
          title: _nameOf(fullscreenId),
          textureId: v.$1,
          width: v.$2,
          height: v.$3,
          onExit: _exitFullscreen,
          onStopWatching: () {
            _exitFullscreen();
            _watch(null);
          },
        ),
      );
    }
    return Stack(
      children: [
        Scaffold(
          appBar: AppBar(
            title: const Text('Parley'),
            actions: [
              if (_session != null) ...[
                Text(
                  '$_ownName · id ${_session!.userId}',
                  style: const TextStyle(color: Colors.white70, fontSize: 13),
                ),
                IconButton(
                  tooltip: 'Disconnect',
                  icon: const Icon(Icons.logout, size: 20),
                  onPressed: _disconnect,
                ),
                const SizedBox(width: 8),
              ],
            ],
          ),
          body: Stack(
            children: [
              if (_session == null)
                LoginView(
                  server: _server,
                  user: _user,
                  password: _password,
                  connecting: _connecting,
                  status: _status,
                  onConnect: _connect,
                )
              else
                _sessionView(theme),
            ],
          ),
          floatingActionButton: _session == null ? null : _actionBar(theme),
          floatingActionButtonLocation: FloatingActionButtonLocation.centerFloat,
        ),
        // The native title bar is gone; this is the replacement. It is
        // drawn over the app bar only while the pointer is at the top.
        const WindowToolbar(),
      ],
    );
  }

  // --- connected layout: channels on the left, chat on the right, and a
  // share pushes in ABOVE the chat (the chat keeps its width and moves
  // down rather than the whole page re-splitting).

  Widget _sessionView(ThemeData theme) {
    return Padding(
      // Bottom room for the floating share/mute/HW bar.
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 80),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _connectionStrip(theme),
          const SizedBox(height: 12),
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Expanded(flex: 4, child: _channelsPanel(theme)),
                const SizedBox(width: 16),
                Expanded(flex: 6, child: _rightPane(theme)),
              ],
            ),
          ),
        ],
      ),
    );
  }

  /// The right-hand split: the share (when there is one) on top, the
  /// chat below it. With no share the chat simply owns the pane.
  Widget _rightPane(ThemeData theme) {
    final sharing = _videos.isNotEmpty;
    return Column(
      children: [
        if (sharing) ...[
          Expanded(flex: 3, child: _sharePane(theme)),
          const SizedBox(height: 12),
        ],
        Expanded(flex: sharing ? 2 : 1, child: _chatPanel(theme)),
      ],
    );
  }

  Widget _connectionStrip(ThemeData theme) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Icon(
              _voiceUp && !_micMuted ? Icons.mic : Icons.mic_off,
              size: 16,
              color: _voiceUp && !_micMuted ? accentSoft : Colors.white38,
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                _status,
                style: const TextStyle(color: Colors.white70, fontSize: 13),
              ),
            ),
            if (_sharing) const Pill(text: 'sharing', color: Colors.greenAccent),
          ],
        ),
        const SizedBox(height: 4),
        SelectableText(
          'fingerprint: ${_session!.fingerprint}',
          style: const TextStyle(
            color: Colors.white38,
            fontSize: 11,
            fontFamily: 'monospace',
          ),
        ),
      ],
    );
  }

  // --- channels: one card per channel, with its live roster. The
  // column fills the left pane and scrolls on its own.

  Widget _channelsPanel(ThemeData theme) {
    final idle = [
      for (final u in _presence)
        if (!_voiceChannel.containsKey(u.userId)) u.userId,
    ];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Text('Channels', style: theme.textTheme.titleMedium),
            const SizedBox(width: 8),
            Text(
              '${_presence.length} online',
              style: const TextStyle(color: Colors.white38, fontSize: 12),
            ),
          ],
        ),
        const SizedBox(height: 8),
        Expanded(
          child: ListView(
            children: [
              for (final c in _channels) _channelCard(theme, c),
              if (idle.isNotEmpty) _idleCard(theme, idle),
            ],
          ),
        ),
      ],
    );
  }

  Widget _channelCard(ThemeData theme, ChannelInfo c) {
    final joined = _joinedChannelId == c.id;
    final members = _membersOf(c.id);
    final accent = theme.colorScheme.primary;
    return Container(
      margin: const EdgeInsets.only(bottom: 8),
      padding: const EdgeInsets.fromLTRB(12, 8, 8, 10),
      decoration: BoxDecoration(
        color: joined
            ? accent.withValues(alpha: 0.16)
            : Colors.white.withValues(alpha: 0.04),
        borderRadius: BorderRadius.circular(10),
        border: Border.all(
          color: joined ? accent : Colors.white.withValues(alpha: 0.10),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(
                joined
                    ? Icons.graphic_eq
                    : (c.mode == 'music' ? Icons.music_note : Icons.mic),
                size: 18,
                color: joined ? accent : Colors.white54,
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  c.name,
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
              ),
              Text(
                members.isEmpty
                    ? 'voice empty'
                    : '${members.length} in voice',
                style: const TextStyle(color: Colors.white38, fontSize: 12),
              ),
              const SizedBox(width: 4),
              joined
                  ? TextButton(
                      onPressed: _leaveChannel,
                      child: const Text('Leave'),
                    )
                  : TextButton(
                      onPressed: () => _joinChannel(c),
                      child: const Text('Join'),
                    ),
            ],
          ),
          if (members.isNotEmpty)
            Padding(
              padding: const EdgeInsets.only(left: 26, top: 4),
              child: Wrap(
                spacing: 6,
                runSpacing: 6,
                children: [for (final id in members) _memberChip(id)],
              ),
            ),
        ],
      ),
    );
  }

  Widget _idleCard(ThemeData theme, List<int> ids) {
    return Container(
      margin: const EdgeInsets.only(bottom: 8),
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(10),
        border: Border.all(color: Colors.white.withValues(alpha: 0.06)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Row(
            children: [
              Icon(Icons.people_outline, size: 18, color: Colors.white38),
              SizedBox(width: 8),
              Text(
                'Online, not in a channel',
                style: TextStyle(color: Colors.white54, fontSize: 13),
              ),
            ],
          ),
          const SizedBox(height: 6),
          Padding(
            padding: const EdgeInsets.only(left: 26),
            child: Wrap(
              spacing: 6,
              runSpacing: 6,
              children: [for (final id in ids) _memberChip(id, idle: true)],
            ),
          ),
        ],
      ),
    );
  }

  Widget _memberChip(int userId, {bool idle = false}) {
    final self = userId == _session?.userId;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.06),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: 6,
            height: 6,
            decoration: BoxDecoration(
              shape: BoxShape.circle,
              color: idle
                  ? Colors.white24
                  : (self ? accentSoft : Colors.greenAccent),
            ),
          ),
          const SizedBox(width: 6),
          Text(
            self ? '${_nameOf(userId)} (you)' : _nameOf(userId),
            style: const TextStyle(fontSize: 12),
          ),
        ],
      ),
    );
  }

  // --- chat

  Widget _chatPanel(ThemeData theme) {
    final joinedName = _joinedChannelId == null
        ? null
        : _channels
              .where((c) => c.id == _joinedChannelId)
              .map((c) => c.name)
              .firstOrNull;
    return Container(
      decoration: BoxDecoration(
        border: Border.all(color: Colors.white24),
        borderRadius: BorderRadius.circular(10),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 10, 12, 6),
            child: Text(
              joinedName == null ? 'Chat' : 'Chat · $joinedName',
              style: theme.textTheme.titleSmall,
            ),
          ),
          const Divider(height: 1),
          Expanded(
            child: _joinedChannelId == null
                ? const Center(
                    child: Text(
                      'Join a channel to chat.',
                      style: TextStyle(color: Colors.white38),
                    ),
                  )
                : ListView.builder(
                    controller: _chatScroll,
                    padding: const EdgeInsets.all(10),
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
                                  color: own ? accentSoft : Colors.white70,
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
          if (_joinedChannelId != null) ...[
            const Divider(height: 1),
            Padding(
              padding: const EdgeInsets.all(8),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      controller: _chatInput,
                      onSubmitted: (_) => _sendChat(),
                      decoration: const InputDecoration(
                        labelText: 'Message',
                        isDense: true,
                        border: OutlineInputBorder(),
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  ElevatedButton(onPressed: _sendChat, child: const Text('Send')),
                ],
              ),
            ),
          ],
        ],
      ),
    );
  }

  // --- share pane (right side): an invitation until "Watch".

  Widget _sharePane(ThemeData theme) {
    final watching =
        _watching != null && _videos.containsKey(_watching) ? _watching : null;
    return Container(
      decoration: BoxDecoration(
        color: const Color(0xFF0B0B0D),
        borderRadius: BorderRadius.circular(10),
        border: Border.all(color: Colors.white24),
      ),
      child: watching == null
          ? _shareInvitations(theme)
          : _shareView(theme, watching),
    );
  }

  Widget _shareInvitations(ThemeData theme) {
    final ids = _videos.keys.toList()
      ..sort((a, b) => _nameOf(a).compareTo(_nameOf(b)));
    return ListView(
      padding: const EdgeInsets.all(12),
      children: [
        const Text(
          'Screens',
          style: TextStyle(color: Colors.white54, fontSize: 12),
        ),
        const SizedBox(height: 8),
        for (final id in ids) _shareCard(theme, id),
      ],
    );
  }

  Widget _shareCard(ThemeData theme, int userId) {
    final v = _videos[userId]!;
    return Container(
      margin: const EdgeInsets.only(bottom: 8),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.05),
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: Colors.white.withValues(alpha: 0.10)),
      ),
      child: Row(
        children: [
          const Icon(Icons.screen_share, color: accentSoft),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  '${_nameOf(userId)} is sharing',
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
                Text(
                  '${v.$2}×${v.$3}',
                  style: const TextStyle(color: Colors.white38, fontSize: 12),
                ),
              ],
            ),
          ),
          FilledButton.tonal(
            onPressed: () => _watch(userId),
            child: const Text('Watch'),
          ),
        ],
      ),
    );
  }

  Widget _shareView(ThemeData theme, int userId) {
    final v = _videos[userId]!;
    final texture = Texture(textureId: v.$1);
    return Column(
      children: [
        Expanded(
          child: Padding(
            padding: const EdgeInsets.all(8),
            child: Center(
              child: AspectRatio(
                aspectRatio: v.$2 / v.$3,
                child: texture,
              ),
            ),
          ),
        ),
        const Divider(height: 1),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 4, 4, 4),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  '${_nameOf(userId)} · ${v.$2}×${v.$3}',
                  style: const TextStyle(color: Colors.white70, fontSize: 12),
                ),
              ),
              IconButton(
                tooltip: 'Fullscreen',
                icon: const Icon(Icons.fullscreen, size: 20),
                onPressed: () => _enterFullscreen(userId),
              ),
              IconButton(
                tooltip: 'Stop watching',
                icon: const Icon(Icons.visibility_off, size: 20),
                onPressed: () => _watch(null),
              ),
            ],
          ),
        ),
      ],
    );
  }

  Future<void> _toggleMic() async {
    final sessionId = _session?.sessionId;
    if (sessionId == null) return;
    final next = !_micMuted;
    try {
      await setMicMuted(sessionId: sessionId, muted: next);
    } catch (e) {
      if (mounted) setState(() => _status = 'mute failed: $e');
      return;
    }
    if (!mounted) return;
    setState(() {
      _micMuted = next;
      _status = next ? 'mic muted' : 'mic live';
    });
  }

  // --- floating bottom bar: mute + share + hardware encode.

  Widget _actionBar(ThemeData theme) {
    final inVoice = _joinedChannelId != null && !_connecting;
    return Material(
      elevation: 10,
      color: theme.colorScheme.surfaceContainerHigh,
      shape: const StadiumBorder(),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextButton.icon(
              onPressed: inVoice ? _toggleMic : null,
              icon: Icon(_micMuted ? Icons.mic_off : Icons.mic, size: 20),
              label: Text(_micMuted ? 'Unmute' : 'Mute'),
            ),
            Container(
              width: 1,
              height: 24,
              color: Colors.white.withValues(alpha: 0.12),
            ),
            TextButton.icon(
              onPressed: inVoice
                  ? (_sharing ? _stopShare : _startShare)
                  : null,
              icon: Icon(
                _sharing ? Icons.stop_screen_share : Icons.screen_share,
                size: 20,
              ),
              label: Text(_sharing ? 'Stop sharing' : 'Share screen'),
            ),
            if (_hwEncodeSupported) ...[
              Container(
                width: 1,
                height: 24,
                color: Colors.white.withValues(alpha: 0.12),
              ),
              const SizedBox(width: 6),
              const Text('HW', style: TextStyle(fontSize: 12)),
              Switch(
                value: _hwEncode,
                onChanged: (v) async {
                  final now = await setHwEncode(enable: v);
                  setState(() => _hwEncode = now);
                  _status = v
                      ? 'hw encode: ON (next share; if the GPU wedges, this is why)'
                      : 'hw encode: off (software)';
                },
              ),
            ],
          ],
        ),
      ),
    );
  }
}
