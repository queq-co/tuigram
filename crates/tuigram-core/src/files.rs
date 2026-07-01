//! File transfer state — the store that turns the bare file ids on media content
//! into something a caller can download, observe, and open.
//!
//! A photo, video, or document carries only a [`FileRef`](crate::model::FileRef);
//! the bytes and the live transfer state live here. TDLib streams every change to
//! a file's local/remote copy as `updateFile`, and expects the client to keep the
//! latest. [`FileStore`] is that kept state: the single update router folds each
//! file-route update into it via [`FileStore::reduce`], and [`FileStore::get`]
//! reads back the current [`File`] for whatever id a media snapshot holds.
//!
//! Folding is **idempotent** — TDLib re-emits `updateFile` repeatedly as a
//! transfer progresses and on resync — so re-applying any update converges on the
//! newest record rather than accreting state.
//!
//! [`FileRequests`] is this module's slice of the request surface — start a
//! download, cancel one, fetch a file's current state — owned here rather than in
//! `bridge` so the bridge stays pure transport and a driver depends on just the
//! requests it makes, exactly as [`UserRequests`](crate::users::UserRequests) and
//! [`ChatRequests`](crate::chats::ChatRequests) do. The progress that follows a
//! download arrives unsolicited as `updateFile` and folds through the router;
//! [`FileRequests::get_file`] only backfills a file the stream has not announced.

use std::collections::HashMap;

use tdlib_rs::enums::Update;
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::File;

/// Default download priority for [`FileRequests::download_file`]. TDLib accepts
/// `1..=32` (higher downloads first when several are queued); a single
/// interactive download has nothing to race, so the top priority is the simplest
/// sensible default.
pub const DOWNLOAD_PRIORITY: i32 = 32;

/// Grace period, in seconds, protecting freshly-accessed files from a retention
/// sweep ([`StorageRequests::sweep_chat_media`]). `optimizeStorage` never deletes a
/// file used within `immunity_delay` of the call, so a file the user just opened (or
/// that is mid-download) survives a sweep whose TTL would otherwise catch it. A
/// minute is ample for that race without meaningfully weakening the policy.
pub const SWEEP_IMMUNITY_DELAY: i32 = 60;

/// The file request seam — tuigram's file slice of the `tdlib_rs::functions`
/// surface, segregated from the auth, chat, message, and user requests so a
/// driver (and its test double) implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a mock. Logic written against `C: FileRequests` runs
/// unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over `C: FileRequests`,
// so the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait FileRequests {
    /// Start downloading a file, returning its state at the moment the request is
    /// accepted.
    ///
    /// The download runs asynchronously: TDLib streams progress as `updateFile`,
    /// which the router folds into the [`FileStore`], so a caller observes
    /// completion there rather than awaiting this. The returned [`File`] is the
    /// initial snapshot (typically `is_downloading_active`), for a caller that
    /// wants to reflect the started state immediately.
    async fn download_file(&self, file_id: i32, priority: i32) -> Result<File, TdError>;

    /// Cancel an in-progress download.
    ///
    /// When `only_if_pending` is true the cancel applies only to a download that
    /// has not started transferring yet, leaving an active one running; false
    /// cancels regardless.
    async fn cancel_download_file(
        &self,
        file_id: i32,
        only_if_pending: bool,
    ) -> Result<(), TdError>;

    /// Fetch a single file's current state by id.
    ///
    /// A backfill for an id the update stream has not announced — most files
    /// arrive folded from message content and `updateFile`, but a caller can need
    /// a file's state synchronously before the stream has touched it.
    async fn get_file(&self, file_id: i32) -> Result<File, TdError>;
}

impl FileRequests for Bridge {
    async fn download_file(&self, file_id: i32, priority: i32) -> Result<File, TdError> {
        // offset 0 / limit 0: download the whole file from the start.
        // synchronous false: return as soon as the download is queued and let
        // `updateFile` carry progress, rather than blocking until it completes.
        let tdlib_rs::enums::File::File(file) =
            tdlib_rs::functions::download_file(file_id, priority, 0, 0, false, self.id()).await?;
        Ok(File::from_tdlib(&file))
    }

    async fn cancel_download_file(
        &self,
        file_id: i32,
        only_if_pending: bool,
    ) -> Result<(), TdError> {
        tdlib_rs::functions::cancel_download_file(file_id, only_if_pending, self.id()).await
    }

    async fn get_file(&self, file_id: i32) -> Result<File, TdError> {
        let tdlib_rs::enums::File::File(file) =
            tdlib_rs::functions::get_file(file_id, self.id()).await?;
        Ok(File::from_tdlib(&file))
    }
}

/// The download-cache retention seam — the slice of `optimizeStorage` tuigram uses
/// to expire old downloaded media (#120), segregated from [`FileRequests`] so a
/// retention driver depends only on the sweep, not on downloads.
///
/// The one operation is a **scoped** sweep: delete files, older than a TTL, that
/// belong to a given set of chats. The scoping is deliberate — it is how per-kind
/// retention (private/groups/channels, each with its own TTL) is expressed, running
/// one sweep per kind over that kind's chats. A sweep over *all* files would ignore
/// the per-kind policy, so this seam never offers an unscoped variant.
///
/// [`Bridge`] implements it over a live `tdjson` client; tests implement it with a
/// mock, exactly as [`FileRequests`] is exercised.
// Internal seam: every consumer is in-crate and generic over `C: StorageRequests`,
// so the missing `Send` bound this lint wants is not a concern here.
#[allow(async_fn_in_trait)]
pub trait StorageRequests {
    /// Delete downloaded files belonging to `chat_ids` that have not been accessed
    /// within `ttl` seconds, keeping anything used within [`SWEEP_IMMUNITY_DELAY`].
    ///
    /// Maps to `optimizeStorage` with no size or count limit (`ttl` is the only
    /// bound) and no file-type filter (all media). **`chat_ids` must be non-empty** —
    /// `optimizeStorage` treats an empty list as *every* chat, which would apply one
    /// kind's TTL globally, so the caller filters empty kinds out rather than passing
    /// them here. The freed-space statistics TDLib returns are discarded; the sweep
    /// is fire-and-forget maintenance.
    async fn sweep_chat_media(&self, ttl: i32, chat_ids: Vec<i64>) -> Result<(), TdError>;
}

impl StorageRequests for Bridge {
    async fn sweep_chat_media(&self, ttl: i32, chat_ids: Vec<i64>) -> Result<(), TdError> {
        // size -1 / count -1: no size or count cap, TTL is the only limit.
        // file_types []: every media type. exclude_chat_ids []: nothing exempted.
        // return_deleted_file_statistics false / chat_limit 0: we ignore the report.
        tdlib_rs::functions::optimize_storage(
            -1,
            ttl,
            -1,
            SWEEP_IMMUNITY_DELAY,
            Vec::new(),
            chat_ids,
            Vec::new(),
            false,
            0,
            self.id(),
        )
        .await
        .map(|_stats| ())
    }
}

/// The folded file state: every known file, keyed by id.
#[derive(Debug, Default)]
pub struct FileStore {
    files: HashMap<i32, File>,
}

impl FileStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one file-route update into the store.
    ///
    /// `updateFile` carries the file's newest full record; it is inserted or
    /// replaced. Any other variant reaching here is a harmless no-op — the router
    /// owns classification, this owns only the fold — so a non-file update (which
    /// the router never routes here) is ignored rather than mishandled.
    pub fn reduce(&mut self, update: &Update) {
        if let Update::File(u) = update {
            self.upsert(File::from_tdlib(&u.file));
        }
    }

    /// Look up a file by the id a [`FileRef`](crate::model::FileRef) carries.
    #[must_use]
    pub fn get(&self, file_id: i32) -> Option<&File> {
        self.files.get(&file_id)
    }

    /// Number of known files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no files are known yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Insert or replace a file from `updateFile`. TDLib sends the full record on
    /// every change, so a replace is correct — each emission supersedes the last.
    fn upsert(&mut self, file: File) {
        self.files.insert(file.id, file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileRef;
    use std::cell::{Cell, RefCell};
    use tdlib_rs::types::{File as TdFile, LocalFile, RemoteFile, UpdateFile};

    /// A TDLib `File` with the local/remote sub-records the projection reads.
    fn td_file(id: i32, size: i64, path: &str, downloaded: i64, completed: bool) -> TdFile {
        TdFile {
            id,
            size,
            expected_size: size,
            local: LocalFile {
                path: path.to_owned(),
                can_be_downloaded: true,
                can_be_deleted: true,
                is_downloading_active: !completed && downloaded > 0,
                is_downloading_completed: completed,
                download_offset: 0,
                downloaded_prefix_size: downloaded,
                downloaded_size: downloaded,
            },
            remote: RemoteFile::default(),
        }
    }

    fn update_file(file: TdFile) -> Update {
        Update::File(UpdateFile { file })
    }

    #[test]
    fn update_file_folds_a_file() {
        let mut store = FileStore::new();
        store.reduce(&update_file(td_file(7, 1000, "", 0, false)));

        let file = store.get(7).unwrap();
        assert_eq!(file.id, 7);
        assert_eq!(file.total_size(), 1000);
        assert!(!file.is_present());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn progress_updates_replace_in_place_until_complete() {
        let mut store = FileStore::new();
        // Download announced, then partway, then finished writing to a path.
        store.reduce(&update_file(td_file(7, 1000, "", 0, false)));
        store.reduce(&update_file(td_file(7, 1000, "", 400, false)));
        assert_eq!(store.len(), 1);
        assert!(store.get(7).unwrap().is_downloading_active);
        assert!(!store.get(7).unwrap().is_present());

        store.reduce(&update_file(td_file(7, 1000, "/tmp/dl/7.jpg", 1000, true)));
        let file = store.get(7).unwrap();
        assert!(file.is_downloading_completed);
        assert!(file.is_present());
        assert_eq!(file.local_path, "/tmp/dl/7.jpg");
        // Re-applying the completed record is idempotent, never a duplicate.
        store.reduce(&update_file(td_file(7, 1000, "/tmp/dl/7.jpg", 1000, true)));
        assert_eq!(store.len(), 1);
    }

    /// A TDLib `File` mid-upload: bytes on the remote side, no local download
    /// state. Mirrors what `updateFile` carries while an outgoing media message's
    /// file is being sent.
    fn td_uploading_file(id: i32, size: i64, uploaded: i64, completed: bool) -> TdFile {
        TdFile {
            id,
            size,
            expected_size: size,
            local: LocalFile::default(),
            remote: RemoteFile {
                uploaded_size: uploaded,
                is_uploading_active: !completed && uploaded > 0,
                is_uploading_completed: completed,
                ..RemoteFile::default()
            },
        }
    }

    #[test]
    fn upload_progress_folds_in_place_until_complete() {
        // The upload side of `updateFile`, the path an outgoing media send takes:
        // announced, partway, then fully uploaded — each emission replaces the last.
        let mut store = FileStore::new();
        store.reduce(&update_file(td_uploading_file(7, 1000, 0, false)));
        store.reduce(&update_file(td_uploading_file(7, 1000, 400, false)));
        assert_eq!(store.len(), 1);
        let mid = store.get(7).unwrap();
        assert!(mid.is_uploading_active);
        assert!(!mid.is_uploading_completed);
        assert_eq!(mid.uploaded_size, 400);

        store.reduce(&update_file(td_uploading_file(7, 1000, 1000, true)));
        let done = store.get(7).unwrap();
        assert!(done.is_uploading_completed);
        assert!(!done.is_uploading_active);
        // Re-applying the completed record is idempotent, never a duplicate.
        store.reduce(&update_file(td_uploading_file(7, 1000, 1000, true)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn is_present_requires_a_path_not_just_completion() {
        // A completed flag with no local path is not yet openable.
        let file = File::from_tdlib(&td_file(7, 10, "", 10, true));
        assert!(!file.is_present());
    }

    #[test]
    fn total_size_falls_back_to_expected_when_size_unknown() {
        let mut td = td_file(7, 0, "", 0, false);
        td.expected_size = 512;
        let file = File::from_tdlib(&td);
        assert_eq!(file.total_size(), 512);
    }

    #[test]
    fn a_file_ref_resolves_to_the_folded_file() {
        let mut store = FileStore::new();
        store.reduce(&update_file(td_file(7, 1000, "/tmp/7", 1000, true)));

        // The id a media `FileRef` carries reads back the live file state.
        let file_ref = FileRef::new(7);
        assert!(store.get(file_ref.id).unwrap().is_present());
        // An id the store has never seen is absent, not a panic.
        assert!(store.get(404).is_none());
    }

    #[test]
    fn non_file_updates_are_ignored_by_the_reducer() {
        let mut store = FileStore::new();
        store.reduce(&update_file(td_file(7, 1, "", 0, false)));
        // A message-route update reaching the file reducer (shouldn't happen, but
        // the catch-all must be inert) leaves the store untouched.
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

    /// A spy `FileRequests` that records its calls and answers a fixed file.
    #[derive(Default)]
    struct FileSpy {
        downloaded: Cell<Option<(i32, i32)>>,
        cancelled: Cell<Option<(i32, bool)>>,
        got: Cell<Option<i32>>,
    }

    impl FileRequests for FileSpy {
        async fn download_file(&self, file_id: i32, priority: i32) -> Result<File, TdError> {
            self.downloaded.set(Some((file_id, priority)));
            Ok(File::from_tdlib(&td_file(file_id, 1000, "", 0, false)))
        }

        async fn cancel_download_file(
            &self,
            file_id: i32,
            only_if_pending: bool,
        ) -> Result<(), TdError> {
            self.cancelled.set(Some((file_id, only_if_pending)));
            Ok(())
        }

        async fn get_file(&self, file_id: i32) -> Result<File, TdError> {
            self.got.set(Some(file_id));
            Ok(File::from_tdlib(&td_file(
                file_id, 1000, "/tmp/x", 1000, true,
            )))
        }
    }

    #[tokio::test]
    async fn download_drives_the_seam_with_id_and_priority() {
        let spy = FileSpy::default();
        let file = spy.download_file(7, DOWNLOAD_PRIORITY).await.unwrap();
        assert_eq!(file.id, 7);
        assert_eq!(spy.downloaded.get(), Some((7, DOWNLOAD_PRIORITY)));
    }

    #[tokio::test]
    async fn cancel_and_get_drive_the_seam() {
        let spy = FileSpy::default();
        spy.cancel_download_file(7, true).await.unwrap();
        assert_eq!(spy.cancelled.get(), Some((7, true)));

        let file = spy.get_file(7).await.unwrap();
        assert!(file.is_present());
        assert_eq!(spy.got.get(), Some(7));
    }

    /// A spy `StorageRequests` recording the ttl and chats it was asked to sweep.
    #[derive(Default)]
    struct StorageSpy {
        swept: RefCell<Option<(i32, Vec<i64>)>>,
    }

    impl StorageRequests for StorageSpy {
        async fn sweep_chat_media(&self, ttl: i32, chat_ids: Vec<i64>) -> Result<(), TdError> {
            *self.swept.borrow_mut() = Some((ttl, chat_ids));
            Ok(())
        }
    }

    #[tokio::test]
    async fn sweep_drives_the_seam_with_ttl_and_scoped_chats() {
        let spy = StorageSpy::default();
        // A 3-day TTL scoped to two channels — the ttl in seconds and the exact chat
        // set reach the seam.
        spy.sweep_chat_media(3 * 86_400, vec![10, 11])
            .await
            .unwrap();
        assert_eq!(spy.swept.into_inner(), Some((3 * 86_400, vec![10, 11])));
    }
}
