# Headless client

> What the implemented headless client does, end to end. Code lives in
> [`tuigram-core`](../crates/tuigram-core/src); the manual harness that drives it
> is [`crates/tuigram/examples/repl.rs`](../crates/tuigram/examples/repl.rs).
> Builds on the Phase 2 [login flow](login-flow.md) (the authenticated bridge is
> the starting point here) and the [TDLib bridge](research/tdlib.md#async-bridge-to-tokio).
>
> *Build order, for context only:* the core surface (chats, messages, send/edit/
> delete) was built first and the extended surface (media, archive/folders,
> search/forward, reactions/pins, chat actions, secret chats) was added after — but
> on the **same** architecture, so they are documented here as one client rather
> than split by when each part landed.

Scope is the **whole client surface, headless**: list chats and a chat's messages
across the Main, Archive, and folder lists; send, reply, edit, delete, and the
synced compose draft; structured (media and rich) message content with a real
download/upload lifecycle; message search and forward; reactions, pins, and chat
actions; and the secret-chat lifecycle. All of it is unit-testable library API with
no terminal (that is Phase 5). Contacts/discovery, media *inside* secret chats, and
the long tail of unmodeled content stay out of scope and are surfaced explicitly
(see [Out of scope](#out-of-scope)).

## The shape of the problem

TDLib does not hand back "the chat list" or "this chat's messages" as values you
fetch once. It streams a firehose of unsolicited **updates** — `updateNewChat`,
`updateChatPosition`, `updateNewMessage`, `updateMessageSendSucceeded`,
`updateFile`, `updateSecretChat`, `updateUser`, … — and expects the client to
*fold* them into its own maintained state. A few things are pulled directly
(history pages, a single user backfill, a search); everything else is pushed and
must be reduced as it arrives.

So the client is two halves that meet in the middle:

- a **write side** — typed requests that ask TDLib to do something (send, edit,
  delete, view, react, pin, download, set a draft, load more);
- a **read side** — owned stores that fold the resulting updates into ordered,
  deduplicated snapshots the UI will render.

The pieces below are arranged so those two halves never tangle: requests are
segregated per domain, and a **single** task does all the folding. That shape is
also why the client grows cheaply — **every** new capability is the same pattern. A
new domain (files, chat actions, secret chats) is a write-seam plus a read-store
the one router folds into; a widened domain (media/reactions/pins/search/forward on
messages; archive/folders on chats) just adds more requests and folds to an
existing one. No central file accretes per-domain knowledge as the surface grows.

## The pieces

| Piece | Module | Role |
|---|---|---|
| **Client facade** | [`client`](../crates/tuigram-core/src/client.rs) | Own the account state + the router task; one handle for the app. |
| **Update router** | [`router`](../crates/tuigram-core/src/router.rs) | The one always-on subscriber; classify each update, dispatch O(1) to its reducer. |
| **Chat list** | [`chats`](../crates/tuigram-core/src/chats.rs) | Fold the chat-update family into the ordered Main/Archive/folder lists (incl. drafts, read state). |
| **Messages** | [`messages`](../crates/tuigram-core/src/messages.rs) | Per-chat history + live messages + the send/edit/delete/media lifecycle, plus reactions, pins, search, and forward. |
| **Files** | [`files`](../crates/tuigram-core/src/files.rs) | The download lifecycle: ask to fetch, fold `updateFile` progress, read the local path back. |
| **Chat actions** | [`actions`](../crates/tuigram-core/src/actions.rs) | Send our typing/recording action; fold others' actions for display. |
| **Secret chats** | [`secret_chats`](../crates/tuigram-core/src/secret_chats.rs) | The end-to-end chat lifecycle behind a `ChatKind::Secret`. |
| **Users** | [`users`](../crates/tuigram-core/src/users.rs) | Resolve the bare `i64` ids on senders/private chats into named people. |
| **Headless model** | [`model`](../crates/tuigram-core/src/model.rs) | tuigram's own `Chat`/`Message`/`MessageContent`/`File`/`SecretChat`/… types, projected from TDLib shapes. |

The Phase 2 [bridge](../crates/tuigram-core/src/bridge.rs) underneath stays **pure
transport**: it pumps `tdjson` and exposes the typed `functions::*` request API
plus a `broadcast` update `Stream`. The client adds no protocol knowledge to it —
every request seam below is just another trait it implements over the same client.

## The single router

TDLib's updates arrive on one `broadcast` stream. The tempting design — let each
subsystem subscribe and clone the whole firehose — costs a full update clone per
subsystem and scatters the account's data path across the codebase. Instead, the
[`Router`](../crates/tuigram-core/src/router.rs) is the **only** always-on
subscriber: it drains the stream once, [`classify`]s each update with a single
match into a routing [`Route`], and dispatches it O(1) to the owning domain's
reducer behind the [`UpdateSink`] seam.

```
broadcast stream ──▶ Router::run ──▶ classify(update) ──▶ reduce_chat
                                                       ├─▶ reduce_message
                                                       ├─▶ reduce_file
                                                       ├─▶ reduce_chat_action
                                                       ├─▶ reduce_secret_chat
                                                       ├─▶ reduce_user
                                                       └─▶ (Ignored: dropped)
```

Three properties make this hold up:

- **The router holds no business logic.** `classify` only tags *who owns* an
  update; which field changes and how state is ordered lives in the domain
  reducer the tag points at. So each reducer is independently unit-tested by
  feeding it synthetic updates directly — never through the router — and this
  file never accretes per-domain knowledge as the surface grows.
- **Classification is a routing match, not a model projection**, so its
  catch-all `Ignored` arm is correct: most of TDLib's hundreds of update variants
  are connectivity/metadata the client does not fold, and a new variant
  defaulting to `Ignored` is simply not routed. (Contrast `model::*::from_tdlib`,
  which is *total on purpose* — see below.)
- **A broadcast lag is handled, never swallowed.** If a slow drain falls behind
  the channel's buffer, the stream yields `Lagged(skipped)`; the router routes
  that to [`UpdateSink::resync_after_lag`] rather than dropping it, because a gap
  in the fold means the snapshot may be stale and must be re-queried.

This is also where **drafts are kept honest**: `updateChatDraftMessage`
classifies to the **chat** reducer, so a synced compose draft physically cannot
reach the message store — it can never be confused with a sent message.

## The headless model

The crate depends on **its own** types, not `tdlib_rs` shapes — the same
insulation Phase 2 gave with `AuthState`. [`model`](../crates/tuigram-core/src/model.rs)
projects each TDLib shape with a `from_tdlib` (and, where the write side needs to
push it back, a `to_tdlib`): `Chat`, `Message`, `MessageContent`, `Sender`,
`User`, `ChatPosition`, `ChatFolderInfo`, `File`/`FileRef`, `Reaction`,
`SecretChat`, `ChatAction`, `FormattedText`/`TextEntity`, `SendState`, `Presence`,
and `Draft`.

[`MessageContent`] carries the real variants — text, `Photo`, `Video`, `Document`,
`Audio`, `Voice`, `Sticker`, `Animation`, `Location`, `Venue`, `Contact`, `Poll` —
each projected from its TDLib shape, with media variants carrying a [`FileRef`] (a
per-session file id) and a caption alongside.

The projection is **total on purpose**. [`MessageContent::from_tdlib`] handles the
variants above and maps **every** other TDLib content variant (games, dice,
service messages, …) to `Unsupported(name)` — no catch-all that could silently
mis-map. The discipline mirrors [`AuthState::from_tdlib`](login-flow.md#state-machine):
an unhandled variant surfaces as a named "unsupported" rather than masquerading as
something it isn't, and adding real support is a deliberate change, not an accident.

## Folding, per domain

Every reducer is **idempotent** — TDLib repeats and reorders updates freely (on
reconnect, on resync, or just because order changed), so re-applying any update
converges to the same state instead of double-counting.

### Chat list — `chats`

[`ChatStore::reduce`] folds the chat-update family (`updateNewChat`,
`updateChatPosition`, `updateChatLastMessage`, `updateChatReadInbox`,
`updateChatDraftMessage`, `updateChatFolders`, …) into a maintained list. A chat
carries a position **per list** it belongs to, so the same store backs every view:
[`ChatStore::main_list`], [`ChatStore::archive_list`], and
[`ChatStore::folder_list`] each read back an ordered snapshot keyed by the
[`ChatListKind`] (`Main` / `Archive` / `Folder(id)`) a position names; a chat with
no position in a given list simply isn't in that snapshot. [`load_main_list`],
[`load_archive_list`], and [`load_folder_list`] drive paging to pull more of each
list on demand — the request side only *asks* for chats; they arrive asynchronously
as updates. Folders themselves arrive as `updateChatFolders` and fold into
[`ChatStore::folders`] as [`ChatFolderInfo`] (id + display title) — the catalogue a
folder view is selected from.

Read state and drafts ride this same family: `updateChatReadInbox` updates the
unread counters surfaced on the `Chat` snapshot, and `updateChatDraftMessage`
sets/updates/clears `Chat.draft`.

### Messages — `messages`

A chat's messages arrive two ways and converge on one view. **History** is
*pulled* a page at a time with `getChatHistory` (it returns messages directly);
**live** messages are *pushed* as `updateNewMessage`. Both land in the same
[`MessageStore`], keyed per chat by message id in a `BTreeMap`, which gives
id-ascending (== chronological, since TDLib assigns ids monotonically per chat)
ordering and **dedupe** for free: a message seen live then re-fetched in a history
page re-inserts in place, not twice. [`load_history`] drives the backward paging;
production folds each page under its lock, never across an `await`.

The **send lifecycle** lives here too. [`MessageRequests::send_text`] posts a text
message (optionally a reply) and [`SendRequests::send_media`] posts an
[`OutgoingMedia`] (a local path + optional caption); either way TDLib creates the
message **optimistically** with a temporary id in [`SendState::Pending`], so it
appears at once. The reducer then reconciles in place: `updateMessageSendSucceeded`
swaps the temp id for the server's real one, `updateMessageSendFailed` flips the
same entry to [`SendState::Failed`] — never blocking on delivery. A media send's
upload streams alongside as `updateFile` (see [Files](#files--the-download-lifecycle)).

**Edit and delete** round out the core write side. [`MessageRequests::edit_text`]
replaces a message's text and folds `updateMessageContent` (content swapped in
place); [`MessageRequests::delete`] removes messages for self or, with `revoke`,
for everyone, folding a *permanent* `updateDeleteMessages` — a cache-eviction
delete is ignored so our copy survives. Re-applying a delete of an absent id is a
no-op.

**Reactions and pins** are one-way writes that reconcile through the router, the
same shape as everything else. [`ReactionRequests::add_message_reaction`] /
`remove_message_reaction` set or clear our emoji reaction; the new counts fold via
`updateMessageInteractionInfo` onto [`Message::reactions`] (a [`Reaction`] per
bucket: kind, count, and whether *we* chose it). [`PinRequests::pin_chat_message`]
/ `unpin_chat_message` pin or unpin; the change folds via
`updateChatPinnedMessage` / `updateMessageIsPinned` into the chat's pinned set.

### Search & forward — `messages`

**Search returns a value, not a fold.** Unlike account content,
[`SearchRequests::search_chat_messages`] (scoped to a chat) and
[`SearchRequests::search_messages`] (account-wide) page their hits into a transient
[`SearchResults`] view via [`search_chat`]/[`search_global`]. Those results
**never** fold into the [`MessageStore`], so a search leaves the loaded history
untouched — the read model has exactly one owner per piece of state, and search is
a query *beside* it, not a mutation of it.

**Forward is an ordinary send.** [`ForwardRequests::forward_messages`] posts the
forwarded copies into the target chat, where they fold through the **same
optimistic send lifecycle** as a fresh message — temporary ids in `Pending`,
reconciled by the send-succeeded/failed echoes. Nothing chat-specific; the target's
history just settles.

### Files — the download lifecycle

A media message does not carry bytes — it carries a [`FileRef`], TDLib's
per-session file id. Fetching the file is the same push/pull split as everything
else:

- **Ask** — [`FileRequests::download_file`] requests the download (at
  [`DOWNLOAD_PRIORITY`]); [`FileRequests::cancel_download_file`] stops it. The
  request only *starts* the transfer; it never blocks on completion.
- **Fold** — progress streams back as `updateFile`, which [`FileStore::reduce`]
  folds into a [`File`] keyed by file id: the local path (empty until a download
  begins writing one), downloaded/total sizes, and the in-progress/completed flags.
  [`FileStore::get`] reads the current state back.

So a caller triggers a download and then reads the **local path** out of the store
once it appears — the file's bytes never pass through tuigram itself. Sending media
is the inverse, handled by the [send lifecycle](#messages--messages) above: the
upload streams as `updateFile` into the very same store.

### Read state — `chats` + `messages`

[`MessageRequests::view_messages`] marks a chat's messages read. It is
**advisory**: the call acknowledges the messages to the server and never blocks
the read path. The resulting unread-count change comes back as
`updateChatReadInbox`, folded by the chat store onto the `Chat` snapshot's
counters.

### Chat actions — `actions`

[`ChatActionRequests::send_chat_action`] broadcasts a [`ChatAction`] (typing,
recording a voice note, uploading a photo, …) — or cancels it. It is **advisory and
best-effort**: the server rebroadcasts it to the chat and expires it after a few
seconds, and TDLib never echoes *our own* action back, so there is nothing to fold
for it — a driver fires it and moves on. *Incoming* actions from other members do
fold, via `updateChatAction` into the [`ChatActionStore`]
([`action`](../crates/tuigram-core/src/actions.rs)/`actors`/`is_acting`), so the
future UI can render "Alice is typing…".

### Secret chats — `secret_chats`

A [`ChatKind::Secret`] chat in the snapshot carries only a `secret_chat_id`; the
encryption state behind it — lifecycle, the key hash for fingerprint verification,
and who opened it — lives in a separate record TDLib streams as `updateSecretChat`,
folded by [`SecretChatStore`] into a [`SecretChat`]. [`SecretChatRequests`]
opens ([`create_new_secret_chat`], returning the new chat) and closes
([`close_secret_chat`]) one; the lifecycle advances **pending → ready → closed**,
each announced as a fresh record the fold overwrites in place.

**Messaging needs nothing secret-chat-specific.** A secret chat is reached by its
ordinary chat id, so text sent and received in one flows through the same
[`MessageStore`] and send lifecycle as any other chat — the one rule the lifecycle
adds is readiness, so a driver gates the compose path on [`SecretChat::is_ready`].
Media *inside* secret chats is out of scope (see below).

### Users — `users`

A `Sender::User` and a private `Chat` carry only a user id; alone they render as
opaque integers. [`UserStore::reduce`] folds `updateUser` (the full record) and
`updateUserStatus` (presence only), and [`UserStore::display_name`] reads a name
back for whatever id a chat or message holds. Most users arrive unsolicited;
[`UserRequests::get_user`] only backfills an id the stream hasn't announced (e.g.
the sender of a message paged in from history).

## Request seams, segregated per domain

Phase 2 put auth requests behind their own [`AuthRequests`] trait so the login
driver depended only on what it called. The client keeps that discipline
everywhere: each domain owns its **slice** of the `tdlib_rs::functions` surface as
its own trait — [`ChatRequests`], [`MessageRequests`], [`SendRequests`],
[`ReactionRequests`], [`PinRequests`], [`SearchRequests`], [`ForwardRequests`],
[`FileRequests`], [`ChatActionRequests`], [`SecretChatRequests`], [`UserRequests`] —
rather than one god-trait on the bridge. [`Bridge`] implements all of them over a
live `tdjson` client (via [`Bridge::id`]); tests implement each with a spy. Logic
written against `C: MessageRequests` (etc.) runs unchanged on either, with no
network and no live `tdjson`. The bridge stays pure transport; a driver depends on
exactly the requests it makes.

The write actions are **one-way**: a request asks TDLib to do something and the
store updates when the resulting update echoes back through the router, so there
is a single fold path for account content. Setting a draft
([`ChatRequests::set_chat_draft_message`], with a `None` draft to clear) is the
same shape — push it, and the snapshot updates via the `updateChatDraftMessage`
echo, idempotently. Search is the one read that deliberately stays *beside* this
path rather than in it, returning a transient view (above).

## The Client facade

[`Client`](../crates/tuigram-core/src/client.rs) is the long-lived owner that ties
this together so the rest of the app holds **one handle** instead of wiring the
broadcast stream, the router task, and the shared state by hand. It owns the
[`AccountState`] (the composition root for the chat, message, user, file, chat-
action, and secret-chat stores) and the single router task that folds into it.

**Lifecycle.** A session is assembled in order: open secure storage
([`SessionStorage`](../crates/tuigram-core/src/session.rs)), run login to `Ready`
([`Login`](../crates/tuigram-core/src/auth.rs)) over the bridge, then hand the
*authenticated* bridge to [`Client::start`], which spawns the router. Login is
interactive, so that step stays with its caller (the harness today, the TUI
later); the facade takes over once the account is authenticated.

- **Reads** go through [`Client::read`], a closure over `&AccountState` so a
  caller composes a snapshot under one lock.
- **Writes** go over the bridge's per-domain request traits ([`Client::bridge`])
  and reconcile through the router.
- The fetches that **return directly** rather than as updates — history paging and
  search — fold in via [`Client::merge_history`] or return a transient
  [`SearchResults`] beside the snapshot.

**Logout** is the inverse of the login the session opens with: it invalidates the
account session and wipes TDLib's local database, so the next run starts at a
fresh login rather than resuming the persisted session. (The Phase 2 auth state
machine already models the `Closed`/`loggingOut` tail of the
[state machine](login-flow.md#state-machine); logout drives the bridge into it.)

## Login states

The login that opens every session is the Phase 2 [login flow](login-flow.md), now
complete: the phone path (number + code + 2FA password), QR login, new-user
registration, and email login are all driven to completion, and premium purchase
is surfaced as an explicit headless dead end. With every TDLib authorization state
mapped, [`AuthState::from_tdlib`] is **total by exhaustive match with no
catch-all** — a state added by a future TDLib is a compile error, not a silent
miss. Full detail is in [login-flow.md](login-flow.md#state-machine).

## Out of scope

Surfaced rather than silently dropped, as the carried-forward follow-ups always are:

- **Contacts & discovery** — adding contacts by phone, looking users up, and the
  rest of the social-graph surface are not modeled; chats are reached by id.
- **Media inside secret chats** — the secret-chat **lifecycle** and **text**
  messaging are in scope, but file transfer within an end-to-end chat is its own
  follow-up; only ordinary chats download/send media here.
- **The long tail of message content** — the structured variants above are handled;
  everything else (games, dice, service/system messages, …) still projects as
  [`MessageContent::Unsupported`]`(name)`, a named placeholder to render plainly,
  never a wrong mapping.
- **Non-text drafts** — TDLib allows voice/video-note drafts; the client models a
  **text** draft (the realistic case for a keyboard-driven client), projecting a
  non-text draft with empty text.

## Trying it

There is no UI yet (that is Phase 5). A feature-gated REPL harness drives the full
client surface end to end against a real account over stdin — it logs in (reusing
the four Phase 2 pieces), hands the authenticated bridge to the `Client` facade,
and exposes: list chats, open a chat (load + view history), send, reply, edit,
delete, mark read, search, forward, download/inspect and send media, list the
archive and folders, react and pin, send a typing action, create/list/close secret
chats, and log out.

```text
cargo run -p tuigram --example repl --features login-harness
```

It is off by default — excluded from the product binary and from default CI — and
keeps the login harness's secrets discipline: the login code and 2FA password move
straight into their TDLib request and are never logged or stored, TDLib's own
logging is silenced before the first credential-bearing request, the REPL never
echoes the unsolicited live stream (it prints a chat's messages only when the
operator asks), and **media is handled by local path only** — the harness never
opens, reads, or logs file bytes. See
[`crates/tuigram/examples/repl.rs`](../crates/tuigram/examples/repl.rs).

[`Route`]: ../crates/tuigram-core/src/router.rs
[`classify`]: ../crates/tuigram-core/src/router.rs
[`UpdateSink`]: ../crates/tuigram-core/src/router.rs
[`UpdateSink::resync_after_lag`]: ../crates/tuigram-core/src/router.rs
[`MessageContent`]: ../crates/tuigram-core/src/model.rs
[`MessageContent::from_tdlib`]: ../crates/tuigram-core/src/model.rs
[`MessageContent::Unsupported`]: ../crates/tuigram-core/src/model.rs
[`FileRef`]: ../crates/tuigram-core/src/model.rs
[`File`]: ../crates/tuigram-core/src/model.rs
[`OutgoingMedia`]: ../crates/tuigram-core/src/model.rs
[`Reaction`]: ../crates/tuigram-core/src/model.rs
[`Message::reactions`]: ../crates/tuigram-core/src/model.rs
[`ChatFolderInfo`]: ../crates/tuigram-core/src/model.rs
[`ChatListKind`]: ../crates/tuigram-core/src/model.rs
[`ChatAction`]: ../crates/tuigram-core/src/model.rs
[`ChatKind::Secret`]: ../crates/tuigram-core/src/model.rs
[`SecretChat`]: ../crates/tuigram-core/src/model.rs
[`SecretChat::is_ready`]: ../crates/tuigram-core/src/model.rs
[`SendState::Pending`]: ../crates/tuigram-core/src/model.rs
[`SendState::Failed`]: ../crates/tuigram-core/src/model.rs
[`ChatStore::reduce`]: ../crates/tuigram-core/src/chats.rs
[`ChatStore::main_list`]: ../crates/tuigram-core/src/chats.rs
[`ChatStore::archive_list`]: ../crates/tuigram-core/src/chats.rs
[`ChatStore::folder_list`]: ../crates/tuigram-core/src/chats.rs
[`ChatStore::folders`]: ../crates/tuigram-core/src/chats.rs
[`load_main_list`]: ../crates/tuigram-core/src/chats.rs
[`load_archive_list`]: ../crates/tuigram-core/src/chats.rs
[`load_folder_list`]: ../crates/tuigram-core/src/chats.rs
[`ChatRequests`]: ../crates/tuigram-core/src/chats.rs
[`ChatRequests::set_chat_draft_message`]: ../crates/tuigram-core/src/chats.rs
[`MessageStore`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::send_text`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::edit_text`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::delete`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::view_messages`]: ../crates/tuigram-core/src/messages.rs
[`SendRequests`]: ../crates/tuigram-core/src/messages.rs
[`SendRequests::send_media`]: ../crates/tuigram-core/src/messages.rs
[`ReactionRequests`]: ../crates/tuigram-core/src/messages.rs
[`ReactionRequests::add_message_reaction`]: ../crates/tuigram-core/src/messages.rs
[`PinRequests`]: ../crates/tuigram-core/src/messages.rs
[`PinRequests::pin_chat_message`]: ../crates/tuigram-core/src/messages.rs
[`SearchRequests`]: ../crates/tuigram-core/src/messages.rs
[`SearchRequests::search_chat_messages`]: ../crates/tuigram-core/src/messages.rs
[`SearchRequests::search_messages`]: ../crates/tuigram-core/src/messages.rs
[`SearchResults`]: ../crates/tuigram-core/src/messages.rs
[`search_chat`]: ../crates/tuigram-core/src/messages.rs
[`search_global`]: ../crates/tuigram-core/src/messages.rs
[`ForwardRequests`]: ../crates/tuigram-core/src/messages.rs
[`ForwardRequests::forward_messages`]: ../crates/tuigram-core/src/messages.rs
[`load_history`]: ../crates/tuigram-core/src/messages.rs
[`FileRequests`]: ../crates/tuigram-core/src/files.rs
[`FileRequests::download_file`]: ../crates/tuigram-core/src/files.rs
[`FileRequests::cancel_download_file`]: ../crates/tuigram-core/src/files.rs
[`DOWNLOAD_PRIORITY`]: ../crates/tuigram-core/src/files.rs
[`FileStore`]: ../crates/tuigram-core/src/files.rs
[`FileStore::reduce`]: ../crates/tuigram-core/src/files.rs
[`FileStore::get`]: ../crates/tuigram-core/src/files.rs
[`ChatActionRequests`]: ../crates/tuigram-core/src/actions.rs
[`ChatActionRequests::send_chat_action`]: ../crates/tuigram-core/src/actions.rs
[`ChatActionStore`]: ../crates/tuigram-core/src/actions.rs
[`SecretChatStore`]: ../crates/tuigram-core/src/secret_chats.rs
[`SecretChatRequests`]: ../crates/tuigram-core/src/secret_chats.rs
[`create_new_secret_chat`]: ../crates/tuigram-core/src/secret_chats.rs
[`close_secret_chat`]: ../crates/tuigram-core/src/secret_chats.rs
[`UserStore::reduce`]: ../crates/tuigram-core/src/users.rs
[`UserStore::display_name`]: ../crates/tuigram-core/src/users.rs
[`UserRequests`]: ../crates/tuigram-core/src/users.rs
[`UserRequests::get_user`]: ../crates/tuigram-core/src/users.rs
[`AuthRequests`]: ../crates/tuigram-core/src/auth.rs
[`AuthState::from_tdlib`]: ../crates/tuigram-core/src/auth.rs
[`Bridge`]: ../crates/tuigram-core/src/bridge.rs
[`Bridge::id`]: ../crates/tuigram-core/src/bridge.rs
[`AccountState`]: ../crates/tuigram-core/src/client.rs
[`Client::read`]: ../crates/tuigram-core/src/client.rs
[`Client::bridge`]: ../crates/tuigram-core/src/client.rs
[`Client::start`]: ../crates/tuigram-core/src/client.rs
[`Client::merge_history`]: ../crates/tuigram-core/src/client.rs
