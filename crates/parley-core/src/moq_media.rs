//! The shared MoQ media plane used by screen, camera and share audio.
//!
//! One [`MoqVideoPublisher`] owns a local origin, one broadcast, a hang
//! catalog and up to two tracks (H.264 video and Opus audio). The server
//! is a pure fan-out of those broadcasts; no frame is decoded or encoded
//! outside the capturing client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hang::catalog::{AudioCodec, AudioConfig, H264, VideoCodec, VideoConfig};
use moq_mux::container::{Frame, Kind};
use tokio::sync::mpsc;

use crate::adaptive::AdaptiveScreenController;
use crate::audio::SAMPLE_RATE;
use crate::audio::mixer::SharedMixer;
use crate::video_receive::SharedVideoReceiveHub;

const VIDEO_QUEUE: usize = 8;
const AUDIO_QUEUE: usize = 32;
/// Audio groups are bounded so a late joiner never waits out a long GOP.
const AUDIO_GROUP_FRAMES: u32 = 50; // 1 s at 20 ms packets
const SUBSCRIBE_MAX_AGE: Duration = Duration::from_secs(1);
/// A media-plane handshake that cannot finish in this window is not going
/// to start a share; surface it instead of leaving the UI "starting"
/// forever on a blocked UDP port.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the Parley control plane told us the MoQ listener lives. The
/// token is the same session token used by the QUIC control session.
#[derive(Debug, Clone)]
pub struct MoqEndpoint {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub token: String,
    pub media_id: u32,
    /// Account id. The broadcast path is account-scoped while the query
    /// carries the per-device media id used for routing and display.
    pub user_id: u32,
}

impl MoqEndpoint {
    pub fn publisher_url(
        &self,
        channel_id: u64,
        user_id: u32,
        kind: ShareKind,
    ) -> Result<String, String> {
        let path = format!("room/{channel_id}/user/{user_id}/{}", kind.as_str());
        self.url(&path)
    }

    pub fn room_url(&self, channel_id: u64) -> Result<String, String> {
        self.url(&format!("room/{channel_id}"))
    }

    fn url(&self, path: &str) -> Result<String, String> {
        let host = self.host.trim_matches(['[', ']']);
        // IPv6 literals need brackets in a URL authority; a bare `::1`
        // would parse the last group as the port.
        let authority = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let mut url = url::Url::parse(&format!("moqt://{authority}:{}/{}", self.port, path))
            .map_err(|e| format!("invalid MoQ endpoint host {:?}: {e}", self.host))?;
        url.query_pairs_mut()
            .append_pair("token", &self.token)
            .append_pair("media", &self.media_id.to_string());
        Ok(url.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShareKind {
    #[default]
    Screen,
    Camera,
}

impl ShareKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Camera => "camera",
        }
    }
}

/// One encoded H.264 access unit. `keyframe` starts a new MoQ group.
pub struct VideoFrame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// One encoded Opus packet plus its source sample clock. Timestamps must
/// come from the encoder, not from when this writer happens to drain the
/// queue: the mux delivers audio in bursts, and arrival time would push a
/// group's end into the future and make the next keyframe rewind.
struct AudioPacket {
    timestamp_samples: u32,
    data: Vec<u8>,
}

/// Handle used by the capture/encoder thread. Cloning is cheap.
#[derive(Clone)]
pub struct MoqVideoPublisher {
    video: mpsc::Sender<VideoFrame>,
    audio: Option<mpsc::Sender<AudioPacket>>,
    bandwidth: Arc<AtomicU64>,
    join_idr: Arc<AtomicBool>,
    catalog: moq_mux::catalog::Producer,
    // The spawned origin driver finishes when every producer handle drops.
    // Keep one for the share's lifetime; the session and the broadcast are
    // not a substitute for this handle.
    _origin: moq_net::origin::Producer,
    // The origin ends a broadcast when its last producer handle drops. The
    // writers hold track handles, not this one, so keep a clone here for the
    // share's lifetime or the announcement is retracted immediately after
    // connect.
    _broadcast: moq_net::broadcast::Producer,
    closed: Arc<AtomicBool>,
    shutdown: Arc<PublisherShutdown>,
}

/// The last publisher clone closes the MoQ session. `watch_bandwidth` owns
/// a connection clone for the share's lifetime, so dropping the senders
/// alone would leave that task (and the UDP session) alive forever.
struct PublisherShutdown(tokio::sync::watch::Sender<bool>);

impl PublisherShutdown {
    fn signal(&self) {
        let _ = self.0.send(true);
    }
}

impl Drop for PublisherShutdown {
    fn drop(&mut self) {
        self.signal();
    }
}

impl MoqVideoPublisher {
    /// Dial the server, announce the broadcast and start the two writer
    /// tasks. Must run inside a Tokio runtime.
    pub async fn connect(
        url: &str,
        fingerprint: &str,
        path: &str,
        controller: AdaptiveScreenController,
        with_audio: bool,
    ) -> Result<Self, String> {
        let parsed = url::Url::parse(url).map_err(|e| format!("moq url: {e}"))?;
        let mut config = moq_tokio::connect::Config::default();
        config.url = Some(parsed.clone());
        config.tls.fingerprint = vec![fingerprint.to_string()];
        config.once = Some(true);
        let client = config
            .init(Default::default())
            .map_err(|e| format!("moq client: {e}"))?;

        let origin = moq_tokio::origin::spawn();
        let client = client.with_publisher(origin.clone());
        let connection =
            tokio::time::timeout(CONNECT_TIMEOUT, client.connect(parsed).established())
                .await
                .map_err(|_| "moq connect timed out".to_string())?
                .map_err(|e| format!("moq connect: {e}"))?;

        let (width, height) = controller.current_dimensions();
        let mut broadcast = origin
            .create_broadcast(path)
            .map_err(|e| format!("moq broadcast: {e}"))?;
        let catalog = moq_mux::catalog::Producer::new(&mut broadcast, Default::default())
            .map_err(|e| format!("moq catalog: {e}"))?;

        let mut video_config = VideoConfig::new(VideoCodec::H264(H264 {
            // OpenH264/VAAPI repeat SPS/PPS in-band before every IDR.
            inline: true,
            profile: 0x64,
            constraints: 0,
            level: 0x2a,
        }));
        video_config.coded_width = Some(width);
        video_config.coded_height = Some(height);
        video_config.framerate = Some(f64::from(controller.target_fps()));
        video_config.bitrate = Some(controller.max_bitrate());
        video_config.optimize_for_latency = Some(true);
        let video_track = broadcast
            .create_track("video", catalog.track_info(hang::catalog::PRIORITY.video))
            .map_err(|e| format!("moq video track: {e}"))?;
        let video = catalog
            .video(
                video_track,
                moq_mux::catalog::hang::Container::Legacy(Kind::Video),
                video_config,
            )
            .map_err(|e| format!("moq video rendition: {e}"))?;

        let mut audio_out = None;
        if with_audio {
            let audio_track = broadcast
                .create_track("audio", catalog.track_info(hang::catalog::PRIORITY.audio))
                .map_err(|e| format!("moq audio track: {e}"))?;
            let mut audio_config = AudioConfig::new(AudioCodec::Opus, 48_000, 2);
            audio_config.bitrate = Some(256_000);
            let audio = catalog
                .audio(
                    audio_track,
                    moq_mux::catalog::hang::Container::Legacy(Kind::Audio),
                    audio_config,
                )
                .map_err(|e| format!("moq audio rendition: {e}"))?;
            let (tx, rx) = mpsc::channel(AUDIO_QUEUE);
            audio_out = Some(tx);
            tokio::spawn(audio_writer(audio, rx, catalog.clock()));
        }

        broadcast
            .announce(Default::default())
            .map_err(|e| format!("moq announce: {e}"))?;

        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE);
        let bandwidth = Arc::new(AtomicU64::new(0));
        let join_idr = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(video_writer(
            video,
            video_rx,
            catalog.clock(),
            closed.clone(),
        ));
        tokio::spawn(watch_demand(
            broadcast.demand(),
            join_idr.clone(),
            shutdown_rx.clone(),
        ));
        tokio::spawn(watch_bandwidth(
            connection,
            bandwidth.clone(),
            closed.clone(),
            shutdown_rx,
        ));

        Ok(Self {
            video: video_tx,
            audio: audio_out,
            bandwidth,
            join_idr,
            catalog: catalog.clone(),
            _origin: origin,
            _broadcast: broadcast,
            closed,
            shutdown: Arc::new(PublisherShutdown(shutdown_tx)),
        })
    }

    /// Queue one encoded access unit. A full queue drops this frame and
    /// asks the encoder for a fresh anchor; it never blocks capture.
    pub fn send_video(&self, _frame_id: u32, keyframe: bool, data: &[u8]) -> bool {
        if data.is_empty() || self.closed.load(Ordering::Relaxed) {
            return false;
        }
        match self.video.try_send(VideoFrame {
            keyframe,
            data: data.to_vec(),
        }) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.join_idr.store(true, Ordering::Relaxed);
                eprintln!("share: MoQ video queue full; requesting IDR");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queue one encoded Opus packet with the encoder's 48 kHz sample
    /// timestamp. The timestamp defines the packet's place in the MoQ
    /// timeline; a queue full drop leaves a sequence gap but never rewinds
    /// the track.
    pub fn send_audio(&self, packet: &[u8], timestamp_samples: u32) -> bool {
        let Some(audio) = &self.audio else {
            return false;
        };
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        match audio.try_send(AudioPacket {
            timestamp_samples,
            data: packet.to_vec(),
        }) {
            Ok(()) => true,
            // A full queue is momentary backpressure. Dropping one Opus
            // packet is recoverable; reporting "closed" would make the mux
            // terminate and silence the rest of the share.
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Current depth of the bounded writer queue, read at the moment the
    /// adaptive controller asks. A cached value would make one full-queue
    /// sample look current on the next frame even after the writer drained
    /// it, causing a needless resolution step-down.
    pub fn queued_frames(&self) -> usize {
        VIDEO_QUEUE.saturating_sub(self.video.capacity())
    }

    /// The writer task has ended: the MoQ session or broadcast is gone.
    /// A full queue is NOT closed — that is `send_video`'s false return
    /// while this stays false.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed) || self.video.is_closed()
    }

    /// Stop the writer and bandwidth tasks and close the media session.
    /// Idempotent; called when the share's encoder is dropped so a handle
    /// still parked in the UI cannot keep a dead share's UDP session open.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        // Retract the broadcast now. Waiting for the last handle clone to
        // drop would leave the room subscribed to a share the UI has already
        // stopped, and the server would not emit ShareStopped until then.
        self._broadcast.close();
        self.shutdown.signal();
    }

    /// Congestion-controller estimate in bits per second, or `None`
    /// before the transport has a window.
    pub fn bandwidth_bps(&self) -> Option<u64> {
        let value = self.bandwidth.load(Ordering::Relaxed);
        (value > 0).then_some(value)
    }

    /// A subscriber appeared: the encoder should emit an IDR, debounced
    /// by [`AdaptiveScreenController::request_join_idr`].
    pub fn join_idr_pending(&self) -> bool {
        self.join_idr.load(Ordering::Relaxed)
    }

    /// Consume the request once the caller has actually scheduled the IDR.
    /// Keeping the request pending until then matters: the adaptive
    /// controller debounces joins, and consuming it on a debounced frame
    /// used to lose the only keyframe a just-joined viewer needed.
    pub fn take_join_idr_request(&self) -> bool {
        self.join_idr.swap(false, Ordering::Relaxed)
    }

    /// Keep the hang catalog's geometry hint in step with a Stage-2 tier
    /// change. The in-band SPS/PPS remain authoritative; this is the
    /// allocation hint players read before the next IDR arrives.
    pub fn set_video_dimensions(&mut self, width: u32, height: u32) {
        let _ = self.catalog.mutate(|catalog| {
            if let Some(video) = catalog.video.renditions.get_mut("video") {
                video.coded_width = Some(width);
                video.coded_height = Some(height);
            }
        });
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }
}

async fn video_writer(
    mut video: moq_mux::container::Producer<
        moq_mux::catalog::hang::Container,
        hang::catalog::VideoConfig,
    >,
    mut rx: mpsc::Receiver<VideoFrame>,
    clock: moq_mux::Clock,
    closed: Arc<AtomicBool>,
) {
    while let Some(frame) = rx.recv().await {
        let timestamp = clock.now();
        let duration = moq_net::Timestamp::from_micros(16_667).ok();
        if let Err(e) = video.write(Frame {
            timestamp,
            duration,
            payload: Bytes::from(frame.data),
            keyframe: frame.keyframe,
        }) {
            eprintln!("share: MoQ video write failed: {e}");
            closed.store(true, Ordering::Relaxed);
            break;
        }
    }
    // The sender may stay parked in the UI after the transport died. This
    // flag is what tells it to stop, rather than queueing into a writer that
    // no longer exists.
    closed.store(true, Ordering::Relaxed);
}

async fn audio_writer(
    mut audio: moq_mux::container::Producer<
        moq_mux::catalog::hang::Container,
        hang::catalog::AudioConfig,
    >,
    mut rx: mpsc::Receiver<AudioPacket>,
    clock: moq_mux::Clock,
) {
    let mut frames = 0u32;
    // Anchor the encoder's sample clock to the moment the first packet is
    // written. Every later timestamp is derived from the sample count, so a
    // burst drain stays on one continuous, monotonic timeline.
    let mut anchor: Option<(u64, u32)> = None;
    while let Some(packet) = rx.recv().await {
        let keyframe = frames.is_multiple_of(AUDIO_GROUP_FRAMES);
        let duration = moq_net::Timestamp::from_micros(20_000).ok();
        let (base_micros, first_samples) = *anchor
            .get_or_insert_with(|| (clock.now().as_micros() as u64, packet.timestamp_samples));
        let Some(timestamp) = audio_timestamp(base_micros, first_samples, packet.timestamp_samples)
        else {
            eprintln!("share: MoQ audio timestamp overflow");
            break;
        };
        if let Err(e) = audio.write(Frame {
            timestamp,
            duration,
            payload: Bytes::from(packet.data),
            keyframe,
        }) {
            eprintln!("share: MoQ audio write failed: {e}");
            break;
        }
        frames = frames.wrapping_add(1);
    }
}

/// Map the encoder's per-channel sample clock onto the broadcast clock.
/// Sample-derived time is monotonic even when the writer drains a burst
/// of packets faster than real time.
fn audio_timestamp(
    base_micros: u64,
    first_samples: u32,
    samples: u32,
) -> Option<moq_net::Timestamp> {
    let elapsed = u64::from(samples.wrapping_sub(first_samples));
    moq_net::Timestamp::from_micros(base_micros + elapsed * 1_000_000 / u64::from(SAMPLE_RATE)).ok()
}

async fn watch_demand(
    demand: moq_net::broadcast::Demand,
    flag: Arc<AtomicBool>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let used = tokio::select! {
            _ = shutdown.changed() => break,
            used = demand.used() => used,
        };
        if used.is_err() {
            break;
        }
        flag.store(true, Ordering::Relaxed);
        let unused = tokio::select! {
            _ = shutdown.changed() => break,
            unused = demand.unused() => unused,
        };
        if unused.is_err() {
            break;
        }
    }
}

async fn watch_bandwidth(
    connection: moq_tokio::Connection,
    bandwidth: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // The transport's own congestion estimate, not our delivered rate:
    // a static screen using 100 kbps on a 100 Mbps link must not look
    // like a starved link.
    let estimate = connection.send_bandwidth();
    loop {
        tokio::select! {
            // Hold a connection clone only while the connection is alive;
            // otherwise this task would keep a dead share's QUIC session
            // (and its socket) open forever.
            _ = connection.closed() => break,
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if let Some(rate) = estimate.peek() {
                    bandwidth.store(rate.as_bps(), Ordering::Relaxed);
                } else {
                    bandwidth.store(0, Ordering::Relaxed);
                }
            }
        }
    }
    bandwidth.store(0, Ordering::Relaxed);
    closed.store(true, Ordering::Relaxed);
}

/// Maps a room path's account id to the per-device media id the rest of
/// Parley already uses for decoders and the audio mixer. The control
/// plane fills this from `ShareStarted`. Frames that arrive before that
/// mapping are dropped: the account id is not the media id, and feeding
/// it to the display would create a tile under the wrong key that the
/// later `ShareStarted` could not repair.
#[derive(Clone, Default)]
pub struct SenderMap(Arc<Mutex<HashMap<u32, u32>>>);

impl SenderMap {
    pub fn set(&self, user_id: u32, media_id: u32) {
        self.0.lock().unwrap().insert(user_id, media_id);
    }

    pub fn remove(&self, user_id: u32) {
        self.0.lock().unwrap().remove(&user_id);
    }

    /// Drop every account→media mapping. Called when leaving a room: the
    /// next room can reuse account ids with different device media ids.
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn resolve(&self, user_id: u32) -> Option<u32> {
        self.0.lock().unwrap().get(&user_id).copied()
    }
}

/// Subscribe to every screen/camera broadcast in one room.
///
/// The server scopes the origin to that room, so paths arriving here are
/// already authorized. Each broadcast gets independent video and audio
/// consumers; a `max_age` of one second lets a stalled viewer skip an
/// incomplete GOP and resume at the next IDR without touching the
/// publisher.
pub async fn spawn_room_subscriber(
    endpoint: &MoqEndpoint,
    room_id: u64,
    hub: SharedVideoReceiveHub,
    mixer: Option<SharedMixer>,
    senders: SenderMap,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let url = endpoint.room_url(room_id)?;
    let own_user_id = endpoint.user_id;
    let parsed = url::Url::parse(&url).map_err(|e| format!("moq url: {e}"))?;
    let mut config = moq_tokio::connect::Config::default();
    config.url = Some(parsed.clone());
    config.tls.fingerprint = vec![endpoint.fingerprint.clone()];
    config.once = Some(true);
    let client = config
        .init(Default::default())
        .map_err(|e| format!("moq client: {e}"))?;

    let origin = moq_tokio::origin::spawn();
    let connection = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client
            .with_subscriber(origin.clone())
            .connect(parsed)
            .established(),
    )
    .await
    .map_err(|_| "moq subscribe connect timed out".to_string())?
    .map_err(|e| format!("moq subscribe connect: {e}"))?;

    let mut announcements = origin.consume().announced();
    let mut readers: HashMap<String, Vec<tokio::task::JoinHandle<()>>> = HashMap::new();

    tokio::spawn(async move {
        // The consumer's task handle is non-owning; hold an origin producer
        // clone so its driver keeps serving announcements for the room
        // lifetime while the loop below opens broadcasts from it.
        let _origin = origin.clone();
        let _connection = connection;
        loop {
            tokio::select! {
                // Leaving voice/changing rooms must close this subscription,
                // or the old room keeps decoding into the same hub and mixer.
                _ = stop.changed() => break,
                update = announcements.next() => {
                    let Some(update) = update else { break };
                    let path = update.prefix.as_str().to_string();
                    let Some((user_id, kind)) = parse_share_path(&path, room_id) else {
                        continue;
                    };
                    // The MoQ origin fans a room out to every subscriber,
                    // including the broadcaster. The old datagram router
                    // excluded the sender, and the local preview already
                    // shows the sharer their own capture. Subscribing to
                    // self here would decode the picture twice and, for
                    // share audio, play the sharer's own output back into
                    // their speakers.
                    if user_id == own_user_id {
                        continue;
                    }
                    if update.kind.is_active() {
                        if readers.contains_key(&path) {
                            continue;
                        }
                        let broadcast = match origin
                            .consume()
                            .request_broadcast(update.prefix.clone())
                            .await
                        {
                            Ok(broadcast) => broadcast,
                            Err(e) => {
                                eprintln!("moq: cannot open {path}: {e}");
                                continue;
                            }
                        };
                        let hub = hub.clone();
                        let mixer = mixer.clone();
                        let senders = senders.clone();
                        let mut handles = Vec::new();
                        match broadcast.track("video") {
                            Ok(track) => {
                                let subscription = moq_net::track::Subscription::default()
                                    .with_max_age(SUBSCRIBE_MAX_AGE);
                                match track.subscribe(subscription).await {
                                    Ok(subscriber) => {
                                        handles.push(tokio::spawn(read_video(
                                            subscriber,
                                            user_id,
                                            hub,
                                            senders.clone(),
                                        )));
                                    }
                                    Err(e) => eprintln!("moq: video subscribe {path}: {e}"),
                                }
                            }
                            Err(e) => eprintln!("moq: no video track in {path}: {e}"),
                        }
                        if let Some(mixer) = mixer
                            && let Ok(track) = broadcast.track("audio")
                        {
                            let subscription = moq_net::track::Subscription::default()
                                .with_max_age(SUBSCRIBE_MAX_AGE);
                            if let Ok(subscriber) = track.subscribe(subscription).await {
                                handles.push(tokio::spawn(read_audio(
                                    subscriber, user_id, mixer, senders,
                                )));
                            }
                        }
                        let _ = kind;
                        readers.insert(path.clone(), handles);
                    } else if let Some(handles) = readers.remove(&path) {
                        for handle in handles {
                            handle.abort();
                        }
                    }
                }
            }
        }
        // Dropping a JoinHandle detaches the task; abort explicitly so a
        // stopped subscription cannot keep a decoder and mixer fed. Then
        // drop everything the old room already decoded, so a room switch
        // cannot show a stale frame before the new room's first keyframe.
        for (_, handles) in readers {
            for handle in handles {
                handle.abort();
            }
        }
        hub.clear();
    });

    Ok(())
}

fn parse_share_path(path: &str, room_id: u64) -> Option<(u32, ShareKind)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 5
        || parts[0] != "room"
        || parts[1] != room_id.to_string()
        || parts[2] != "user"
    {
        return None;
    }
    let user_id = parts[3].parse().ok()?;
    let kind = match parts[4] {
        "screen" => ShareKind::Screen,
        "camera" => ShareKind::Camera,
        _ => return None,
    };
    Some((user_id, kind))
}

async fn read_video(
    subscriber: moq_net::track::Subscriber,
    user_id: u32,
    hub: SharedVideoReceiveHub,
    senders: SenderMap,
) {
    let mut consumer = moq_mux::container::Consumer::new(
        subscriber,
        moq_mux::catalog::hang::Container::Legacy(Kind::Video),
    );
    let mut frame_id = 0u32;
    let mut previous = None;
    loop {
        match consumer.read().await {
            Ok(Some(frame)) => {
                let timestamp = frame.timestamp.as_micros();
                let high_rate = previous.is_some_and(|p| timestamp.saturating_sub(p) <= 20_000);
                previous = Some(timestamp);
                let Some(sender) = senders.resolve(user_id) else {
                    // The control-plane mapping is the only source of the
                    // media id. Dropping until it arrives is deliberate.
                    continue;
                };
                hub.push_stream_frame(
                    sender,
                    parley_proto::media::VIDEO_CODEC_H264,
                    frame.keyframe,
                    high_rate,
                    frame_id,
                    &frame.payload,
                );
                frame_id = frame_id.wrapping_add(1);
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("moq: video read failed: {e}");
                break;
            }
        }
    }
}

async fn read_audio(
    subscriber: moq_net::track::Subscriber,
    user_id: u32,
    mixer: SharedMixer,
    senders: SenderMap,
) {
    let mut consumer = moq_mux::container::Consumer::new(
        subscriber,
        moq_mux::catalog::hang::Container::Legacy(Kind::Audio),
    );
    let mut sequence = 0u16;
    loop {
        match consumer.read().await {
            Ok(Some(frame)) => {
                let Some(sender) = senders.resolve(user_id) else {
                    continue;
                };
                let _ =
                    mixer
                        .lock()
                        .unwrap()
                        .push_packet(sender, sequence, &frame.payload, true, true);
                sequence = sequence.wrapping_add(1);
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("moq: audio read failed: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_map_only_resolves_announced_media_ids() {
        let senders = SenderMap::default();
        assert_eq!(senders.resolve(7), None);
        senders.set(7, 21);
        assert_eq!(senders.resolve(7), Some(21));
        senders.remove(7);
        assert_eq!(senders.resolve(7), None);
    }

    #[tokio::test]
    async fn audio_burst_never_rewinds_across_group_boundaries() {
        let origin = moq_tokio::origin::spawn();
        let mut broadcast = origin.create_broadcast("room/1/user/7/screen").unwrap();
        let catalog = moq_mux::catalog::Producer::new(&mut broadcast, Default::default()).unwrap();
        let track = broadcast
            .create_track("audio", catalog.track_info(hang::catalog::PRIORITY.audio))
            .unwrap();
        let mut audio = catalog
            .audio(
                track,
                moq_mux::catalog::hang::Container::Legacy(Kind::Audio),
                hang::catalog::AudioConfig::new(AudioCodec::Opus, 48_000, 2),
            )
            .unwrap();

        // 120 packets = 2.4 s of audio, written as one burst. The second
        // group starts exactly where the first one ends; arrival-time
        // stamping would put that start below the first group's live edge.
        let base = catalog.clock().now().as_micros() as u64;
        for frame in 0..120u32 {
            let timestamp = audio_timestamp(base, 0, frame * 960).unwrap();
            audio
                .write(Frame {
                    timestamp,
                    duration: moq_net::Timestamp::from_micros(20_000).ok(),
                    payload: Bytes::from_static(&[0x55]),
                    keyframe: frame.is_multiple_of(AUDIO_GROUP_FRAMES),
                })
                .unwrap_or_else(|e| panic!("frame {frame} rejected: {e}"));
        }
    }

    #[test]
    fn ipv6_publisher_url_is_bracketed() {
        let endpoint = MoqEndpoint {
            host: "::1".into(),
            port: 4445,
            fingerprint: "fingerprint".into(),
            token: "token".into(),
            media_id: 7,
            user_id: 11,
        };
        let url = endpoint
            .publisher_url(42, 11, ShareKind::Screen)
            .expect("valid endpoint");
        assert!(
            url.starts_with("moqt://[::1]:4445/room/42/user/11/screen?"),
            "unexpected url: {url}"
        );
        assert!(url.contains("token=token"));
        assert!(url.contains("media=7"));
    }
}
