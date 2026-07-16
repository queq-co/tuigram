//! User & contact resolution — the store that turns the bare `i64` ids on
//! senders and private chats into named people.
//!
//! A [`Sender::User`](crate::model::Sender::User) and a
//! [`ChatKind::Private`](crate::model::ChatKind::Private) both carry only a user
//! id; on their own they render as opaque integers. `TDLib` streams the actual user
//! records as `updateUser` (the full record) and `updateUserStatus` (presence
//! only), and expects the client to keep them. [`UserStore`] is that kept state:
//! the single update router folds each user-route update into it via
//! [`UserStore::reduce`], and [`UserStore::get`] / [`UserStore::display_name`]
//! read a name back for whatever id a chat or message snapshot holds.
//!
//! Folding is **idempotent** — `TDLib` repeats `updateUser` on reconnect and resync
//! — so re-applying any update converges rather than accreting state.
//!
//! [`UserRequests`] is this module's slice of the request surface — only the
//! single-user fetch — owned here rather than in `bridge` so the bridge stays
//! pure transport and a driver depends on just the requests it makes, exactly as
//! [`ChatRequests`](crate::chats::ChatRequests) and
//! [`MessageRequests`](crate::messages::MessageRequests) do. Most users arrive
//! unsolicited as updates; [`UserRequests::get_user`] only backfills an id the
//! stream has not announced (e.g. the sender of a message paged in from history).

use std::collections::HashMap;

use tdlib_rs::enums::Update;
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::{Presence, User};

/// The user request seam — tuigram's user slice of the `tdlib_rs::functions`
/// surface, segregated from the auth, chat, and message requests so a driver
/// (and its test double) implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: UserRequests` runs
/// unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over `C: UserRequests`,
// so the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait UserRequests {
    /// Fetch a single user by id, projected to [`User`].
    ///
    /// A backfill for an id the update stream has not announced — most users
    /// arrive unsolicited as `updateUser`, but a sender paged in from history can
    /// reference a user `TDLib` has not pushed yet. `TDLib` also emits the
    /// corresponding `updateUser`, so the store folds the same record through the
    /// router; this returned copy is for the caller that needed it synchronously.
    async fn get_user(&self, user_id: i64) -> Result<User, TdError>;
}

impl UserRequests for Bridge {
    async fn get_user(&self, user_id: i64) -> Result<User, TdError> {
        let tdlib_rs::enums::User::User(user) =
            tdlib_rs::functions::get_user(user_id, self.id()).await?;
        Ok(User::from_tdlib(&user))
    }
}

/// The folded users state: every known user, keyed by id.
#[derive(Debug, Default)]
pub struct UserStore {
    users: HashMap<i64, User>,
}

impl UserStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one user-route update into the store.
    ///
    /// - `updateUser` — the full user record; inserted or replaced.
    /// - `updateUserStatus` — presence only; updates a known user's status (an
    ///   unknown user is ignored, see `set_status`).
    ///
    /// The catch-all stays inert — the router owns classification, this owns only
    /// the fold — so any other variant reaching here is a harmless no-op.
    pub fn reduce(&mut self, update: &Update) {
        match update {
            Update::User(u) => self.upsert(User::from_tdlib(&u.user)),
            Update::UserStatus(u) => self.set_status(u.user_id, Presence::from_tdlib(&u.status)),
            _ => {}
        }
    }

    /// Look up a user by id.
    #[must_use]
    pub fn get(&self, user_id: i64) -> Option<&User> {
        self.users.get(&user_id)
    }

    /// Resolve a user id to a display name for a sender line or chat header.
    /// Falls back to a bare `User {id}` when the user is not (yet) known — so a
    /// caller can render *something* legible before the record arrives, rather
    /// than threading an `Option` through every snapshot.
    #[must_use]
    pub fn display_name(&self, user_id: i64) -> String {
        self.get(user_id)
            .map_or_else(|| format!("User {user_id}"), User::display_name)
    }

    /// Number of known users.
    #[must_use]
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// Whether no users are known yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Insert or replace a user from `updateUser`. `TDLib` sends the full record,
    /// so a replace is correct — including its presence, which a later
    /// `updateUserStatus` then refines.
    fn upsert(&mut self, user: User) {
        self.users.insert(user.id, user);
    }

    /// Fold `updateUserStatus`: refresh just the presence of a known user.
    ///
    /// An unknown user is ignored: `TDLib` announces a user with `updateUser`
    /// before sending its status, so a status for an unknown id is a stale or
    /// out-of-order update, safe to drop — and [`get_user`](UserRequests::get_user)
    /// backfills any user we still lack rather than synthesizing a nameless one
    /// from a presence-only update.
    fn set_status(&mut self, user_id: i64, status: Presence) {
        if let Some(user) = self.users.get_mut(&user_id) {
            user.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatKind, Sender, UserKind};
    use std::cell::Cell;
    use tdlib_rs::enums::{UserStatus as TdUserStatus, UserType as TdUserType};
    use tdlib_rs::types::{
        UpdateUser, UpdateUserStatus, User as TdUser, UserStatusOffline, UserStatusOnline,
    };

    /// A `TDLib` `User` with every field zeroed but id, names, and status — what the
    /// fold and resolution paths exercise.
    fn td_user(id: i64, first: &str, last: &str, status: TdUserStatus) -> TdUser {
        TdUser {
            id,
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            usernames: None,
            phone_number: String::new(),
            status,
            profile_photo: None,
            accent_color_id: 0,
            background_custom_emoji_id: 0,
            upgraded_gift_colors: None,
            profile_accent_color_id: 0,
            profile_background_custom_emoji_id: 0,
            emoji_status: None,
            is_contact: false,
            is_mutual_contact: false,
            is_close_friend: false,
            verification_status: None,
            is_premium: false,
            is_support: false,
            restriction_info: None,
            active_story_state: None,
            restricts_new_chats: false,
            paid_message_star_count: 0,
            have_access: true,
            r#type: TdUserType::Regular,
            language_code: String::new(),
            added_to_attachment_menu: false,
        }
    }

    fn update_user(id: i64, first: &str, last: &str) -> Update {
        Update::User(UpdateUser {
            user: td_user(id, first, last, TdUserStatus::Empty),
        })
    }

    fn user_status(user_id: i64, status: TdUserStatus) -> Update {
        Update::UserStatus(UpdateUserStatus { user_id, status })
    }

    #[test]
    fn update_user_folds_a_named_user() {
        let mut store = UserStore::new();
        store.reduce(&update_user(7, "Ada", "Lovelace"));

        let user = store.get(7).unwrap();
        assert_eq!(user.display_name(), "Ada Lovelace");
        assert_eq!(user.kind, UserKind::Regular);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn repeated_update_user_replaces_in_place() {
        let mut store = UserStore::new();
        store.reduce(&update_user(7, "Ada", "Lovelace"));
        // A renamed re-announcement replaces, never duplicates.
        store.reduce(&update_user(7, "Augusta Ada", "King"));

        assert_eq!(store.get(7).unwrap().display_name(), "Augusta Ada King");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn user_status_updates_presence_of_a_known_user() {
        let mut store = UserStore::new();
        store.reduce(&update_user(7, "Ada", "Lovelace"));
        assert_eq!(store.get(7).unwrap().status, Presence::Never);

        store.reduce(&user_status(
            7,
            TdUserStatus::Online(UserStatusOnline { expires: 99 }),
        ));
        assert_eq!(
            store.get(7).unwrap().status,
            Presence::Online { expires: 99 }
        );

        // A later offline status advances it; the name is untouched.
        store.reduce(&user_status(
            7,
            TdUserStatus::Offline(UserStatusOffline { was_online: 42 }),
        ));
        assert_eq!(
            store.get(7).unwrap().status,
            Presence::Offline { was_online: 42 }
        );
        assert_eq!(store.get(7).unwrap().display_name(), "Ada Lovelace");
    }

    #[test]
    fn status_for_an_unknown_user_is_ignored() {
        let mut store = UserStore::new();
        // No prior updateUser: a presence-only update synthesizes nothing.
        store.reduce(&user_status(
            999,
            TdUserStatus::Online(UserStatusOnline { expires: 5 }),
        ));
        assert!(store.is_empty());
        assert!(store.get(999).is_none());
    }

    #[test]
    fn non_user_updates_are_ignored_by_the_reducer() {
        let mut store = UserStore::new();
        store.reduce(&update_user(7, "Ada", "Lovelace"));
        // A chat-route update reaching the user reducer (shouldn't happen, but the
        // catch-all must be inert) leaves the store untouched.
        store.reduce(&Update::DeleteMessages(
            tdlib_rs::types::UpdateDeleteMessages {
                chat_id: 10,
                message_ids: vec![1],
                is_permanent: true,
                from_cache: false,
            },
        ));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_sender_and_a_private_chat_resolve_to_a_named_user() {
        let mut store = UserStore::new();
        store.reduce(&update_user(7, "Ada", "Lovelace"));

        // A message sender id resolves to a name, not a bare integer.
        let Sender::User(uid) = Sender::User(7) else {
            unreachable!()
        };
        assert_eq!(store.display_name(uid), "Ada Lovelace");

        // A private chat's peer id resolves the same way.
        let ChatKind::Private { user_id } = (ChatKind::Private { user_id: 7 }) else {
            unreachable!()
        };
        assert_eq!(store.display_name(user_id), "Ada Lovelace");

        // An id the store has never seen still renders legibly, not as a panic.
        assert_eq!(store.display_name(404), "User 404");
    }

    /// A spy `UserRequests` that answers a fixed user and counts its calls.
    struct GetUserSpy {
        calls: Cell<u32>,
    }

    impl UserRequests for GetUserSpy {
        async fn get_user(&self, user_id: i64) -> Result<User, TdError> {
            self.calls.set(self.calls.get() + 1);
            Ok(User::from_tdlib(&td_user(
                user_id,
                "Backfilled",
                "User",
                TdUserStatus::Empty,
            )))
        }
    }

    #[tokio::test]
    async fn get_user_backfills_a_single_user_through_the_seam() {
        let spy = GetUserSpy {
            calls: Cell::new(0),
        };
        let user = spy.get_user(7).await.unwrap();
        assert_eq!(user.id, 7);
        assert_eq!(user.display_name(), "Backfilled User");
        assert_eq!(spy.calls.get(), 1);
    }
}
