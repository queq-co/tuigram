# Headless client (Phase 3)

> What the implemented headless client does, end to end. Code lives in
> [`tuigram-core`](../crates/tuigram-core/src); the manual harness that drives it
> is [`crates/tuigram/examples/repl.rs`](../crates/tuigram/examples/repl.rs).
> Builds on the Phase 2 [login flow](login-flow.md) (the authenticated bridge is
> the starting point here) and the [TDLib bridge](research/tdlib.md#async-bridge-to-tokio).

Scope is the **core client surface, headless**: list chats and a chat's messages,
send and reply, edit and delete, mark read, and the synced compose draft — all as
a unit-testable library API, with no terminal (that is Phase 4). Archived/secret
chats and non-text content stay out of scope and are surfaced explicitly (see
[Out of scope](#out-of-scope)).

## The shape of the problem

TDLib does not hand back "the chat list" or "this chat's messages" as values you
fetch once. It streams a firehose of unsolicited **updates** — `updateNewChat`,
`updateChatPosition`, `updateNewMessage`, `updateMessageSendSucceeded`,
`updateUser`, … — and expects the client to *fold* them into its own maintained
state. A few things are pulled directly (history pages, a single user backfill);
everything else is pushed and must be reduced as it arrives.

So Phase 3 is two halves that meet in the middle:

- a **write side** — typed requests that ask TDLib to do something (send, edit,
  delete, view, set a draft, load more);
- a **read side** — owned stores that fold the resulting updates into ordered,
  deduplicated snapshots the UI will render.

The pieces below are arranged so those two halves never tangle: requests are
segregated per domain, and a **single** task does all the folding.

## The pieces

| Piece | Module | Role |
|---|---|---|
| **Client facade** | [`client`](../crates/tuigram-core/src/client.rs) | Own the account state + the router task; one handle for the app. |
| **Update router** | [`router`](../crates/tuigram-core/src/router.rs) | The one always-on subscriber; classify each update, dispatch O(1) to its reducer. |
| **Chat list** | [`chats`](../crates/tuigram-core/src/chats.rs) | Fold the chat-update family into the ordered Main list (incl. drafts, read state). |
| **Messages** | [`messages`](../crates/tuigram-core/src/messages.rs) | Per-chat history paging + live messages + the send/edit/delete lifecycle. |
| **Users** | [`users`](../crates/tuigram-core/src/users.rs) | Resolve the bare `i64` ids on senders/private chats into named people. |
| **Headless model** | [`model`](../crates/tuigram-core/src/model.rs) | tuigram's own `Chat`/`Message`/`Draft`/… types, projected from TDLib shapes. |

The Phase 2 [bridge](../crates/tuigram-core/src/bridge.rs) underneath stays **pure
transport**: it pumps `tdjson` and exposes the typed `functions::*` request API
plus a `broadcast` update `Stream`. Phase 3 adds no protocol knowledge to it.

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
push it back, a `to_tdlib`): `Chat`, `Message`, `Sender`, `User`, `ChatPosition`,
`FormattedText`/`TextEntity`, `SendState`, `Presence`, and `Draft`.

Content mapping is **total on purpose**. [`MessageContent::from_tdlib`] handles
text and maps **every** other TDLib content variant to `Unsupported(name)` — no
catch-all that could silently mis-map. The discipline mirrors
[`AuthState::from_tdlib`](login-flow.md#state-machine): an unhandled variant
surfaces as a named "unsupported" rather than masquerading as something it isn't,
and adding real support is a deliberate change, not an accident.

## Folding, per domain

Every reducer is **idempotent** — TDLib repeats and reorders updates freely (on
reconnect, on resync, or just because order changed), so re-applying any update
converges to the same state instead of double-counting.

### Chat list — `chats`

[`ChatStore::reduce`] folds the chat-update family (`updateNewChat`,
`updateChatPosition`, `updateChatLastMessage`, `updateChatReadInbox`,
`updateChatDraftMessage`, …) into a maintained list; [`ChatStore::main_list`]
reads back an ordered snapshot. Ordering is by each chat's **Main-list position**;
chats with no Main position simply aren't in the snapshot. [`load_main_list`]
drives paging to pull more of the list on demand — the request side only *asks*
for chats; they arrive asynchronously as updates.

Read state and drafts ride this same family: `updateChatReadInbox` updates the
unread counters surfaced on the `Chat` snapshot, and `updateChatDraftMessage`
sets/updates/clears `Chat.draft`. Scope is the **Main** list only — archived
chats, folders, and secret chats are follow-ups.

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
message (optionally a reply); TDLib creates it optimistically with a temporary id
in [`SendState::Pending`], so it appears at once. The reducer then reconciles in
place: `updateMessageSendSucceeded` swaps the temp id for the server's real one,
`updateMessageSendFailed` flips the same entry to [`SendState::Failed`] — never
blocking on delivery.

**Edit and delete** round out the write side. [`MessageRequests::edit_text`]
replaces a message's text and folds `updateMessageContent` (content swapped in
place); [`MessageRequests::delete`] removes messages for self or, with `revoke`,
for everyone, folding a *permanent* `updateDeleteMessages` — a cache-eviction
delete is ignored so our copy survives. Re-applying a delete of an absent id is a
no-op.

### Read state — `chats` + `messages`

[`MessageRequests::view_messages`] marks a chat's messages read. It is
**advisory**: the call acknowledges the messages to the server and never blocks
the read path. The resulting unread-count change comes back as
`updateChatReadInbox`, folded by the chat store onto the `Chat` snapshot's
counters.

### Users — `users`

A `Sender::User` and a private `Chat` carry only a user id; alone they render as
opaque integers. [`UserStore::reduce`] folds `updateUser` (the full record) and
`updateUserStatus` (presence only), and [`UserStore::display_name`] reads a name
back for whatever id a chat or message holds. Most users arrive unsolicited;
[`UserRequests::get_user`] only backfills an id the stream hasn't announced (e.g.
the sender of a message paged in from history).

## Request seams, segregated per domain

Phase 2 put auth requests behind their own [`AuthRequests`] trait so the login
driver depended only on what it called. Phase 3 keeps that discipline: each domain
owns its **slice** of the `tdlib_rs::functions` surface as its own trait —
[`ChatRequests`], [`MessageRequests`], [`UserRequests`] — rather than one
god-trait on the bridge. [`Bridge`] implements all of them over a live `tdjson`
client (via [`Bridge::id`]); tests implement each with a spy. Logic written
against `C: MessageRequests` (etc.) runs unchanged on either, with no network and
no live `tdjson`. The bridge stays pure transport; a driver depends on exactly the
requests it makes.

The write actions are **one-way**: a request asks TDLib to do something and the
store updates when the resulting update echoes back through the router, so there
is a single fold path for account content. Setting a draft
([`ChatRequests::set_chat_draft_message`], with a `None` draft to clear) is the
same shape — push it, and the snapshot updates via the `updateChatDraftMessage`
echo, idempotently.

## The Client facade

[`Client`](../crates/tuigram-core/src/client.rs) is the long-lived owner that ties
this together so the rest of the app holds **one handle** instead of wiring the
broadcast stream, the router task, and the shared state by hand. It owns the
[`AccountState`] (the composition root for the chat, message, and user stores) and
the single router task that folds into it.

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
- The **one fetch that returns directly** rather than as updates — history paging
  — folds in via [`Client::merge_history`].

**Logout** is the inverse of the login the session opens with: it invalidates the
account session and wipes TDLib's local database, so the next run starts at a
fresh login rather than resuming the persisted session. (The Phase 2 auth state
machine already models the `Closed`/`loggingOut` tail of the
[state machine](login-flow.md#state-machine); logout drives the bridge into it.)

## Out of scope

Carried-forward follow-ups, surfaced rather than silently dropped:

- **Archived chats, chat folders, secret chats** — the chat store maintains the
  **Main** list only; chats without a Main position are not in the snapshot.
- **Non-text message content** — every non-text TDLib content variant projects as
  `MessageContent::Unsupported(name)` (a named placeholder, never a wrong
  mapping). Media rendering is later work.
- **Non-text drafts** — TDLib allows voice/video-note drafts; Phase 3 models a
  **text** draft (the realistic case for a keyboard-driven client), projecting a
  non-text draft with empty text.
- **The deferred Phase 2 login states** — QR login, new-user registration,
  email-based login, and premium purchase remain `AuthState::Unsupported(name)`
  (see [login-flow.md § Out of scope](login-flow.md#out-of-scope)).

## Trying it

There is no UI yet (that is Phase 4). A feature-gated REPL harness drives the full
Phase 3 surface end to end against a real account over stdin — it logs in (reusing
the four Phase 2 pieces), hands the authenticated bridge to the `Client` facade,
and exposes: list chats, open a chat (load + view history), send, reply, edit,
delete, mark read, and log out.

```text
cargo run -p tuigram --example repl --features login-harness
```

It is off by default — excluded from the product binary and from default CI — and
keeps the login harness's secrets discipline: the login code and 2FA password move
straight into their TDLib request and are never logged or stored, TDLib's own
logging is silenced before the first credential-bearing request, and the REPL
never echoes the unsolicited live stream (it prints a chat's messages only when
the operator asks). See [`crates/tuigram/examples/repl.rs`](../crates/tuigram/examples/repl.rs).

[`Route`]: ../crates/tuigram-core/src/router.rs
[`classify`]: ../crates/tuigram-core/src/router.rs
[`UpdateSink`]: ../crates/tuigram-core/src/router.rs
[`UpdateSink::resync_after_lag`]: ../crates/tuigram-core/src/router.rs
[`MessageContent::from_tdlib`]: ../crates/tuigram-core/src/model.rs
[`ChatStore::reduce`]: ../crates/tuigram-core/src/chats.rs
[`ChatStore::main_list`]: ../crates/tuigram-core/src/chats.rs
[`load_main_list`]: ../crates/tuigram-core/src/chats.rs
[`ChatRequests`]: ../crates/tuigram-core/src/chats.rs
[`ChatRequests::set_chat_draft_message`]: ../crates/tuigram-core/src/chats.rs
[`MessageStore`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::send_text`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::edit_text`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::delete`]: ../crates/tuigram-core/src/messages.rs
[`MessageRequests::view_messages`]: ../crates/tuigram-core/src/messages.rs
[`load_history`]: ../crates/tuigram-core/src/messages.rs
[`SendState::Pending`]: ../crates/tuigram-core/src/model.rs
[`SendState::Failed`]: ../crates/tuigram-core/src/model.rs
[`UserStore::reduce`]: ../crates/tuigram-core/src/users.rs
[`UserStore::display_name`]: ../crates/tuigram-core/src/users.rs
[`UserRequests`]: ../crates/tuigram-core/src/users.rs
[`UserRequests::get_user`]: ../crates/tuigram-core/src/users.rs
[`AuthRequests`]: ../crates/tuigram-core/src/auth.rs
[`Bridge`]: ../crates/tuigram-core/src/bridge.rs
[`Bridge::id`]: ../crates/tuigram-core/src/bridge.rs
[`AccountState`]: ../crates/tuigram-core/src/client.rs
[`Client::read`]: ../crates/tuigram-core/src/client.rs
[`Client::bridge`]: ../crates/tuigram-core/src/client.rs
[`Client::start`]: ../crates/tuigram-core/src/client.rs
[`Client::merge_history`]: ../crates/tuigram-core/src/client.rs
