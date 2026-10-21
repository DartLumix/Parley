import 'dart:async';

import 'package:flutter/material.dart';

import 'rust/api/login.dart' as login_api;
import 'rust/api/session.dart';
import 'rust/api/settings.dart' as settings_api;
import 'rust/api/video.dart' as video_api;
import 'theme.dart';
import 'widgets/fullscreen_share.dart';
import 'widgets/login_view.dart';
import 'widgets/pill.dart';
import 'widgets/share_dialog.dart';
import 'widgets/settings_page.dart';
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
      _watching = null;
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
      _levels.clear();
      _selfLevel = 0;
      _mutedUsers.clear();
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
      _watching = null;
      _fullscreen = null;
      _sharing = false;
      _micMuted = false;
      _levels.clear();
      _selfLevel = 0;
      _mutedUsers.clear();
      _volume.clear();
      _shareVolume.clear();
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

  /// Flip the preview switch from the share pane. Persisted like the
  /// settings-page control (same value), and it takes effect mid-share:
  /// the capture tap checks the setting per encoded frame.
  Future<void> _setPreviewEnabled(bool on) async {
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
          // The preview texture is fetched now, not at startup: a user
          // who never shares never pays for it.
          if (_settings.showSharePreview) _loadPreview();
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
        // Watching someone else replaces the pane above the chat, so the
        // sharer's own view rides here as a small overlay instead of
        // disappearing mid-sentence ("did I stop sharing?").
        if (_sharing &&
            _watching != null &&
            _settings.showSharePreview &&
            _preview != null)
          Positioned(
            right: 24,
            bottom: 96,
            child: _selfPreviewPip(_preview!),
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
    // A running share of our own takes the pane too: that is where the
    // preview and the Stop button live, even when nobody else shares.
    final sharing = _videos.isNotEmpty || _sharing;
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
                members.isEmpty ? 'voice empty' : '${members.length} in voice',
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

  /// One roster chip. It lights up while that person is actually making
  /// sound (local peak meter, not a flag), shows a muted mic when they say
  /// they are muted, and opens the volume dialog on click.
  Widget _memberChip(int userId, {bool idle = false}) {
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
    // comes back to you); only other people's chips open the dialog.
    final adjustable = !idle && !self;
    return Tooltip(
      message: adjustable ? '$name · click for volume' : name,
      child: InkWell(
        borderRadius: BorderRadius.circular(6),
        onTap: adjustable ? () => _showVolumeDialog(userId) : null,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 110),
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
          decoration: BoxDecoration(
            color: speaking
                ? accent.withValues(alpha: 0.30)
                : Colors.white.withValues(alpha: 0.06),
            borderRadius: BorderRadius.circular(6),
            border: Border.all(
              color: speaking
                  ? accentSoft
                  : Colors.white.withValues(alpha: 0.06),
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
            mainAxisSize: MainAxisSize.min,
            children: [
              if (idle)
                Container(
                  width: 6,
                  height: 6,
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
                  size: 13,
                  color: muted ? Colors.white38 : hot,
                ),
              const SizedBox(width: 6),
              Text(name, style: const TextStyle(fontSize: 12)),
              if (volume != 1.0) ...[
                const SizedBox(width: 5),
                Text(
                  '${(volume * 100).round()}%',
                  style: const TextStyle(
                    fontSize: 10,
                    color: Colors.white54,
                    fontFamily: 'monospace',
                  ),
                ),
              ],
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

  // --- share pane (right side): an invitation until "Watch".

  Widget _sharePane(ThemeData theme) {
    final watching = _watching != null && _videos.containsKey(_watching)
        ? _watching
        : null;
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
        if (_sharing) _selfPreviewCard(theme),
        const Text(
          'Screens',
          style: TextStyle(color: Colors.white54, fontSize: 12),
        ),
        const SizedBox(height: 8),
        if (ids.isEmpty && !_sharing)
          const Padding(
            padding: EdgeInsets.only(top: 12),
            child: Text(
              'Nobody is sharing yet. Join a channel and press Share screen.',
              style: TextStyle(color: Colors.white38, fontSize: 12),
            ),
          ),
        for (final id in ids) _shareCard(theme, id),
      ],
    );
  }

  /// The small always-visible version of the preview, used while another
  /// person's share owns the pane.
  Widget _selfPreviewPip(video_api.SharePreview preview) {
    return IgnorePointer(
      child: Container(
        width: 240,
        decoration: BoxDecoration(
          color: Colors.black,
          borderRadius: BorderRadius.circular(8),
          border: Border.all(color: accentSoft.withValues(alpha: 0.5)),
          boxShadow: const [
            BoxShadow(color: Colors.black54, blurRadius: 14, spreadRadius: 1),
          ],
        ),
        clipBehavior: Clip.antiAlias,
        child: Stack(
          children: [
            AspectRatio(
              aspectRatio: preview.width / preview.height,
              child: Texture(textureId: preview.textureId),
            ),
            Positioned(
              left: 6,
              bottom: 5,
              child: Container(
                padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
                decoration: BoxDecoration(
                  color: Colors.black54,
                  borderRadius: BorderRadius.circular(4),
                ),
                child: const Text(
                  'You',
                  style: TextStyle(fontSize: 10, color: Colors.white70),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// What YOU are sending, from the capture tap — the same frames, before
  /// they are encoded. Toggleable in place; off means the Rust side does
  /// no scaling at all, not that a frame is hidden.
  Widget _selfPreviewCard(ThemeData theme) {
    final on = _settings.showSharePreview;
    final preview = _preview;
    return Container(
      margin: const EdgeInsets.only(bottom: 12),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: accent.withValues(alpha: 0.10),
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: accentSoft.withValues(alpha: 0.35)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              const Icon(Icons.podcasts, color: accentSoft, size: 18),
              const SizedBox(width: 8),
              const Expanded(
                child: Text(
                  'Your stream',
                  style: TextStyle(fontWeight: FontWeight.w600),
                ),
              ),
              Text(
                on ? 'live' : 'preview off',
                style: const TextStyle(color: Colors.white54, fontSize: 11),
              ),
              Switch(
                value: on,
                onChanged: _setPreviewEnabled,
              ),
              IconButton(
                tooltip: 'Stop sharing',
                icon: const Icon(Icons.stop_screen_share, size: 20),
                onPressed: _stopShare,
              ),
            ],
          ),
          if (on)
            Padding(
              padding: const EdgeInsets.only(top: 6),
              child: ClipRRect(
                borderRadius: BorderRadius.circular(6),
                child: preview != null
                    ? AspectRatio(
                        aspectRatio: preview.width / preview.height,
                        child: Texture(textureId: preview.textureId),
                      )
                    : Container(
                        height: 120,
                        alignment: Alignment.center,
                        color: Colors.black26,
                        child: Text(
                          _previewError ?? 'waiting for the first frame\u2026',
                          style: const TextStyle(
                            color: Colors.white38,
                            fontSize: 11,
                          ),
                          textAlign: TextAlign.center,
                        ),
                      ),
              ),
            ),
        ],
      ),
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
              child: AspectRatio(aspectRatio: v.$2 / v.$3, child: texture),
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
              // Per-stream volume: this share's system audio only. Their
              // voice keeps its own level (the roster's volume dialog).
              SizedBox(
                width: 150,
                child: Row(
                  children: [
                    const Icon(
                      Icons.volume_up,
                      size: 15,
                      color: Colors.white54,
                    ),
                    Expanded(
                      child: Slider(
                        value: _shareVolume[userId] ?? 1.0,
                        min: 0,
                        max: 2,
                        onChanged: (value) => _applyShareVolume(userId, value),
                      ),
                    ),
                  ],
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
