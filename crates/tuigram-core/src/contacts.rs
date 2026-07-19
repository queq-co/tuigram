//! Contact search — resolving a typed name/username to a user id (#197).
//!
//! The REPL's `secret-new <user_id>` targets an arbitrary user by a free-typed
//! id; the TUI has no such free-text entry, so starting a secret chat with
//! someone outside the open chat list needs a search-by-name picker instead.
//! `TDLib`'s `searchContacts` answers with matching ids only (no name; the
//! existing [`UserStore`](crate::users::UserStore) already resolves those, via
//! [`UserRequests::get_user`](crate::users::UserRequests::get_user) as a
//! backfill for anyone the update stream has not announced) — so this seam
//! stays a thin id lookup, exactly like [`UserRequests`](crate::users::UserRequests)
//! and the other per-domain request traits.

use tdlib_rs::enums::Users as TdUsers;
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;

/// The contact-search request seam — tuigram's slice of the `tdlib_rs::functions`
/// surface for resolving a typed query to contact ids, segregated from the
/// auth, chat, message, and user requests so a driver (and its test double)
/// implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: ContactRequests`
/// runs unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over the trait, so the
// lack of a caller-controllable `Send` bound (the reason this lint fires) is not
// a concern here.
#[allow(async_fn_in_trait)]
pub trait ContactRequests {
    /// Search this account's contacts for `query` (name or username substring),
    /// returning up to `limit` matching user ids in `TDLib`'s ranked order.
    ///
    /// `TDLib` returns ids only; the caller resolves each to a display name
    /// through [`UserStore`](crate::users::UserStore) /
    /// [`UserRequests::get_user`](crate::users::UserRequests::get_user), the
    /// same backfill every other id-only result (senders, private-chat peers)
    /// already goes through.
    async fn search_contacts(&self, query: String, limit: i32) -> Result<Vec<i64>, TdError>;
}

impl ContactRequests for Bridge {
    async fn search_contacts(&self, query: String, limit: i32) -> Result<Vec<i64>, TdError> {
        let TdUsers::Users(users) =
            tdlib_rs::functions::search_contacts(query, limit, self.id()).await?;
        Ok(users.user_ids)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A spy `ContactRequests` recording each query so a test asserts the seam is
    /// driven without a live `tdjson`.
    #[derive(Default)]
    struct ContactSearchSpy {
        queried: RefCell<Vec<(String, i32)>>,
        answer: Vec<i64>,
    }

    impl ContactRequests for ContactSearchSpy {
        async fn search_contacts(&self, query: String, limit: i32) -> Result<Vec<i64>, TdError> {
            self.queried.borrow_mut().push((query, limit));
            Ok(self.answer.clone())
        }
    }

    #[tokio::test]
    async fn seam_searches_contacts_and_returns_matching_ids() {
        let spy = ContactSearchSpy {
            answer: vec![7, 42],
            ..Default::default()
        };
        let ids = spy.search_contacts("ada".to_owned(), 10).await.unwrap();

        assert_eq!(ids, vec![7, 42]);
        assert_eq!(*spy.queried.borrow(), vec![("ada".to_owned(), 10)]);
    }

    #[tokio::test]
    async fn an_empty_query_still_reaches_the_seam() {
        let spy = ContactSearchSpy::default();
        let ids = spy.search_contacts(String::new(), 20).await.unwrap();
        assert!(ids.is_empty());
        assert_eq!(*spy.queried.borrow(), vec![(String::new(), 20)]);
    }
}
