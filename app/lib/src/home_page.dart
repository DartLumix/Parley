import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';

import 'rust/api/login.dart' as login_api;
import 'rust/api/session.dart';
import 'rust/api/settings.dart' as settings_api;
import 'rust/api/video.dart' as video_api;
import 'theme.dart';
import 'widgets/fullscreen_share.dart';
import 'widgets/login_view.dart';
import 'widgets/pill.dart';
import 'widgets/screens_panel.dart';
import 'widgets/share_dialog.dart';
import 'widgets/settings_page.dart';
import 'window.dart';

/// Whether a `voice_state` event is a real membership CHANGE in the
/// channel this client is in — the only thing the join/leave cue is for.
///
/// The event carries three different kinds of news: somebody joining a
/// channel, somebody leaving one, and somebody's mute flag flipping
/// (the server broadcasts `SetMute` as a `VoiceState` with `joined: true`
/// and the member's current channel). A poll or a re-announce repeats a
/// membership that never moved. So the cue is driven by the transition,
/// not by the event: the same channel in and out means nothing happened,
/// and muting must be silent.
bool isVoiceMembershipCue({
  required int userId,
  required int? me,
  required BigInt? myChannel,
  required BigInt? channelBefore,
  required bool joined,
  required BigInt eventChannel,
}) {
  if (me == null || myChannel == null || userId == me) return false;
  final wasMine = channelBefore == myChannel;
  final isMine = joined && eventChannel == myChannel;
  return isMine != wasMine;
}

/// The clamp bounds for the left column (channels + rosters).
///
/// The column is measured to fit its longest name, but a name fits in a
/// couple of hundred pixels and the roster reads like a squeezed strip
/// at that width — so the floor sits near twice the old 220 px default.
/// The ceiling keeps a long handle from eating the screens area, and
/// both stay ordered even on the first frame, when the window can still
/// be a few pixels wide (`clamp` throws on an inverted range).
(double, double) channelPanelBounds(double available) {
  final upper = math.max(160.0, math.min(560.0, available * 0.46));
  // The floor follows the window down: a 900 px window gives the column
  // a third of itself, not half, so the screens area never becomes the
  // sliver on the small windows where it matters most.
  final lower = math.min(math.max(160.0, available * 0.38), math.min(420.0, upper));
  return (lower, upper);
}

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

  /// The remembered session (Rust: ~/.config/parley/session.toml). The
  /// token is the credential — the password is never written down. Set at
  /// startup, refreshed on every successful login, cleared only by the
  /// Disconnect button.
  login_api.SavedLogin? _saved;

  /// The credential this attempt is using. The trust prompt's retry must
  /// repeat the SAME one: after a token login the password field is empty,
  /// so retrying with a password would fail for the wrong reason.
  String? _connectToken;

  /// Per-user output volume and the extra trim on their screen-share
  /// audio (both 1.0 = untouched). Session-scoped, like the mixer.
  final Map<int, double> _volume = {};
  final Map<int, double> _shareVolume = {};

  /// Speaking indicator: decayed peaks polled from the Rust mixer while
  /// in voice, keyed by user id. Missing = silent.
  final Map<int, double> _levels = {};
  double _selfLevel = 0;
  Timer? _meter;

  /// Users whose microphone the server says is muted (SetMute), from the
  /// snapshot and every broadcast since.
  final Set<int> _mutedUsers = {};

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
  // 7f: active remote shares — user_id → (texture_id, w, h, fps).
  // Driven by the video_started / video_stats / video_stopped events;
  // textures are owned by the Rust bridge (one per sender, stable
  // across stop/re-share).
  final Map<int, (int, int, int, int)> _videos = {};
  // Shares the screens area has OPEN, in the order they were opened.
  // Several may be open: the big area splits between them and the rest
  // wait in a strip. Every open share is audible (their system audio is
  // a separate mixer stream), so closing one mutes only that one.
  final List<int> _watched = [];
  // Fullscreen viewer (a whole-page mode, not a route: a share ending
  // has to close it from the same state that receives the event). The
  // GTK window goes genuinely fullscreen alongside it.
  int? _fullscreen;
  // 7g: our own share state (start/stop button).
  bool _sharing = false;
  bool _hwEncodeSupported = false;
  // Persisted preferences (Rust: ~/.config/parley/settings.toml). The
  // share dialog's defaults and the preview switch read from here; the
  // hardware-encode toggle moved to the settings page.
  settings_api.AppSettings _settings = const settings_api.AppSettings(
    hwEncode: false,
    showSharePreview: true,
    shareHeight: 0,
    shareFps: 0,
    echoCancel: true,
  );
  // The sharer's own preview texture (Rust creates it once per process).
  video_api.SharePreview? _preview;
  String? _previewError;
  // The running share's real shape (capture pixels + paced rate), read
  // from the capture thread at 1 Hz while sharing. Only used for the
  // card's "1920×1080 · 30 fps" line.
  video_api.ShareStats? _shareStats;
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
    // ~10 Hz is what a speaking glow needs; the Rust side just reads the
    // peaks its audio callback already computed.
    _meter = Timer.periodic(
      const Duration(milliseconds: 100),
      (_) => _pollLevels(),
    );
    // Coming back up is the normal case: closing the window is not signing
    // out, so the last server is re-entered without typing anything.
    _restoreLogin();
    // Persisted preferences. Best-effort: a featureless build reports no
    // hardware encoder, and the preview is simply unavailable.
    _loadSettings();
  }

  /// Re-enter the last session. The token comes from the Rust side, so no
  /// password is needed and none is stored; a refused token drops the saved
  /// login and leaves the ordinary form in place.
  Future<void> _restoreLogin() async {
    login_api.SavedLogin? saved;
    try {
      saved = await login_api.load();
    } catch (e) {
      debugPrint('login: restore failed: $e');
      return;
    }
    if (saved == null || !mounted) return;
    final remembered = saved;
    _server.text = remembered.server;
    _user.text = remembered.user;
    setState(() {
      _saved = remembered;
      _status = 'reconnecting to ${remembered.server}\u2026';
    });
    await _connect(token: remembered.token);
  }

  @override
  void dispose() {
    // Deliberately NO disconnect here: closing the window leaves the
    // account signed in (the token stays valid), which is what makes the
    // next launch reconnect on its own. The QUIC connection dies with the
    // process either way; only the Disconnect button forgets the login.
    windowChannel.setMethodCallHandler(null);
    _liveness.cancel();
    _meter?.cancel();
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
    // The own-share card's size/rate live on the capture thread; a 1 Hz
    // read is enough and costs nothing on the media path.
    if (_sharing) _loadShareStats();
  }

  /// Read the running share's negotiated shape (capture pixels + paced
  /// fps). Best-effort: a build without capture reports null and the
  /// card falls back to "your screen".
  Future<void> _loadShareStats() async {
    try {
      final stats = await video_api.shareStats();
      if (!mounted) return;
      setState(() => _shareStats = stats);
    } catch (e) {
      debugPrint('share stats: $e');
    }
  }

  int _pollTick = 0;

  /// One outstanding meter poll at a time: the timer does not wait, and a
  /// slow FFI round trip must not queue a second.
  bool _polling = false;

  /// Pull the current speaking peaks (and our own mic peak) and decay
  /// them, so a single silent poll fades the glow instead of blinking it
  /// off. Rebuilds only when a level actually moved.
  Future<void> _pollLevels() async {
    if (_polling) return;
    final s = _session;
    if (s == null || !_voiceUp) {
      if (_levels.isNotEmpty || _selfLevel != 0) {
        setState(() {
          _levels.clear();
          _selfLevel = 0;
        });
      }
      return;
    }
    _polling = true;
    final List<VoiceLevel> rows;
    final double self;
    try {
      rows = await voiceLevels(sessionId: s.sessionId);
      self = await micLevel(sessionId: s.sessionId);
    } catch (_) {
      return;
    } finally {
      _polling = false;
    }
    if (!mounted || _session?.sessionId != s.sessionId) return;

    const decay = 0.55;
    const floor = 0.03;
    final peaks = {for (final r in rows) r.userId: r.voice};
    final next = <int, double>{};
    for (final id in {..._levels.keys, ...peaks.keys}) {
      final peak = (peaks[id] ?? 0).clamp(0.0, 1.0).toDouble();
      final faded = (_levels[id] ?? 0) * decay;
      final v = peak > faded ? peak : faded;
      if (v > floor) next[id] = v;
    }
    final selfPeak = self.clamp(0.0, 1.0).toDouble();
    final selfFaded = _selfLevel * decay;
    final selfNext = selfPeak > selfFaded ? selfPeak : selfFaded;

    var changed = next.length != _levels.length;
    if (!changed) {
      for (final e in next.entries) {
        if (((_levels[e.key] ?? 0) - e.value).abs() > 0.04) {
          changed = true;
          break;
        }
      }
    }
    if (!changed && (selfNext - _selfLevel).abs() > 0.04) changed = true;
    if (!changed) return;
    setState(() {
      _levels
        ..clear()
        ..addAll(next);
      _selfLevel = selfNext > floor ? selfNext : 0;
    });
  }

  Future<void> _applyVolume(int userId, double gain) async {
    final sessionId = _session?.sessionId;
    if (sessionId == null) return;
    setState(() => _volume[userId] = gain);
    try {
      await setUserVolume(sessionId: sessionId, userId: userId, gain: gain);
    } catch (e) {
      debugPrint('volume: $e');
    }
  }

  Future<void> _applyShareVolume(int userId, double gain) async {
    final sessionId = _session?.sessionId;
    if (sessionId == null) return;
    setState(() => _shareVolume[userId] = gain);
    try {
      await setShareVolume(sessionId: sessionId, userId: userId, gain: gain);
    } catch (e) {
      debugPrint('share volume: $e');
    }
  }

  /// Per-user volume (their voice, their share) and — for someone who is
  /// sharing — the trim on that share's system audio. Two sliders because
  /// they are two independent streams in the mixer.
  Future<void> _showVolumeDialog(int userId) async {
    var master = _volume[userId] ?? 1.0;
    var share = _shareVolume[userId] ?? 1.0;
    final self = userId == _session?.userId;
    final sharing =
        _videos.containsKey(userId) || _shareVolume.containsKey(userId);
    await showDialog<void>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          title: Text(_nameOf(userId)),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              _volumeSlider('Voice', master, (v) {
                setDialogState(() => master = v);
                _applyVolume(userId, v);
              }),
              if (sharing && !self)
                _volumeSlider('Screen share audio', share, (v) {
                  setDialogState(() => share = v);
                  _applyShareVolume(userId, v);
                }),
              const Text(
                'The screen-share bar trims only that share\u2019s system '
                'audio; their voice keeps its own level.',
                style: TextStyle(color: Colors.white54, fontSize: 11),
              ),
            ],
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Done'),
            ),
          ],
        ),
      ),
    );
  }

  Widget _volumeSlider(
    String label,
    double value,
    ValueChanged<double> onChanged,
  ) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Text(
              label,
              style: const TextStyle(color: Colors.white70, fontSize: 12),
            ),
            const Spacer(),
            Text(
              '${(value * 100).round()}%',
              style: const TextStyle(
                color: Colors.white54,
                fontSize: 12,
                fontFamily: 'monospace',
              ),
            ),
          ],
        ),
        Slider(value: value, min: 0, max: 2, onChanged: onChanged),
      ],
    );
  }

  void _handleClosed() {
    _events?.cancel();
    _events = null;
    setState(() {
      _session = null;
      // Not signed out: the saved token is what lets the Reconnect button
      // (or the next launch) get back in without a password.
      _status = 'connection lost — press Reconnect';
      _joinedChannelId = null;
      _voiceUp = false;
      _channels.clear();
      _presence.clear();
      _chat.clear();
      _usernames.clear();
      _videos.clear();
      _voiceChannel.clear();
      _watched.clear();
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
      _levels.clear();
      _selfLevel = 0;
      _mutedUsers.clear();
      _shareStats = null;
    });
    setWindowFullscreen(false);
    _muteAllShares();
    _syncScreenWatch();
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

  /// Prefix of the Rust-side refusal when a server's certificate no longer
  /// matches its pin (`cert::mismatch_report`). The trust prompt hangs off
  /// it, so the two sides move together.
  static const _pinMismatch = 'fingerprint mismatch for';

  Future<void> _connect({bool offerTrust = true, String? token}) async {
    _connectToken = token;
    setState(() => _connecting = true);
    try {
      final s = token != null
          ? await connectWithToken(
              server: _server.text,
              user: _user.text,
              token: token,
            )
          : await connect(
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
      // Every login returns a fresh token: remember this one, so the next
      // launch (and every reconnect) uses it instead of the password.
      await _rememberLogin(s);
      // Bootstrap AFTER subscribing (deterministic ordering — the Rust
      // side no longer issues these itself).
      requestPresence(sessionId: s.sessionId);
      listChannels(sessionId: s.sessionId);
      // Who was already sitting in a channel before we connected —
      // no broadcast replays that, only the snapshot has it.
      requestVoiceStates(sessionId: s.sessionId);
    } catch (e) {
      if (mounted) setState(() => _status = 'connect failed: $e');
      // A saved token the server no longer accepts is not an error the
      // user can fix by retyping the password in a hidden field: forget it
      // and say so. The server/user stay prefilled.
      if (token != null && '$e'.contains('auth denied')) {
        await _forgetLogin();
        if (mounted) {
          setState(() => _status = 'saved session expired — sign in again');
        }
      }
      // A changed certificate is a decision, not an error to retype around.
      if (offerTrust && '$e'.contains(_pinMismatch) && mounted) {
        await _offerTrust();
      }
    } finally {
      if (mounted) setState(() => _connecting = false);
    }
  }

  /// Write the token this login returned. Best-effort: a read-only config
  /// directory must not break a working session, it only costs the
  /// auto-reconnect.
  Future<void> _rememberLogin(SessionInfo s) async {
    final server = _server.text.trim();
    final user = _user.text.trim();
    try {
      await login_api.save(server: server, user: user, token: s.sessionToken);
    } catch (e) {
      debugPrint('login: save failed: $e');
      return;
    }
    // The token is the credential now; keeping the password in a live
    // text field (and in memory for the session's lifetime) is pointless.
    _password.clear();
    if (!mounted) return;
    setState(() {
      _saved = login_api.SavedLogin(
        server: server,
        user: user,
        token: s.sessionToken,
      );
    });
  }

  Future<void> _forgetLogin() async {
    try {
      await login_api.forget();
    } catch (e) {
      debugPrint('login: forget failed: $e');
    }
    _saved = null;
  }

  /// TOFU refused the connection: show both digests and let the user decide.
  /// Nothing is written unless they explicitly trust the new certificate;
  /// only then is the pin replaced and the login retried.
  Future<void> _offerTrust() async {
    final server = _server.text.trim();
    final ServerCertificate cert;
    try {
      // Read what is on the wire NOW (TLS only, no credentials) instead of
      // the stale text of the failure that got us here.
      cert = await probeServer(server: server);
    } catch (e) {
      if (mounted) setState(() => _status = 'connect failed: $e');
      return;
    }
    if (!mounted || cert.matches) return; // pin was repaired meanwhile
    final trust = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Server certificate changed'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              '${cert.server} presented a certificate that does not match '
              'the one saved on this machine.',
            ),
            const SizedBox(height: 12),
            _fingerprintRow('saved', cert.pinned ?? '—'),
            _fingerprintRow('now', cert.seen),
            const SizedBox(height: 12),
            const Text(
              'That is expected after a redeploy with a new identity. Compare '
              'the second fingerprint with the one the server prints at '
              'startup. If it differs, someone else is on that address — '
              'cancel and do not sign in.',
              style: TextStyle(color: Colors.white70, fontSize: 12),
            ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Trust new certificate'),
          ),
        ],
      ),
    );
    if (trust != true || !mounted) return;
    try {
      final pinned = await trustServer(server: server);
      setState(() => _status = 'trusted $pinned');
      await _connect(offerTrust: false, token: _connectToken);
    } catch (e) {
      if (mounted) setState(() => _status = 'trust failed: $e');
    }
  }

  Widget _fingerprintRow(String label, String value) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 52,
            child: Text(
              label,
              style: const TextStyle(color: Colors.white54, fontSize: 12),
            ),
          ),
          Expanded(
            child: SelectableText(
              value,
              style: const TextStyle(fontFamily: 'monospace', fontSize: 11),
            ),
          ),
        ],
      ),
    );
  }

  /// Disconnect = sign out: the saved token is deleted, so the next launch
  /// shows the login form instead of re-entering the account.
  Future<void> _disconnect() async {
    if (_session == null) return;
    await _muteAllShares();
    await _forgetLogin();
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
      _watched.clear();
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
      _levels.clear();
      _selfLevel = 0;
      _mutedUsers.clear();
      _volume.clear();
      _shareVolume.clear();
      _shareStats = null;
    });
    setWindowFullscreen(false);
    _syncScreenWatch();
  }

  // --- fullscreen: a real window mode, with only the two controls the
  // viewer needs (exit, stop watching) over the picture.

  /// Fullscreen the share that is ALREADY open.
  ///
  /// It must not go through `_watch`: that is a toggle, and the tile's
  /// fullscreen button only exists on an open tile — so toggling there
  /// CLOSED the share on the way in (it left the watched set, the
  /// display bridge stopped converting and uploading frames, and its
  /// system audio was muted) and left the fullscreen page painting the
  /// one frame it had: the picture froze until the viewer was closed and
  /// the tile watched again. Watching is idempotent here; the picture
  /// simply follows the same texture into the fullscreen page.
  ///
  /// Our own share is the other half of the button: it has no received
  /// texture and no watch state, but the capture preview it draws is a
  /// picture like any other, so it gets the same page.
  void _enterFullscreen(int userId) {
    if (userId == _session?.userId) {
      unawaited(_enterOwnFullscreen());
      return;
    }
    if (!_videos.containsKey(userId)) return;
    _ensureWatched(userId);
    setState(() => _fullscreen = userId);
    setWindowFullscreen(true);
  }

  /// Fullscreen our own share. A share started with the preview switch
  /// off has no preview texture and the capture tap is not feeding one,
  /// so expanding turns the preview on (it is what the page shows) and
  /// the viewer opens on its first frame.
  Future<void> _enterOwnFullscreen() async {
    final me = _session?.userId;
    if (me == null || !_sharing) return;
    if (!_settings.showSharePreview) {
      await _setPreviewEnabled(true);
    } else if (_preview == null) {
      await _loadPreview();
    }
    if (!mounted || _preview == null) return;
    setState(() => _fullscreen = me);
    setWindowFullscreen(true);
  }

  /// What the fullscreen page shows, or null when there is nothing to
  /// paint. Our own share draws the capture preview on a 16:9 canvas; a
  /// remote share keeps its own aspect. Nothing is returned for a share
  /// whose preview is off or still opening, so the window leaves
  /// fullscreen instead of showing black.
  ({
    int userId,
    String title,
    int textureId,
    int width,
    int height,
    double aspectRatio,
    bool own,
  })?
  _fullscreenView() {
    final id = _fullscreen;
    final me = _session?.userId;
    if (id == null || me == null) return null;
    if (id == me) {
      final preview = _preview;
      if (!_sharing || preview == null) return null;
      // The preview canvas follows the stream (it is the capture at the
      // encoder's size), so the page uses the STREAM's shape — a 21:9
      // screen filled the page's 16:9 box with bars before.
      final w = _shareStats?.width.toInt() ?? preview.width.toInt();
      final h = _shareStats?.height.toInt() ?? preview.height.toInt();
      return (
        userId: id,
        title: '${_nameOf(id)} (you)',
        textureId: preview.textureId,
        width: w,
        height: h,
        aspectRatio: w > 0 && h > 0 ? w / h : 16 / 9,
        own: true,
      );
    }
    final v = _videos[id];
    if (v == null) return null;
    return (
      userId: id,
      title: _nameOf(id),
      textureId: v.$1,
      width: v.$2,
      height: v.$3,
      aspectRatio: v.$2 > 0 && v.$3 > 0 ? v.$2 / v.$3 : 16 / 9,
      own: false,
    );
  }

  /// Open one share without the toggle semantics of [_watch]. Used by the
  /// fullscreen viewer, which opens a tile that is already open.
  void _ensureWatched(int userId) {
    if (_watched.contains(userId)) return;
    setState(() => _watched.add(userId));
    _syncScreenWatch();
    if (userId != _session?.userId) _setShareAudio(userId, true);
  }

  /// Open (or close) one share in the big area. A screen share's system
  /// audio is silent until its card is opened: hearing someone's music
  /// the instant they start sharing is not the same thing as watching
  /// the share. Their VOICE is a separate stream and keeps coming
  /// through either way.
  void _watch(int userId) {
    if (_watched.contains(userId)) {
      _unwatch(userId);
      return;
    }
    _ensureWatched(userId);
  }

  /// Close one open share without touching the others.
  void _unwatch(int userId) {
    if (!_watched.remove(userId)) return;
    setState(() {});
    _syncScreenWatch();
    if (userId != _session?.userId) _setShareAudio(userId, false);
    if (_fullscreen == userId) {
      _fullscreen = null;
      setWindowFullscreen(false);
    }
  }


  /// Sharers whose share audio the Rust side is currently playing —
  /// muted again on disconnect so a later session starts silent.
  final Set<int> _audibleShares = {};

  /// The shares whose picture the Rust display bridge has been told to
  /// paint. Only an OPEN tile is on this list: a closed one costs the
  /// bridge nothing — no RGB conversion, no texture upload, no raster
  /// hand-off — because nothing renders it.
  final Set<int> _rustWatched = {};

  /// Push the open-share set to the display bridge (a delta, so a
  /// rebuild never re-sends the same state). Our own share is skipped:
  /// its picture is the capture preview, not a received texture.
  void _syncScreenWatch() {
    final sessionId = _session?.sessionId;
    if (sessionId == null) {
      _rustWatched.clear();
      return;
    }
    final me = _session?.userId;
    final want = {
      for (final id in _watched)
        if (id != me) id,
    };
    for (final id in _rustWatched.difference(want).toList()) {
      _rustWatched.remove(id);
      unawaited(
        setScreenWatched(sessionId: sessionId, userId: id, watched: false)
            .catchError((_) {}),
      );
    }
    for (final id in want.difference(_rustWatched)) {
      _rustWatched.add(id);
      unawaited(
        setScreenWatched(sessionId: sessionId, userId: id, watched: true)
            .catchError((_) {}),
      );
    }
  }

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
        await setShareAudio(sessionId: sessionId, userId: id, audible: false);
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
    final me = _session?.userId;
    if (_videos.isEmpty && !_sharing) return;
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
    // Own share: it lives in `_sharing`, not `_videos`, so "still on
    // screen" is a different question for it.
    bool visible(int id) => id == me ? _sharing : _videos.containsKey(id);
    _watched.removeWhere((id) => !visible(id));
    _syncScreenWatch();
    for (final id in _audibleShares.toList()) {
      if (!visible(id)) _setShareAudio(id, false);
    }
    if (_fullscreen != null && _fullscreenView() == null) {
      _fullscreen = null;
      setWindowFullscreen(false);
    }
  }

  Future<void> _loadSettings() async {
    try {
      final loaded = await settings_api.load();
      final supported = await hwEncodeSupported();
      if (!mounted) return;
      setState(() {
        _settings = loaded;
        _hwEncodeSupported = supported;
      });
    } catch (e) {
      debugPrint('settings: load failed: $e');
    }
  }

  /// Persist one preference change. The Rust side writes the file and
  /// applies what it can immediately (the preview switch applies to a
  /// running share); the rest lands on the next share or voice join.
  Future<void> _saveSettings(settings_api.AppSettings next) async {
    final prev = _settings;
    setState(() => _settings = next);
    try {
      await settings_api.save(settings: next);
    } catch (e) {
      if (!mounted) return;
      setState(() {
        _settings = prev;
        _status = 'could not save settings: $e';
      });
    }
  }

  Future<void> _openSettings() async {
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => SettingsPage(
          settings: _settings,
          hwEncodeSupported: _hwEncodeSupported,
          server: _session?.server,
          user: _session == null ? null : _ownName,
          onSignOut: _session == null ? null : _disconnect,
        ),
      ),
    );
    // The page saves as it goes; re-read so a value applied there (and a
    // failed write) is reflected here too.
    if (mounted) await _loadSettings();
  }

  /// The sharer's own preview texture, created on first use and reused
  /// for every later share in this process.
  Future<void> _loadPreview() async {
    try {
      final preview = await video_api.sharePreview();
      if (!mounted) return;
      setState(() {
        _preview = preview;
        _previewError = null;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _previewError = 'preview unavailable: $e');
    }
  }

  /// Flip the preview switch on our own screen tile. Persisted like
  /// every preference, and it takes effect mid-share: the capture tap
  /// checks it before any frame is even handed to the preview thread.
  Future<void> _setPreviewEnabled(bool on) async {
    // Turning the preview off takes away the only picture our own
    // fullscreen page has: leave it first, then switch.
    if (!on && _fullscreen == _session?.userId) _exitFullscreen();
    await _saveSettings(
      settings_api.AppSettings(
        hwEncode: _settings.hwEncode,
        showSharePreview: on,
        shareHeight: _settings.shareHeight,
        shareFps: _settings.shareFps,
        echoCancel: _settings.echoCancel,
      ),
    );
    if (on && _preview == null && _sharing) await _loadPreview();
  }

  // 7g: share lifecycle. Start: options dialog → portal dialog
  // (compositor's picker); share_started/share_ended events flip the
  // button. FRB exposes the start as async (even though the Rust side
  // returns instantly) — the portal dialog blocks for seconds.
  Future<void> _startShare() async {
    final session = _session;
    if (session == null) return;
    final options = await showShareOptionsDialog(
      context,
      height: _settings.shareHeight,
      fps: _settings.shareFps,
    );
    if (options == null || !mounted || _session?.sessionId != session.sessionId) {
      return;
    }
    // The dialog is the same cap Settings → Video edits, so remember what
    // was actually picked: next time it should not snap back to a stale
    // value (a 720p60 choice that reverts to 1440p60 reads as a bug).
    if (options.height != _settings.shareHeight ||
        options.fps != _settings.shareFps) {
      unawaited(
        _saveSettings(
          settings_api.AppSettings(
            hwEncode: _settings.hwEncode,
            showSharePreview: _settings.showSharePreview,
            shareHeight: options.height,
            shareFps: options.fps,
            echoCancel: _settings.echoCancel,
          ),
        ),
      );
    }
    try {
      await startScreenShare(
        sessionId: session.sessionId,
        maxHeight: options.height,
        maxFps: options.fps,
      );
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
          // Mute state outlives leave/join, but not a session: forget ids
          // that are no longer online so a later account cannot inherit a
          // stale badge.
          if (ev.users.isNotEmpty) {
            final online = {for (final u in ev.users) u.userId};
            _mutedUsers.removeWhere((id) => !online.contains(id));
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
        // A cue for the people we are in a call with: somebody entered
        // or left THIS channel. Mute changes arrive as voice_state too,
        // and a poll or a reconnect re-announces members who never
        // moved, so only a real transition makes a sound.
        final mine = _joinedChannelId;
        if (isVoiceMembershipCue(
          userId: ev.userId,
          me: _session?.userId,
          myChannel: mine,
          channelBefore: _voiceChannel[ev.userId],
          joined: ev.joined,
          eventChannel: ev.channelId,
        )) {
          unawaited(playVoiceChime(join: ev.joined).catchError((_) {}));
        }
        setState(() {
          if (ev.joined) {
            _voiceChannel[ev.userId] = ev.channelId;
          } else {
            _voiceChannel.remove(ev.userId);
            _levels.remove(ev.userId);
          }
          // Announced mute state — the badge does not depend on the
          // silenced packets (a muted client sends none at all).
          if (ev.muted) {
            _mutedUsers.add(ev.userId);
          } else {
            _mutedUsers.remove(ev.userId);
          }
          if (ev.userId == _session?.userId) {
            // 7e: the own join/leave IS the voice lifecycle — the Rust
            // side already opened/closed the devices on this event.
            _joinedChannelId = ev.joined ? ev.channelId : null;
            _voiceUp = ev.joined;
            _levels.clear();
            _selfLevel = 0;
            _status = ev.joined
                ? 'in voice — mic + speakers live'
                : 'connected';
          }
          _pruneShares();
        });
      case 'voice_states':
        setState(() {
          _voiceChannel.clear();
          _mutedUsers.clear();
          for (final st in ev.states) {
            if (st.joined) _voiceChannel[st.userId] = st.channelId;
            if (st.muted) _mutedUsers.add(st.userId);
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
          // A share starts silent. Opening it (the card, fullscreen)
          // turns its sound on; the sharer's voice is unaffected.
          _setShareAudio(s0.userId, _watched.contains(s0.userId));
          setState(() {
            // FRB maps u64 → BigInt and i64 → PlatformInt64: narrow
            // for display (dev-client resolutions fit int).
            _videos[s0.userId] = (
              s0.textureId,
              s0.width.toInt(),
              s0.height.toInt(),
              s0.fps.toInt(),
            );
            final name = _nameOf(s0.userId);
            _status = '$name is sharing ${s0.width}×${s0.height}';
          });
        }
      case 'video_stats':
        // Same surface, new dimensions and/or a measured cadence. The
        // texture id is stable, so the card simply keeps rendering.
        final s2 = ev.surface;
        if (s2 != null) {
          final old = _videos[s2.userId];
          if (old != null) {
            final fps = s2.fps.toInt() > 0 ? s2.fps.toInt() : old.$4;
            setState(() {
              _videos[s2.userId] = (
                old.$1,
                s2.width.toInt(),
                s2.height.toInt(),
                fps,
              );
            });
          }
        }
      case 'video_stopped':
        final s1 = ev.surface;
        if (s1 != null) {
          _setShareAudio(s1.userId, false);
          setState(() {
            _videos.remove(s1.userId);
            _watched.remove(s1.userId);
            if (_fullscreen == s1.userId) {
              _fullscreen = null;
              setWindowFullscreen(false);
            }
            // The strip must not keep advertising a share that ended.
            if (_videos.isEmpty && !_sharing && _status.contains('sharing')) {
              _status = 'connected';
            }
          });
          // The bridge forgot this sender when the stop arrived; the
          // mirror has to as well, or a later share from the same user
          // would look watched and never be told to paint.
          _rustWatched.remove(s1.userId);
          _syncScreenWatch();
        }
      case 'share_started':
        if (mounted) {
          setState(() => _sharing = true);
          _status = 'sharing this screen';
          // The preview texture is fetched now, not at startup: a user
          // who never shares never pays for it.
          if (_settings.showSharePreview) _loadPreview();
          _loadShareStats();
        }
      case 'share_ended':
        if (mounted) {
          final ownFullscreen = _fullscreen == _session?.userId;
          setState(() {
            _sharing = false;
            _shareStats = null;
            _watched.remove(_session?.userId);
            if (ownFullscreen) _fullscreen = null;
          });
          // The page that was showing the preview has nothing left to
          // paint: put the window back before the picture disappears.
          if (ownFullscreen) setWindowFullscreen(false);
          _status = 'share ended';
        }
      case 'share_failed':
        if (mounted) {
          final ownFullscreen = _fullscreen == _session?.userId;
          setState(() {
            _sharing = false;
            _shareStats = null;
            _watched.remove(_session?.userId);
            if (ownFullscreen) _fullscreen = null;
          });
          if (ownFullscreen) setWindowFullscreen(false);
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
    // Fullscreen is its own page, not a layer over the session view: no
    // app bar, no floating share/HW bar, no padding, no window toolbar.
    // The GTK window is in real fullscreen behind it.
    final fs = _fullscreenView();
    if (fs != null) {
      return Scaffold(
        backgroundColor: Colors.black,
        body: FullscreenShare(
          title: fs.title,
          textureId: fs.textureId,
          width: fs.width,
          height: fs.height,
          aspectRatio: fs.aspectRatio,
          onExit: _exitFullscreen,
          // Our own share has no watch state to stop — the window is
          // showing the preview, not somebody else's stream.
          onStopWatching: fs.own
              ? null
              : () {
                  _exitFullscreen();
                  _unwatch(fs.userId);
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
              IconButton(
                tooltip: 'Settings',
                icon: const Icon(Icons.settings_outlined, size: 20),
                onPressed: _openSettings,
              ),
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
                  onConnect: () => _connect(),
                  reconnectLabel: _saved == null
                      ? null
                      : 'Reconnect as ${_saved!.user}',
                  onReconnect: _saved == null
                      ? null
                      : () => _connect(token: _saved!.token),
                )
              else
                _sessionView(theme),
            ],
          ),
          floatingActionButton: _session == null ? null : _actionBar(theme),
          floatingActionButtonLocation:
              FloatingActionButtonLocation.centerFloat,
        ),
        // The native title bar is gone; this is the replacement. It is
        // drawn over the app bar only while the pointer is at the top.
        const WindowToolbar(),
      ],
    );
  }

  // --- connected layout: channels on the left, screens + chat on the
  // right. The channel column is as wide as its longest name needs (see
  // [_channelPanelWidth]) so the rest of the window is screens and chat.

  Widget _sessionView(ThemeData theme) {
    return Padding(
      // Bottom room for the floating share/mute bar.
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 80),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _connectionStrip(theme),
          const SizedBox(height: 12),
          Expanded(
            child: LayoutBuilder(
              builder: (context, c) {
                final channels = _channelPanelWidth(theme, c.maxWidth);
                return Row(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    SizedBox(
                      key: const ValueKey('channels-column'),
                      width: channels,
                      child: _channelsPanel(theme),
                    ),
                    const SizedBox(width: 16),
                    Expanded(child: _rightPane(theme)),
                  ],
                );
              },
            ),
          ),
        ],
      ),
    );
  }

  /// The width the channel column needs: the longest name it will ever
  /// print, plus the chrome every row carries (icon, Join button,
  /// padding). Clamped so a long username cannot eat the screens area,
  /// and floored so the panel stays legible.
  ///
  /// The floor is deliberately generous — the roster is the one list a
  /// user reads to know who is in voice, and the same column holds the
  /// channel cards — so between a narrow measured width and a roomy one
  /// the panel takes the roomy one.
  double _channelPanelWidth(ThemeData theme, double available) {
    var needed = 0.0;
    void measure(String text, TextStyle style, double chrome) {
      final painter = TextPainter(
        text: TextSpan(text: text, style: style),
        maxLines: 1,
        textDirection: TextDirection.ltr,
      )..layout();
      needed = math.max(needed, painter.width + chrome);
      painter.dispose();
    }

    // Header: "Channels" + "N online".
    measure(
      'Channels',
      theme.textTheme.titleMedium ?? const TextStyle(fontSize: 16),
      90.0,
    );
    // A channel row: card padding (10+6), icon 18 + gap 8, the trailing
    // Join/Leave button 64 + gap.
    const channelChrome = 10.0 + 6 + 18 + 8 + 64 + 8;
    for (final c in _channels) {
      measure(
        c.name,
        const TextStyle(fontSize: 15, fontWeight: FontWeight.w600),
        channelChrome,
      );
    }
    // A roster row: padding 24, icon 15 + gap 9, the volume label 34,
    // plus breathing room so the longest name is not flush against the
    // edge (the speaker list is the one place a long handle must stay
    // readable).
    const rowChrome = 24.0 + 15 + 9 + 34 + 12;
    for (final u in _presence) {
      final name = u.userId == _session?.userId
          ? '${_nameOf(u.userId)} (you)'
          : _nameOf(u.userId);
      measure(name, const TextStyle(fontSize: 13), rowChrome);
    }
    // The idle card's title has only its own icon as chrome.
    measure('Online, not in a channel', const TextStyle(fontSize: 13), 46.0);
    final (lower, upper) = channelPanelBounds(available);
    return needed.clamp(lower, upper);
  }

  /// The right-hand split: the screens area (sized to its content) on
  /// top, the chat below it. With no screens the chat owns the pane.
  Widget _rightPane(ThemeData theme) {
    return LayoutBuilder(
      builder: (context, c) {
        final tiles = _screenTiles();
        if (tiles.isEmpty) return _chatPanel(theme);
        final ids = {for (final t in tiles) t.userId};
        final open = [
          for (final id in _watched)
            if (ids.contains(id)) id,
        ];
        final height = ScreensPanel.heightFor(
          tileCount: tiles.length,
          openCount: open.length,
          width: c.maxWidth,
          availableHeight: c.maxHeight,
        );
        return Column(
          children: [
            SizedBox(
              height: height,
              child: ScreensPanel(
                tiles: tiles,
                open: open,
                onWatch: _watch,
                onClose: _unwatch,
                onFullscreen: _enterFullscreen,
                onPreviewEnabled: _setPreviewEnabled,
                onVolume: _applyShareVolume,
              ),
            ),
            const SizedBox(height: 12),
            Expanded(child: _chatPanel(theme)),
          ],
        );
      },
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
            if (_sharing)
              const Pill(text: 'sharing', color: Colors.greenAccent),
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
      padding: const EdgeInsets.fromLTRB(10, 6, 6, 8),
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
        crossAxisAlignment: CrossAxisAlignment.stretch,
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
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      c.name,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: const TextStyle(
                        fontWeight: FontWeight.w600,
                        fontSize: 15,
                      ),
                    ),
                    Text(
                      members.isEmpty
                          ? 'voice empty'
                          : '${members.length} in voice',
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: const TextStyle(
                        color: Colors.white38,
                        fontSize: 12,
                      ),
                    ),
                  ],
                ),
              ),
              joined
                  ? _joinButton('Leave', _leaveChannel)
                  : _joinButton('Join', () => _joinChannel(c)),
            ],
          ),
          // Speakers are a VERTICAL list: one row per person, so a long
          // roster grows downwards and names never get squeezed into a
          // chip that wraps mid-name.
          for (final id in members) _memberRow(id),
        ],
      ),
    );
  }

  Widget _joinButton(String label, VoidCallback onPressed) {
    return TextButton(
      onPressed: onPressed,
      style: TextButton.styleFrom(
        minimumSize: const Size(64, 32),
        padding: const EdgeInsets.symmetric(horizontal: 8),
        tapTargetSize: MaterialTapTargetSize.shrinkWrap,
        visualDensity: VisualDensity.compact,
      ),
      child: Text(label),
    );
  }

  Widget _idleCard(ThemeData theme, List<int> ids) {
    return Container(
      margin: const EdgeInsets.only(bottom: 8),
      padding: const EdgeInsets.fromLTRB(10, 8, 6, 8),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(10),
        border: Border.all(color: Colors.white.withValues(alpha: 0.06)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const Row(
            children: [
              Icon(Icons.people_outline, size: 18, color: Colors.white38),
              SizedBox(width: 8),
              Expanded(
                child: Text(
                  'Online, not in a channel',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: Colors.white54, fontSize: 13),
                ),
              ),
            ],
          ),
          for (final id in ids) _memberRow(id, idle: true),
        ],
      ),
    );
  }

  /// One roster row. It lights up while that person is actually making
  /// sound (local peak meter, not a flag), shows a muted mic when they say
  /// they are muted, and opens the volume dialog on click.
  Widget _memberRow(int userId, {bool idle = false}) {
    final self = userId == _session?.userId;
    final level = self ? _selfLevel : (_levels[userId] ?? 0.0);
    final speaking = !idle && level > 0.05;
    final muted = _mutedUsers.contains(userId);
    final volume = _volume[userId] ?? 1.0;
    final name = self ? '${_nameOf(userId)} (you)' : _nameOf(userId);
    final hot = speaking
        ? accentSoft
        : (self ? accentSoft : Colors.greenAccent);
    // Your own volume is not a thing the mix can apply (your voice never
    // comes back to you); only other people's rows open the dialog.
    final adjustable = !idle && !self;
    return Tooltip(
      message: adjustable ? '$name · click for volume' : name,
      child: InkWell(
        borderRadius: BorderRadius.circular(6),
        onTap: adjustable ? () => _showVolumeDialog(userId) : null,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 110),
          margin: const EdgeInsets.only(top: 4),
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
          decoration: BoxDecoration(
            color: speaking
                ? accent.withValues(alpha: 0.30)
                : Colors.white.withValues(alpha: 0.05),
            borderRadius: BorderRadius.circular(6),
            border: Border.all(
              color: speaking
                  ? accentSoft
                  : Colors.white.withValues(alpha: 0.05),
            ),
            boxShadow: speaking
                ? [
                    BoxShadow(
                      color: accentSoft.withValues(alpha: 0.45),
                      blurRadius: 9,
                      spreadRadius: 1,
                    ),
                  ]
                : const [],
          ),
          child: Row(
            children: [
              if (idle)
                Container(
                  width: 6,
                  height: 6,
                  margin: const EdgeInsets.only(left: 3, right: 4),
                  decoration: const BoxDecoration(
                    shape: BoxShape.circle,
                    color: Colors.white24,
                  ),
                )
              else
                Icon(
                  muted
                      ? Icons.mic_off
                      : (speaking ? Icons.graphic_eq : Icons.mic_none),
                  size: 15,
                  color: muted ? Colors.white38 : hot,
                ),
              const SizedBox(width: 9),
              Expanded(
                child: Text(
                  name,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(fontSize: 13),
                ),
              ),
              if (volume != 1.0)
                Text(
                  '${(volume * 100).round()}%',
                  style: const TextStyle(
                    fontSize: 11,
                    color: Colors.white54,
                    fontFamily: 'monospace',
                  ),
                ),
            ],
          ),
        ),
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
                  ElevatedButton(
                    onPressed: _sendChat,
                    child: const Text('Send'),
                  ),
                ],
              ),
            ),
          ],
        ],
      ),
    );
  }

  // --- screens: the 16:9 grid and its open/strip states live in
  // widgets/screens_panel.dart (layout math and interactions are
  // widget-tested there); this is just the session state behind it.

  /// Everyone who has a screen to show: ourselves while sharing, then
  /// the other sharers alphabetically (a stable order — cards must not
  /// jump around on refresh).
  List<int> _screenIds() {
    final ids = [
      for (final id in _videos.keys)
        if (id != _session?.userId) id,
    ];
    ids.sort(
      (a, b) => _nameOf(a).toLowerCase().compareTo(_nameOf(b).toLowerCase()),
    );
    final me = _session?.userId;
    if (_sharing && me != null) ids.insert(0, me);
    return ids;
  }

  /// The panel's model for every current share. Our own tile's picture
  /// is the capture preview; its size/rate are the ENCODER's negotiation
  /// (so a cap shows as the real thing), falling back to the preview's
  /// own dimensions before the first frame.
  List<ScreenTile> _screenTiles() {
    final tiles = <ScreenTile>[];
    for (final id in _screenIds()) {
      final own = id == _session?.userId;
      final v = own ? null : _videos[id];
      final stats = own ? _shareStats : null;
      final preview = own ? _preview : null;
      final showing = !own || _settings.showSharePreview;
      tiles.add(
        ScreenTile(
          userId: id,
          name: own ? '${_nameOf(id)} (you)' : _nameOf(id),
          own: own,
          textureId: showing ? (own ? preview?.textureId : v?.$1) : null,
          width: own
              ? (stats?.width.toInt() ?? preview?.width.toInt() ?? 0)
              : (v?.$2 ?? 0),
          height: own
              ? (stats?.height.toInt() ?? preview?.height.toInt() ?? 0)
              : (v?.$3 ?? 0),
          fps: own ? (stats?.fps.toInt() ?? 0) : (v?.$4 ?? 0),
          notice: !showing
              ? 'preview off'
              : (own && preview == null
                    ? (_previewError ?? 'waiting for the first frame\u2026')
                    : (own ? null : 'connecting\u2026')),
          previewEnabled: _settings.showSharePreview,
          volume: _shareVolume[id] ?? 1.0,
        ),
      );
    }
    return tiles;
  }

  Future<void> _toggleMic() async {
    final sessionId = _session?.sessionId;
    if (sessionId == null) return;
    final me = _session?.userId;
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
      _selfLevel = 0;
      if (me != null) {
        if (next) {
          _mutedUsers.add(me);
        } else {
          _mutedUsers.remove(me);
        }
      }
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
              onPressed: inVoice ? (_sharing ? _stopShare : _startShare) : null,
              icon: Icon(
                _sharing ? Icons.stop_screen_share : Icons.screen_share,
                size: 20,
              ),
              label: Text(_sharing ? 'Stop sharing' : 'Share screen'),
            ),
            // The hardware-encode switch moved to Settings → Video: one
            // source of truth for a preference that is already persisted
            // and only takes effect on the NEXT share.
          ],
        ),
      ),
    );
  }
}
