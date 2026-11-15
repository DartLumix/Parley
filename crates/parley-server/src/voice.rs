//! Voice channel membership state.
//!
//! Membership is per DEVICE (`media_id`), not per account. A user may be
//! signed in on a phone and a desktop, and both may transmit; each has its
//! own media id so decoders and rosters cannot confuse them. Account-wide
//! away text stays keyed by the account.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// media id -> share epoch. The epoch lets a publisher session retire
    /// only the share it actually announced, not a newer share that reused
    /// the same device id after a quick stop/start.
    sharing: Mutex<HashMap<u32, u64>>,
    next_share_epoch: AtomicU64,
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
        let mut sharing = self.sharing.lock().unwrap();
        if sharing.contains_key(&media_id) {
            return false;
        }
        let epoch = self
            .next_share_epoch
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        sharing.insert(media_id, epoch);
        true
    }

    /// Forget the share. True when there was one to forget.
    pub fn stop_sharing(&self, media_id: u32) -> bool {
        self.sharing.lock().unwrap().remove(&media_id).is_some()
    }

    /// Retire the share only if it is still the one this caller announced.
    /// A late close from an old publisher must not stop a replacement share.
    pub fn stop_sharing_if(&self, media_id: u32, epoch: u64) -> bool {
        let mut sharing = self.sharing.lock().unwrap();
        if sharing.get(&media_id) != Some(&epoch) {
            return false;
        }
        sharing.remove(&media_id);
        true
    }

    pub fn sharing_epoch(&self, media_id: u32) -> Option<u64> {
        self.sharing.lock().unwrap().get(&media_id).copied()
    }

    /// Whether this device currently has a live share. The MoQ publisher
    /// loop checks this so a StopShare also retires the media session;
    /// otherwise a client could keep publishing after the control plane
    /// told the room the share ended.
    pub fn is_sharing(&self, media_id: u32) -> bool {
        self.sharing.lock().unwrap().contains_key(&media_id)
    }

    pub fn sharing_snapshot(&self) -> HashSet<u32> {
        self.sharing.lock().unwrap().keys().copied().collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_share_cleanup_cannot_retire_a_replacement() {
        let voice = VoiceManager::new();
        assert!(voice.mark_sharing(7));
        let old_epoch = voice.sharing_epoch(7).expect("first share epoch");

        assert!(voice.stop_sharing(7));
        assert!(voice.mark_sharing(7));
        let new_epoch = voice.sharing_epoch(7).expect("replacement share epoch");
        assert_ne!(old_epoch, new_epoch);

        // A delayed publisher from the old share must not clear the new one.
        assert!(!voice.stop_sharing_if(7, old_epoch));
        assert!(voice.is_sharing(7));

        assert!(voice.stop_sharing_if(7, new_epoch));
        assert!(!voice.is_sharing(7));
    }
}
