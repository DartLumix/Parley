//! Voice channel membership state.
//!
//! Membership is per DEVICE (`media_id`), not per account. A user may be
//! signed in on a phone and a desktop, and both may transmit; each has its
//! own media id so decoders and rosters cannot confuse them. Account-wide
//! away text stays keyed by the account.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Away texts are roster decoration, not a message: cap them so one
/// client cannot push unbounded data to every session with every
/// keystroke.
pub const MAX_STATUS_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceMember {
    pub media_id: u32,
    pub user_id: u32,
    pub session_id: u64,
    pub channel_id: u64,
    pub device: i32,
}

#[derive(Default)]
pub struct VoiceManager {
    /// media_id -> the device's current channel
    members: Mutex<HashMap<u32, VoiceMember>>,
    /// media ids with a live screen share.
    sharing: Mutex<HashSet<u32>>,
    /// Mic-muted media ids, announced by SetMute.
    muted: Mutex<HashSet<u32>>,
    /// Away texts (SetStatus), keyed by account.
    statuses: Mutex<HashMap<u32, String>>,
}

impl VoiceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Join (or switch) a channel. Returns the previous membership, if any.
    pub fn join(&self, member: VoiceMember) -> Option<VoiceMember> {
        self.members.lock().unwrap().insert(member.media_id, member)
    }

    /// Leave voice entirely. Returns the membership that was removed.
    pub fn leave(&self, media_id: u32) -> Option<VoiceMember> {
        self.members.lock().unwrap().remove(&media_id)
    }

    /// Remember that this device is sharing. True when this is the
    /// first video datagram of the share, so the caller can announce it
    /// exactly once.
    pub fn mark_sharing(&self, media_id: u32) -> bool {
        self.sharing.lock().unwrap().insert(media_id)
    }

    /// Forget the share. True when there was one to forget.
    pub fn stop_sharing(&self, media_id: u32) -> bool {
        self.sharing.lock().unwrap().remove(&media_id)
    }

    pub fn sharing_snapshot(&self) -> HashSet<u32> {
        self.sharing.lock().unwrap().clone()
    }

    pub fn member(&self, media_id: u32) -> Option<VoiceMember> {
        self.members.lock().unwrap().get(&media_id).copied()
    }

    pub fn channel_of(&self, media_id: u32) -> Option<u64> {
        self.members
            .lock()
            .unwrap()
            .get(&media_id)
            .map(|member| member.channel_id)
    }

    /// Every device currently in `channel_id`.
    pub fn members_of(&self, channel_id: u64) -> Vec<VoiceMember> {
        self.members
            .lock()
            .unwrap()
            .values()
            .filter(|member| member.channel_id == channel_id)
            .copied()
            .collect()
    }

    /// Every device on an account, wherever it is.
    pub fn media_ids_of_user(&self, user_id: u32) -> Vec<u32> {
        self.members
            .lock()
            .unwrap()
            .values()
            .filter(|member| member.user_id == user_id)
            .map(|member| member.media_id)
            .collect()
    }

    /// Record a device's mic-mute announcement. Returns true when the
    /// value actually changed.
    pub fn set_muted(&self, media_id: u32, muted: bool) -> bool {
        if muted {
            self.muted.lock().unwrap().insert(media_id)
        } else {
            self.muted.lock().unwrap().remove(&media_id)
        }
    }

    pub fn is_muted(&self, media_id: u32) -> bool {
        self.muted.lock().unwrap().contains(&media_id)
    }

    /// Drop a departed device's mute preference.
    pub fn clear_muted(&self, media_id: u32) {
        self.muted.lock().unwrap().remove(&media_id);
    }

    /// Record an away status for an account. Returns true when it changed.
    pub fn set_status(&self, user_id: u32, text: &str) -> bool {
        let text = text.trim();
        let mut statuses = self.statuses.lock().unwrap();
        if text.is_empty() {
            return statuses.remove(&user_id).is_some();
        }
        let capped: String = text.chars().take(MAX_STATUS_CHARS).collect();
        if statuses.get(&user_id) == Some(&capped) {
            return false;
        }
        statuses.insert(user_id, capped);
        true
    }

    pub fn status_of(&self, user_id: u32) -> String {
        self.statuses
            .lock()
            .unwrap()
            .get(&user_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn clear_status(&self, user_id: u32) {
        self.statuses.lock().unwrap().remove(&user_id);
    }

    /// Full state for snapshots.
    pub fn snapshot(&self) -> Vec<VoiceMember> {
        self.members.lock().unwrap().values().copied().collect()
    }
}
