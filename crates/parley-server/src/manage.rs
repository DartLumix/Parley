//! Server management: users, channels, roles and channel kicks.
//!
//! Every request is answered with a full [`ManageState`] rather than a
//! delta: the management page is one screen, the tables are small, and a
//! stale list after a failed action is worse than one extra SELECT. A
//! successful mutation is also pushed to every OTHER connected manager,
//! so two open pages cannot disagree.
//!
//! Permissions are enforced here, never in the UI:
//! - `admin` and `mod` may manage users and channels;
//! - only `admin` may change a role or create an admin/mod;
//! - a `mod` may not alter or remove an admin;
//! - nobody may remove or demote the last admin, change their own role,
//!   kick themselves, or remove their own account.

use std::collections::HashSet;
use std::sync::Arc;

use crate::db::{DbHandle, UserInfo, UserRole};
use crate::session::SessionRegistry;
use crate::transcode::TranscodeManager;
use crate::voice::VoiceManager;
use parley_proto::pb::manage_request::Action;
use parley_proto::{
    ChannelList, ChannelMode, ManageRequest, ManageResult, ManageState, ManagedChannel,
    ManagedUser, ServerBody, ServerMessage,
};

pub async fn handle(
    req: ManageRequest,
    actor: &UserInfo,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
) -> ManageResult {
    if !actor.role.can_manage() {
        return fail("not permitted");
    }
    let Some(action) = req.action else {
        return fail("empty management request");
    };

    match action {
        Action::State(_) => match state(db, registry, voice).await {
            Ok(state) => ok(state),
            Err(e) => fail(e),
        },

        Action::AddUser(a) => {
            let username = match clean_name(&a.username, 32) {
                Ok(name) => name,
                Err(e) => return fail(e),
            };
            if a.password.is_empty() {
                return fail("password cannot be empty");
            }
            let Some(role) = parse_role(a.role) else {
                return fail("invalid role");
            };
            if role != UserRole::User && !actor.role.is_admin() {
                return fail("only an admin can add an admin or mod");
            }
            if let Err(e) = db.create_user_with_role(&username, &a.password, role).await {
                return fail(db_error(e));
            }
            publish(actor.id, db, registry, voice).await
        }

        Action::UpdateUser(u) => {
            let target = match db.get_user(u.user_id).await {
                Ok(Some(target)) => target,
                Ok(None) => return fail("user not found"),
                Err(e) => return fail(db_error(e)),
            };
            if !actor.role.is_admin() && target.role.is_admin() {
                return fail("only an admin can change an admin");
            }
            let Some(new_role) = parse_role(u.role) else {
                return fail("invalid role");
            };
            if new_role != target.role {
                if !actor.role.is_admin() {
                    return fail("only an admin can change permissions");
                }
                if target.id == actor.id {
                    return fail("you cannot change your own role");
                }
            }
            let username = match u.username.trim() {
                "" => None,
                raw => match clean_name(raw, 32) {
                    Ok(name) => Some(name),
                    Err(e) => return fail(e),
                },
            };
            let password = (!u.password.is_empty()).then_some(u.password.as_str());
            let role = (new_role != target.role).then_some(new_role);
            if username.is_none() && password.is_none() && role.is_none() {
                return fail("nothing to change");
            }
            if let Err(e) = db
                .update_user(target.id, username.as_deref(), password, role)
                .await
            {
                return fail(db_error(e));
            }
            // A username or role is part of the live session snapshot in
            // the registry (chat stamps, manager filtering). The target
            // reconnects with the new values; its token stays valid.
            if username.is_some() || role.is_some() {
                registry.disconnect_user(target.id);
            }
            publish(actor.id, db, registry, voice).await
        }

        Action::RemoveUser(r) => {
            let target = match db.get_user(r.user_id).await {
                Ok(Some(target)) => target,
                Ok(None) => return fail("user not found"),
                Err(e) => return fail(db_error(e)),
            };
            if target.id == actor.id {
                return fail("you cannot remove your own account");
            }
            if !actor.role.is_admin() && target.role.is_admin() {
                return fail("only an admin can remove an admin");
            }
            if let Err(e) = db.delete_user(target.id).await {
                return fail(db_error(e));
            }
            force_leave(target.id, registry, voice, transcode);
            registry.disconnect_user(target.id);
            publish(actor.id, db, registry, voice).await
        }

        Action::CreateChannel(c) => {
            let name = match clean_name(&c.name, 80) {
                Ok(name) => name,
                Err(e) => return fail(e),
            };
            let Some(music) = parse_mode(c.mode) else {
                return fail("invalid channel mode");
            };
            // Disappearing chat is the default; a manager must opt out
            // explicitly (older clients omit the field and get it on).
            let ephemeral = c.ephemeral.unwrap_or(true);
            if let Err(e) = db.create_channel(&name, music, ephemeral).await {
                return fail(db_error(e));
            }
            broadcast_channel_list(db, registry).await;
            publish(actor.id, db, registry, voice).await
        }

        Action::UpdateChannel(u) => {
            let name = match u.name.trim() {
                "" => None,
                raw => match clean_name(raw, 80) {
                    Ok(name) => Some(name),
                    Err(e) => return fail(e),
                },
            };
            let Some(music) = parse_mode(u.mode) else {
                return fail("invalid channel mode");
            };
            if let Err(e) = db
                .update_channel(u.channel_id, name.as_deref(), Some(music), u.ephemeral)
                .await
            {
                return fail(db_error(e));
            }
            broadcast_channel_list(db, registry).await;
            publish(actor.id, db, registry, voice).await
        }

        Action::DeleteChannel(d) => {
            let members = voice.members_of(d.channel_id);
            // Delete first: a guard failure (unknown channel) must not
            // empty a live room.
            if let Err(e) = db.delete_channel(d.channel_id).await {
                return fail(db_error(e));
            }
            for user_id in members {
                force_leave(user_id, registry, voice, transcode);
            }
            broadcast_channel_list(db, registry).await;
            publish(actor.id, db, registry, voice).await
        }

        Action::KickUser(k) => {
            let target = match db.get_user(k.user_id).await {
                Ok(Some(target)) => target,
                Ok(None) => return fail("user not found"),
                Err(e) => return fail(db_error(e)),
            };
            if target.id == actor.id {
                return fail("you cannot kick yourself");
            }
            if !actor.role.is_admin() && target.role.is_admin() {
                return fail("only an admin can kick an admin");
            }
            if voice.channel_of(target.id) != Some(k.channel_id) {
                return fail("that user is not in the channel");
            }
            // The ordinary leave path, and nothing else: no ban, no
            // announcement beyond the membership change. They may rejoin
            // immediately.
            force_leave(target.id, registry, voice, transcode);
            publish(actor.id, db, registry, voice).await
        }
    }
}

fn ok(state: ManageState) -> ManageResult {
    ManageResult {
        ok: true,
        error: String::new(),
        state: Some(state),
    }
}

fn fail(message: impl Into<String>) -> ManageResult {
    ManageResult {
        ok: false,
        error: message.into(),
        state: None,
    }
}

/// Read the current tables plus the live session/voice facts.
async fn state(
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) -> Result<ManageState, String> {
    let users = db.list_users().await.map_err(db_error)?;
    let online: HashSet<u32> = registry
        .online_users()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut state = ManageState {
        users: users
            .into_iter()
            .map(|u| ManagedUser {
                user_id: u.id,
                username: u.username,
                role: pb_role(u.role),
                online: online.contains(&u.id),
                channel_id: voice.channel_of(u.id).unwrap_or(0),
            })
            .collect(),
        channels: Vec::new(),
    };
    for (id, name, music, ephemeral) in db.list_channels().await.map_err(db_error)? {
        state.channels.push(ManagedChannel {
            channel_id: id,
            name,
            mode: if music {
                ChannelMode::Music as i32
            } else {
                ChannelMode::Speech as i32
            },
            members: voice.members_of(id).len() as u32,
            ephemeral,
        });
    }
    Ok(state)
}

/// Answer the requester with the new state and push it to every other
/// manager. The requester's copy is written by the session loop (its
/// control stream stays ordered), so it is excluded here.
async fn publish(
    actor_id: u32,
    db: &DbHandle,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
) -> ManageResult {
    match state(db, registry, voice).await {
        Ok(state) => {
            registry.broadcast_managers_except(
                actor_id,
                ServerMessage {
                    body: Some(ServerBody::ManageResult(ManageResult {
                        ok: true,
                        error: String::new(),
                        state: Some(state.clone()),
                    })),
                },
            );
            ok(state)
        }
        Err(e) => fail(format!("changed, but the state refresh failed: {e}")),
    }
}

async fn broadcast_channel_list(db: &DbHandle, registry: &Arc<SessionRegistry>) {
    if let Ok(channels) = db.list_channels().await {
        registry.broadcast(ServerMessage {
            body: Some(ServerBody::ChannelList(ChannelList {
                channels: channels
                    .into_iter()
                    .map(|(id, name, music, ephemeral)| parley_proto::Channel {
                        id,
                        name,
                        mode: if music {
                            ChannelMode::Music as i32
                        } else {
                            ChannelMode::Speech as i32
                        },
                        ephemeral,
                    })
                    .collect(),
            })),
        });
    }
}

/// The same teardown a LeaveVoice performs. Used by kick, account removal
/// and channel deletion: membership, share route and transcode seat all
/// die together, and the room hears one ordinary leave.
fn force_leave(
    user_id: u32,
    registry: &Arc<SessionRegistry>,
    voice: &Arc<VoiceManager>,
    transcode: &Arc<TranscodeManager>,
) {
    let Some(channel_id) = voice.leave(user_id) else {
        return;
    };
    if voice.stop_sharing(user_id) {
        crate::server::notify_share_stopped(user_id, channel_id, registry, voice);
    }
    transcode.remove_share(user_id);
    transcode.remove_viewer(user_id);
    registry.broadcast(crate::server::voice_state_message(voice, user_id, 0, false));
}

fn pb_role(role: UserRole) -> i32 {
    match role {
        UserRole::User => parley_proto::Role::User as i32,
        UserRole::Mod => parley_proto::Role::Mod as i32,
        UserRole::Admin => parley_proto::Role::Admin as i32,
    }
}

fn parse_role(role: i32) -> Option<UserRole> {
    match parley_proto::Role::try_from(role).ok()? {
        parley_proto::Role::User => Some(UserRole::User),
        parley_proto::Role::Mod => Some(UserRole::Mod),
        parley_proto::Role::Admin => Some(UserRole::Admin),
    }
}

fn parse_mode(mode: i32) -> Option<bool> {
    match ChannelMode::try_from(mode).ok()? {
        ChannelMode::Speech => Some(false),
        ChannelMode::Music => Some(true),
    }
}

fn clean_name(raw: &str, max: usize) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name cannot be empty".into());
    }
    if name.chars().count() > max {
        return Err(format!("name is longer than {max} characters"));
    }
    Ok(name.to_string())
}

fn db_error(e: crate::error::Error) -> String {
    let text = e.to_string();
    if text.contains("UNIQUE constraint failed: users.username") {
        "username already taken".into()
    } else if text.contains("UNIQUE constraint failed: channels.name") {
        "channel name already taken".into()
    } else {
        text
    }
}
