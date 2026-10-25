//! Server-side per-viewer transcode.
//!
//! The sharer's stream is left alone. When a viewer reports that it
//! cannot sustain the original (a low `viewer_fps` in the five-second
//! `VideoReport`), a job decodes the sharer's H.264 once and re-encodes
//! a lower tier for that viewer only. Viewers that keep up continue to
//! receive the original datagrams untouched.
//!
//! One job = one sender, one worker thread, one decoder and one encoder
//! per active tier. The manager also suppresses the weak viewer's report
//! on its way to the sharer, so the sharer's own resolution ladder does
//! not fight the server's tier.
//!
//! The worker is deliberately single-threaded (the encoder's slice
//! parallelism is configurable, default one thread): a share that needs
//! help must not cost the host several cores. The cap and tiers live in
//! [`crate::config::VideoConfig`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tracing::{debug, info, warn};

use crate::config::VideoConfig;
use crate::session::SessionRegistry;
use crate::voice::VoiceManager;
use parley_core::error::Error;
use parley_core::video::{SoftwareEncoder, VideoDecoder, VideoEncoder, bitrate_for};
use parley_proto::video::{
    AssembledFrame, MAX_FRAGMENT_PAYLOAD, VideoAssembler, decode_fragment, fragment_frame_with_max,
};

/// The re-encoded tiers run at the client software profile's rate.
const TIER_FPS: f32 = 30.0;
/// The tier's own IDR cadence, matching the client encoder's intra
/// period. A lost fragment, or a stale original datagram arriving just
/// after the switch, leaves the viewer's assembler waiting for a
/// keyframe; only a keyframe gets it decoding again.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Frame-rate meter window for the sharer's own stream.
const METER_WINDOW: Duration = Duration::from_secs(1);
/// A sender/viewer with no traffic for this long is gone.
const STALE: Duration = Duration::from_secs(15);
/// Ignore reports immediately after a tier change: the viewer is still
/// watching the old stream while the new keyframe is found.
const SWITCH_GRACE: Duration = Duration::from_secs(8);
/// How long after joining a job we wait for a transcoded keyframe before
/// giving up and handing the viewer back to the sharer's ladder.
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(10);
/// Consecutive healthy reports needed to stop transcoding a viewer.
const GOOD_REPORTS: u32 = 3;
/// How long a viewer stays on a tier before the server probes the
/// original again. A 30 fps tier trivially keeps up with a 30 fps
/// source, so a report alone cannot prove the viewer now decodes the
/// ORIGINAL; without this dwell, every low-cadence share would flap
/// between the tier and the full stream every report.
const MIN_TIER_DWELL: Duration = Duration::from_secs(60);
/// Fragments buffered for the worker. At ~1 KB each, 512 is a few hundred
/// milliseconds of a busy stream; a full queue drops the newest fragment,
/// and the assembler rebases on the next keyframe.
const CHANNEL_CAPACITY: usize = 512;
/// A share with no video datagrams for this long ends its job.
const WORKER_IDLE: Duration = Duration::from_secs(10);
/// Below this incoming frame rate the bottleneck is upstream, not the
/// viewer's decoder: transcoding the same slow stream cannot help.
const MIN_SOURCE_FPS: f64 = 12.0;

/// What the server does with a viewer's `VideoReport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportAction {
    /// Route it to the channel as before: the sharer's ladder owns it.
    Forward,
    /// The server is handling this viewer; don't let the sharer react.
    Suppress,
}

pub struct TranscodeManager {
    config: VideoConfig,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// channel -> senders with live video.
    shares: HashMap<u64, HashSet<u32>>,
    /// sender -> incoming frame-rate meter.
    meters: HashMap<u32, Meter>,
    /// sender -> active job.
    jobs: HashMap<u32, Arc<Job>>,
    /// (viewer, sender) -> report history while deciding whether to start.
    watches: HashMap<(u32, u32), Watch>,
}

struct Meter {
    last_frame: Option<u32>,
    frames: u32,
    window: Instant,
    fps: f64,
    last_seen: Instant,
}

impl Meter {
    fn new() -> Self {
        Self {
            last_frame: None,
            frames: 0,
            window: Instant::now(),
            fps: 0.0,
            last_seen: Instant::now(),
        }
    }

    fn observe(&mut self, frame_id: u32, now: Instant) {
        self.last_seen = now;
        if self.last_frame != Some(frame_id) {
            self.last_frame = Some(frame_id);
            self.frames += 1;
        }
        let elapsed = now.duration_since(self.window);
        if elapsed >= METER_WINDOW {
            self.fps = f64::from(self.frames) / elapsed.as_secs_f64();
            self.frames = 0;
            self.window = now;
        }
    }
}

struct Watch {
    low: u32,
    good: u32,
    last: Instant,
}

impl Watch {
    fn new() -> Self {
        Self {
            low: 0,
            good: 0,
            last: Instant::now(),
        }
    }
}

#[derive(Clone, Copy)]
enum WatchStep {
    Wait,
    Start,
    Recover,
    Forward,
}

/// Pure decision step for a viewer that is not yet in a job.
fn watch_step(watch: &mut Watch, viewer_fps: u32, min_fps: u32, now: Instant) -> WatchStep {
    if now.duration_since(watch.last) > STALE {
        *watch = Watch::new();
    }
    watch.last = now;
    if viewer_fps >= min_fps {
        watch.low = 0;
        watch.good += 1;
        return if watch.good >= GOOD_REPORTS {
            WatchStep::Recover
        } else {
            WatchStep::Forward
        };
    }
    watch.low += 1;
    watch.good = 0;
    // A severe slump (half the target) starts on the first report; a
    // marginal one waits for confirmation, so one keyframe does not
    // spill a transcode job.
    if viewer_fps < min_fps / 2 || watch.low >= 2 {
        WatchStep::Start
    } else {
        WatchStep::Wait
    }
}

struct Job {
    sender_id: u32,
    channel_id: u64,
    tx: SyncSender<Vec<u8>>,
    viewers: Mutex<HashMap<u32, ViewerJob>>,
    stop: AtomicBool,
    force_idr: AtomicBool,
}

#[derive(Clone)]
struct ViewerJob {
    tier: u32,
    /// True once a transcoded keyframe reached this viewer. Until then
    /// the server keeps forwarding the original, so switching streams
    /// does not blank the tile while the worker finds an IDR.
    live: bool,
    /// When this viewer joined the job (not reset by a tier change).
    /// The recovery probe waits [`MIN_TIER_DWELL`] from here.
    started: Instant,
    since: Instant,
    low: u32,
    good: u32,
}

impl TranscodeManager {
    pub fn new(
        config: VideoConfig,
        registry: Arc<SessionRegistry>,
        voice: Arc<VoiceManager>,
    ) -> Self {
        Self {
            config: config.normalized(),
            registry,
            voice,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Feed one video datagram's payload (the bytes after the media
    /// header). Records the live share, meters the sender's own rate,
    /// and hands the fragment to an active job.
    pub fn note_video(&self, sender_id: u32, channel_id: u64, payload: &[u8]) {
        let Ok(frag) = decode_fragment(payload) else {
            return;
        };
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        self.prune(&mut inner, now);
        inner
            .shares
            .entry(channel_id)
            .or_default()
            .insert(sender_id);
        inner
            .meters
            .entry(sender_id)
            .or_insert_with(Meter::new)
            .observe(frag.frame_id, now);
        if let Some(job) = inner.jobs.get(&sender_id)
            && job.channel_id == channel_id
        {
            match job.tx.try_send(payload.to_vec()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    debug!(sender_id, "transcode input behind: dropping a fragment");
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    /// Should this viewer receive the sharer's original fragment? While a
    /// job is waiting for its first keyframe, yes: the viewer keeps the
    /// old stream until a transcoded picture is ready.
    pub fn should_forward(&self, sender_id: u32, viewer_id: u32) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.jobs.get(&sender_id) {
            Some(job) => {
                let viewers = job.viewers.lock().unwrap();
                viewers.get(&viewer_id).is_none_or(|v| !v.live)
            }
            None => true,
        }
    }

    /// Account for a viewer's report. `Suppress` means the server is
    /// steering this viewer and the sharer must not also lower its
    /// stream in response.
    pub fn on_report(&self, viewer_id: u32, channel_id: u64, viewer_fps: u32) -> ReportAction {
        if !self.config.transcode || self.config.max_transcodes == 0 {
            return ReportAction::Forward;
        }
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        self.prune(&mut inner, now);

        // A report is an aggregate over the viewer's active senders, so
        // only a channel with exactly one live share is attributed
        // unambiguously. With two, the sharer's own ladder handles it.
        let Some(sender_id) = Self::sole_share(&inner, channel_id) else {
            return ReportAction::Forward;
        };
        if viewer_fps == 0 {
            // No pictures decoded, not "not measured yet" when this
            // viewer already owns a seat in a job: the assembler is
            // stalled waiting for a keyframe. Repair the tier, and keep
            // the report from making the sharer degrade every viewer.
            if let Some(job) = inner.jobs.get(&sender_id)
                && job.viewers.lock().unwrap().contains_key(&viewer_id)
            {
                job.force_idr.store(true, Ordering::Relaxed);
                return ReportAction::Suppress;
            }
            return ReportAction::Forward;
        }
        let sender_fps = inner.meters.get(&sender_id).map_or(0.0, |m| m.fps);
        let sender_ok = sender_fps == 0.0 || sender_fps >= MIN_SOURCE_FPS;
        let in_job = inner
            .jobs
            .get(&sender_id)
            .is_some_and(|job| job.viewers.lock().unwrap().contains_key(&viewer_id));
        // A viewer at 20 fps on a 20 fps source is keeping up; the low
        // absolute number is the source, not a decode deficit. Do not
        // start a tier (and do not suppress the report) for that case.
        //
        // This comparison is only meaningful for a viewer still on the
        // ORIGINAL: while it watches a 30 fps tier, "30 >= 0.8 * 30"
        // would keep cancelling the very job that made the stream
        // decodable. Members go through the tier's own recovery path.
        let keeps_up = sender_fps > 0.0 && f64::from(viewer_fps) >= sender_fps * 0.8;
        if keeps_up && !in_job {
            inner.watches.remove(&(viewer_id, sender_id));
            return ReportAction::Forward;
        }

        if let Some(job) = inner.jobs.get(&sender_id).cloned() {
            let mut viewers = job.viewers.lock().unwrap();
            if let Some(v) = viewers.get_mut(&viewer_id) {
                if now.duration_since(v.since) < SWITCH_GRACE {
                    return ReportAction::Suppress;
                }
                if !v.live && now.duration_since(v.since) > FIRST_FRAME_GRACE {
                    // The worker never produced a keyframe (decode error,
                    // unsupported bitstream). Stop steering this viewer;
                    // the sharer's ladder is the fallback.
                    viewers.remove(&viewer_id);
                    let empty = viewers.is_empty();
                    drop(viewers);
                    warn!(sender_id, viewer_id, "transcode: no first frame; giving up");
                    if empty {
                        self.stop_job(&mut inner, sender_id);
                    }
                    return ReportAction::Forward;
                }
                if viewer_fps < self.config.min_viewer_fps {
                    v.low += 1;
                    v.good = 0;
                    if v.low >= 2 {
                        if v.tier > self.config.second_height {
                            v.tier = self.config.second_height;
                            v.since = now;
                            v.live = false;
                            job.force_idr.store(true, Ordering::Relaxed);
                            info!(
                                sender_id,
                                viewer_id,
                                tier = v.tier,
                                "transcode: lowering tier"
                            );
                        }
                        v.low = 0;
                    }
                    return ReportAction::Suppress;
                }
                if viewer_fps >= self.config.min_viewer_fps + 4 {
                    v.low = 0;
                    // A healthy report on the tier says the tier is
                    // comfortable, not that the original is: probe only
                    // after the dwell, then hand the viewer back and let
                    // the next reports decide. A failed probe costs one
                    // report window, not a permanent 5 fps stream.
                    if now.duration_since(v.started) < MIN_TIER_DWELL {
                        v.good = 0;
                        return ReportAction::Suppress;
                    }
                    v.good += 1;
                    if v.good >= GOOD_REPORTS {
                        viewers.remove(&viewer_id);
                        let empty = viewers.is_empty();
                        drop(viewers);
                        if empty {
                            self.stop_job(&mut inner, sender_id);
                        }
                        info!(sender_id, viewer_id, "transcode: viewer recovered");
                        return ReportAction::Forward;
                    }
                } else {
                    v.low = 0;
                    v.good = 0;
                }
                return ReportAction::Suppress;
            }
            // The job already exists for this sender (another weak
            // viewer started it); adding a viewer costs no extra decode.
            if viewer_fps < self.config.min_viewer_fps {
                viewers.insert(
                    viewer_id,
                    ViewerJob {
                        tier: self.config.first_height,
                        live: false,
                        started: now,
                        since: now,
                        low: 0,
                        good: 0,
                    },
                );
                job.force_idr.store(true, Ordering::Relaxed);
                info!(sender_id, viewer_id, "transcode: viewer joined");
                return ReportAction::Suppress;
            }
            return ReportAction::Forward;
        }

        if !sender_ok || inner.jobs.len() >= self.config.max_transcodes {
            inner.watches.remove(&(viewer_id, sender_id));
            return ReportAction::Forward;
        }

        let step = {
            let watch = inner
                .watches
                .entry((viewer_id, sender_id))
                .or_insert_with(Watch::new);
            watch_step(watch, viewer_fps, self.config.min_viewer_fps, now)
        };
        match step {
            // Holding the report while the start is confirmed keeps the
            // sharer's governor from counting it: one forwarded report
            // stays in its 15-second worst-viewer window and would step
            // the share down for every viewer even though the server is
            // about to take this viewer over. The confirmation window is
            // one report (five seconds), the same order as the sharer's
            // own two-report step.
            WatchStep::Wait => ReportAction::Suppress,
            WatchStep::Forward => ReportAction::Forward,
            WatchStep::Recover => {
                inner.watches.remove(&(viewer_id, sender_id));
                ReportAction::Forward
            }
            WatchStep::Start => {
                inner.watches.remove(&(viewer_id, sender_id));
                if self.start_job(&mut inner, sender_id, channel_id, viewer_id, now) {
                    ReportAction::Suppress
                } else {
                    // Could not get a worker: fall back to the sharer's
                    // own ladder rather than swallowing the report.
                    ReportAction::Forward
                }
            }
        }
    }

    /// The sharer stopped (explicit stop, leave, channel switch, session
    /// end). Drop the job; its worker exits on the stop flag.
    pub fn remove_share(&self, sender_id: u32) {
        let mut inner = self.inner.lock().unwrap();
        for senders in inner.shares.values_mut() {
            senders.remove(&sender_id);
        }
        inner.shares.retain(|_, senders| !senders.is_empty());
        inner.meters.remove(&sender_id);
        inner.watches.retain(|(_, sender), _| *sender != sender_id);
        self.stop_job(&mut inner, sender_id);
    }

    /// A viewer left the channel or ended its session.
    pub fn remove_viewer(&self, viewer_id: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.watches.retain(|(viewer, _), _| *viewer != viewer_id);
        let mut empty = Vec::new();
        for (sender_id, job) in &inner.jobs {
            let mut viewers = job.viewers.lock().unwrap();
            viewers.remove(&viewer_id);
            if viewers.is_empty() {
                empty.push(*sender_id);
            }
        }
        for sender_id in empty {
            self.stop_job(&mut inner, sender_id);
        }
    }

    fn sole_share(inner: &Inner, channel_id: u64) -> Option<u32> {
        let senders = inner.shares.get(&channel_id)?;
        if senders.len() == 1 {
            senders.iter().next().copied()
        } else {
            None
        }
    }

    fn start_job(
        &self,
        inner: &mut Inner,
        sender_id: u32,
        channel_id: u64,
        viewer_id: u32,
        now: Instant,
    ) -> bool {
        // Re-check under the same lock: a watch started when there was a
        // free slot may reach its confirmation after another share took
        // it. `max_transcodes` is host-wide and must hold exactly.
        if inner.jobs.len() >= self.config.max_transcodes {
            return false;
        }
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let job = Arc::new(Job {
            sender_id,
            channel_id,
            tx,
            viewers: Mutex::new(HashMap::from([(
                viewer_id,
                ViewerJob {
                    tier: self.config.first_height,
                    live: false,
                    started: now,
                    since: now,
                    low: 0,
                    good: 0,
                },
            )])),
            stop: AtomicBool::new(false),
            force_idr: AtomicBool::new(true),
        });
        let worker_job = job.clone();
        let registry = self.registry.clone();
        let voice = self.voice.clone();
        let config = self.config.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("transcode-{sender_id}"))
            .spawn(move || run_worker(worker_job, rx, registry, voice, config));
        if let Err(e) = spawned {
            warn!(sender_id, error = %e, "transcode: could not start worker");
            return false;
        }
        inner.jobs.insert(sender_id, job);
        info!(
            sender_id,
            viewer_id,
            channel = channel_id,
            tier = self.config.first_height,
            "transcode: started"
        );
        true
    }

    fn stop_job(&self, inner: &mut Inner, sender_id: u32) {
        if let Some(job) = inner.jobs.remove(&sender_id) {
            job.stop.store(true, Ordering::Relaxed);
            info!(sender_id, "transcode: stopped");
        }
    }

    /// Drop senders/viewers that vanished without a stop notice, and
    /// reap workers that exited on their own.
    fn prune(&self, inner: &mut Inner, now: Instant) {
        let meters = &inner.meters;
        inner.shares.retain(|_, senders| {
            senders.retain(|sender| {
                meters
                    .get(sender)
                    .is_some_and(|m| now.duration_since(m.last_seen) < STALE)
            });
            !senders.is_empty()
        });
        let stale: Vec<u32> = inner
            .meters
            .iter()
            .filter(|(_, m)| now.duration_since(m.last_seen) >= STALE)
            .map(|(sender, _)| *sender)
            .collect();
        for sender_id in stale {
            inner.meters.remove(&sender_id);
            inner.watches.retain(|(_, sender), _| *sender != sender_id);
            self.stop_job(inner, sender_id);
        }
        inner
            .watches
            .retain(|_, watch| now.duration_since(watch.last) < STALE);
        inner
            .jobs
            .retain(|_, job| !job.stop.load(Ordering::Relaxed));
    }
}

/// Stop suppressing the original for every viewer of this job. Called
/// before the worker exits and whenever it has gone two seconds without
/// producing a picture; a live flag left set would freeze the viewer.
fn drop_to_original(job: &Job) {
    let now = Instant::now();
    let mut viewers = job.viewers.lock().unwrap();
    for viewer in viewers.values_mut() {
        if viewer.live {
            viewer.live = false;
            viewer.since = now;
        }
    }
}

fn run_worker(
    job: Arc<Job>,
    rx: Receiver<Vec<u8>>,
    registry: Arc<SessionRegistry>,
    voice: Arc<VoiceManager>,
    config: VideoConfig,
) {
    let mut assembler = VideoAssembler::new();
    let mut pipeline = match Pipeline::new(config.threads) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            warn!(sender_id = job.sender_id, error = %e, "transcode: decoder init failed");
            drop_to_original(&job);
            job.stop.store(true, Ordering::Relaxed);
            return;
        }
    };
    let mut frame_ids: HashMap<u32, u32> = HashMap::new();
    let mut seq: u16 = 0x8000;
    let mut last_input = Instant::now();
    let mut last_output = Instant::now();
    let mut last_keyframe = Instant::now();
    let mut errors: u64 = 0;

    while !job.stop.load(Ordering::Relaxed) {
        // A worker that stopped producing pictures must hand the viewer
        // back to the original stream immediately; otherwise `live`
        // suppresses the original and the viewer freezes on the last
        // transcoded frame. The next keyframe flips it back.
        if last_output.elapsed() > Duration::from_secs(2) {
            drop_to_original(&job);
        }
        let payload = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(payload) => payload,
            Err(RecvTimeoutError::Timeout) => {
                if last_input.elapsed() >= WORKER_IDLE {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        last_input = Instant::now();
        let Ok(frag) = decode_fragment(&payload) else {
            continue;
        };
        assembler.push_fragment(frag);
        while let Some(frame) = assembler.pop_frame() {
            let viewers = job.viewers.lock().unwrap().clone();
            if viewers.is_empty() {
                job.stop.store(true, Ordering::Relaxed);
                break;
            }
            let tiers: HashSet<u32> = viewers.values().map(|v| v.tier).collect();
            if !pipeline.decode(&frame) {
                continue;
            }
            let force = job.force_idr.swap(false, Ordering::Relaxed)
                || last_keyframe.elapsed() >= KEYFRAME_INTERVAL;
            for tier in tiers {
                if !force && !pipeline.due(tier) {
                    continue;
                }
                let encoded = match pipeline.encode(tier, force) {
                    Ok(Some(encoded)) => encoded,
                    Ok(None) => continue,
                    Err(e) => {
                        errors += 1;
                        if errors <= 5 || errors.is_multiple_of(100) {
                            warn!(
                                sender_id = job.sender_id,
                                tier,
                                error = %e,
                                "transcode: encode failed"
                            );
                        }
                        continue;
                    }
                };
                let next = frame_ids.entry(tier).or_insert(0);
                let frame_id = *next;
                *next = next.wrapping_add(1);
                let dgs = fragment_frame_with_max(
                    frame_id,
                    encoded.keyframe,
                    &encoded.data,
                    seq,
                    encoded.timestamp,
                    job.sender_id,
                    MAX_FRAGMENT_PAYLOAD,
                    false,
                );
                seq = seq.wrapping_add(dgs.len() as u16);
                if encoded.keyframe {
                    last_keyframe = Instant::now();
                    let mut live = job.viewers.lock().unwrap();
                    for (viewer_id, v) in &viewers {
                        if v.tier == tier
                            && let Some(current) = live.get_mut(viewer_id)
                        {
                            current.live = true;
                        }
                    }
                }
                for dg in &dgs {
                    for (viewer_id, v) in &viewers {
                        // Re-check membership, not just the job's viewer
                        // set: a viewer who switched channels must not
                        // receive this share through a stale job entry.
                        if v.tier == tier && voice.channel_of(*viewer_id) == Some(job.channel_id) {
                            let _ = registry.send_datagram(*viewer_id, Bytes::from(dg.clone()));
                        }
                    }
                }
                last_output = Instant::now();
            }
        }
    }
    drop_to_original(&job);
    job.stop.store(true, Ordering::Relaxed);
    debug!(sender_id = job.sender_id, "transcode worker exited");
}

/// Decoder + one encoder per tier. Frames are decoded at the sender's
/// rate (H.264 reference chains cannot be skipped); encoders are paced
/// at [`TIER_FPS`].
struct Pipeline {
    decoder: Option<VideoDecoder>,
    threads: u16,
    rgba: Vec<u8>,
    width: usize,
    height: usize,
    need_key: bool,
    tiers: HashMap<u32, Tier>,
}

struct Tier {
    encoder: SoftwareEncoder,
    dims: (usize, usize),
    bgra: Vec<u8>,
    last: Option<Instant>,
    /// Codec clock for THIS tier. One source frame can feed two tiers;
    /// a shared counter would advance each tier's timestamps twice per
    /// frame and make a raw viewer play the stream at double speed.
    timestamp: u32,
}

struct TierFrame {
    keyframe: bool,
    data: Vec<u8>,
    timestamp: u32,
}

impl Pipeline {
    fn new(threads: usize) -> Result<Self, Error> {
        Ok(Self {
            decoder: Some(VideoDecoder::new()?),
            threads: u16::try_from(threads).unwrap_or(1).max(1),
            rgba: Vec::new(),
            width: 0,
            height: 0,
            need_key: true,
            tiers: HashMap::new(),
        })
    }

    fn decode(&mut self, frame: &AssembledFrame) -> bool {
        if self.need_key && !frame.keyframe {
            return false;
        }
        if self.decoder.is_none() {
            self.decoder = VideoDecoder::new().ok();
            if self.decoder.is_none() {
                return false;
            }
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return false;
        };
        match decoder.decode_rgba_with_dims(&frame.data) {
            Ok(Some((rgba, width, height))) => {
                self.rgba = rgba;
                self.width = width;
                self.height = height;
                self.need_key = false;
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!(error = %e, "transcode: decode failed; waiting for a keyframe");
                self.decoder = VideoDecoder::new().ok();
                self.need_key = true;
                self.tiers.clear();
                false
            }
        }
    }

    fn due(&self, tier: u32) -> bool {
        self.tiers
            .get(&tier)
            .and_then(|t| t.last)
            .is_none_or(|last| last.elapsed() >= Duration::from_secs_f32(1.0 / TIER_FPS))
    }

    fn encode(&mut self, tier_height: u32, force: bool) -> Result<Option<TierFrame>, Error> {
        if self.width == 0 || self.height == 0 {
            return Ok(None);
        }
        let th = ((tier_height as usize).min(self.height).max(2)) & !1;
        let tw = ((self.width * th / self.height).max(2)) & !1;
        if !self.tiers.contains_key(&tier_height) {
            let encoder = SoftwareEncoder::with_bitrate_threads(
                tw,
                th,
                bitrate_for(tw, th, TIER_FPS),
                self.threads,
            )?;
            self.tiers.insert(
                tier_height,
                Tier {
                    encoder,
                    dims: (tw, th),
                    bgra: vec![0u8; tw * th * 4],
                    last: None,
                    timestamp: 0,
                },
            );
        }
        let tier = self.tiers.get_mut(&tier_height).expect("just inserted");
        if tier.dims != (tw, th) {
            tier.encoder = SoftwareEncoder::with_bitrate_threads(
                tw,
                th,
                bitrate_for(tw, th, TIER_FPS),
                self.threads,
            )?;
            tier.dims = (tw, th);
            tier.bgra.resize(tw * th * 4, 0);
            tier.last = None;
        }
        if force {
            tier.encoder.force_keyframe();
        }
        if tier.bgra.len() != tw * th * 4 {
            tier.bgra.resize(tw * th * 4, 0);
        }
        scale_rgba_to_bgra(&self.rgba, self.width, self.height, tw, th, &mut tier.bgra);
        let (data, keyframe) = tier.encoder.encode_bgra(&tier.bgra)?;
        tier.last = Some(Instant::now());
        let timestamp = tier.timestamp;
        tier.timestamp = timestamp.wrapping_add((48_000.0 / TIER_FPS) as u32);
        Ok(Some(TierFrame {
            keyframe,
            data,
            timestamp,
        }))
    }
}

/// Nearest-neighbour RGBA → BGRA at `(dw, dh)`. Screen shares are mostly
/// text; a box filter would cost more than it buys and would soften the
/// very edges a viewer is trying to read.
fn scale_rgba_to_bgra(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, dst: &mut [u8]) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    for y in 0..dh {
        let sy = y * sh / dh;
        let src_row = sy * sw * 4;
        let dst_row = y * dw * 4;
        for x in 0..dw {
            let sx = x * sw / dw;
            let si = src_row + sx * 4;
            let di = dst_row + x * 4;
            dst[di] = src[si + 2];
            dst[di + 1] = src[si + 1];
            dst[di + 2] = src[si];
            dst[di + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_marginal_slump_needs_two_reports() {
        let mut w = Watch::new();
        let now = t0();
        assert!(matches!(watch_step(&mut w, 20, 24, now), WatchStep::Wait));
        assert!(matches!(
            watch_step(&mut w, 20, 24, now + Duration::from_secs(5)),
            WatchStep::Start
        ));
    }

    #[test]
    fn a_severe_slump_starts_on_the_first_report() {
        let mut w = Watch::new();
        assert!(matches!(watch_step(&mut w, 5, 24, t0()), WatchStep::Start));
    }

    /// A viewer that is already in a job and reports zero decoded
    /// pictures is stalled, not "not measured yet": the server repairs
    /// the tier instead of forwarding the report to the sharer. A
    /// viewer with no seat is still the sharer's business.
    #[test]
    fn a_stalled_viewer_in_a_job_is_repaired_not_forwarded() {
        let config = VideoConfig {
            transcode: true,
            max_transcodes: 1,
            min_viewer_fps: 24,
            first_height: 120,
            second_height: 90,
            threads: 1,
        };
        let manager = TranscodeManager::new(
            config,
            Arc::new(SessionRegistry::new()),
            Arc::new(VoiceManager::new()),
        );
        let dgs =
            fragment_frame_with_max(0, true, &[0u8; 32], 0, 0, 0, MAX_FRAGMENT_PAYLOAD, false);
        // `route_datagram` hands `note_video` the media payload, after
        // the 11-byte header; the fragmenter emits the whole datagram.
        let media = parley_proto::media::decode(&dgs[0]).expect("media header");
        manager.note_video(1, 7, media.payload);
        assert_eq!(manager.on_report(2, 7, 5), ReportAction::Suppress);
        assert_eq!(manager.on_report(2, 7, 0), ReportAction::Suppress);
        assert_eq!(manager.on_report(3, 7, 0), ReportAction::Forward);
        manager.remove_share(1);
    }

    #[test]
    fn recovery_needs_three_healthy_reports() {
        let mut w = Watch::new();
        let now = t0();
        assert!(matches!(
            watch_step(&mut w, 30, 24, now),
            WatchStep::Forward
        ));
        assert!(matches!(
            watch_step(&mut w, 30, 24, now + Duration::from_secs(5)),
            WatchStep::Forward
        ));
        assert!(matches!(
            watch_step(&mut w, 30, 24, now + Duration::from_secs(10)),
            WatchStep::Recover
        ));
    }

    #[test]
    fn scaling_writes_bgra_at_the_new_size() {
        // 2x1 RGBA: red then blue.
        let src = [255u8, 0, 0, 255, 0, 0, 255, 255];
        let mut dst = vec![0u8; 4];
        scale_rgba_to_bgra(&src, 2, 1, 1, 1, &mut dst);
        assert_eq!(dst, [0, 0, 255, 255], "blue BGRA");
    }

    #[test]
    fn a_transcoded_frame_decodes_at_the_tier_size() {
        let (w, h) = (320usize, 180usize);
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                bgra[i] = (x % 256) as u8;
                bgra[i + 1] = (y % 256) as u8;
                bgra[i + 2] = 128;
                bgra[i + 3] = 255;
            }
        }
        let mut source = SoftwareEncoder::new(w, h).expect("source encoder");
        let (data, key) = source.encode_bgra(&bgra).expect("source encode");
        assert!(key, "first source frame is an IDR");
        let frame = AssembledFrame {
            frame_id: 0,
            keyframe: true,
            high_rate: false,
            data,
        };
        let mut pipeline = Pipeline::new(1).expect("pipeline");
        assert!(pipeline.decode(&frame), "source keyframe decodes");
        let tier = pipeline
            .encode(90, false)
            .expect("tier encode")
            .expect("frame");
        let mut decoder = VideoDecoder::new().expect("tier decoder");
        let (_, dw, dh) = decoder
            .decode_rgba_with_dims(&tier.data)
            .expect("tier decode")
            .expect("tier picture");
        assert_eq!((dw, dh), (160, 90));
    }
}
