# Contributing

Teamspace is an independent native Rust (egui) desktop app. This file holds the
contracts every change must keep. Each section is meant to be linked by its
heading. For what the app does and how to run it, see [README.md](README.md).

- [Layout & ownership](#layout--ownership)
- [Building & verifying](#building--verifying)
- [Persistence](#persistence)
- [Theme & UI](#theme--ui)
- [Accessibility](#accessibility)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Testing](#testing)
- [Security & tokens](#security--tokens)
- [Microsoft 365 integration](#microsoft-365-integration)
- [Release & packaging](#release--packaging)

## Layout & ownership

- `src/features/` owns runtime behavior: keep it inside the owning feature.
- `src/app/` owns the shell and lifecycle code.
- `src/ui/` owns reusable egui code.
- `src/shared/` owns shared models.
- `src/data/` owns immutable preview seed data.
- `src/integrations/` owns provider-specific protocol code (transport, Entra
  authorization, Microsoft 365 adapters).
- Runtime user data belongs in the platform app-data location, never in this
  repository.
- Do not commit generated screenshots or build output (see
  [Testing](#screenshots)).

## Building & verifying

Use Cargo directly for builds and checks. `scripts/bundle-macos.sh` only
packages an already built binary (see [Release & packaging](#release--packaging)).

There is no hosted CI. Before pushing or opening a pull request, run every gate
below locally:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features screenshot -- -D warnings
cargo clippy --locked --all-targets --features os-notifications -- -D warnings
cargo clippy --locked --lib --features fuzzing -- -D warnings
cargo clippy --locked --all-targets --features ui-snapshots -- -D warnings   # macOS only
cargo test --locked --no-fail-fast
cargo build --locked --release
```

Also run, when relevant: the MSRV check (`cargo check --locked --all-targets` on
the `rust-version` in `Cargo.toml`) after raising a dependency, `cargo deny check`
after changing dependencies, and the macOS-only snapshot comparison after a visual
change (see [Testing](#testing)).

Cargo features: `screenshot` (off-screen renderer), `ui-snapshots` (golden-image
tests, implies `screenshot`), `os-notifications` (system notifications), and
`fuzzing` (the internal `teamspace::fuzz` entry points for the `fuzz/` crate).

## Persistence

- Keep a versioned persistence projection next to the feature that owns its
  domain state, use stable domain IDs, and exclude transient UI state from
  snapshots.
- Keep profile-path selection in `src/app/storage.rs`.
- Keep shared authenticated-profile setup, debounced saves, retries and cleanup
  in `src/app/persistence.rs` rather than duplicating that lifecycle in each
  feature.
- Remote projections are session-only replacements, never additions to local
  catalogs or stored snapshots. Feature state may keep an opaque intent ID to
  reconcile a receipt, but may not infer delivery or write a remote record into
  local persistence.
- Two small per-profile preference files exist (private `0600`, atomic):
  - `chat-prefs-v1.json` (schema 2) holds the notification, read-receipt and
    system-notification choices and the per-chat mute list (see
    [Notifications](#notifications)). Per-chat Mute stays in the chat "..." menu,
    which also links to Settings.
  - `app-prefs-v1.json` (schema 1, bounded) holds app-wide choices that are not
    about chats. Today that is only the colour theme (`light`, `dark` or
    `system`; an unknown value reads as `system`). It never holds identity, chat
    or message data. Like the other per-profile stores it is removed on sign-out.

## Theme & UI

- Use the design tokens and type ramp in `src/ui/theme.rs`; do not add raw
  `Color32::from_rgb` or literal font sizes in feature code.
- Every colour token is declared once in the `palette!` table with a light and a
  dark value (a token without both does not compile) and is read through its
  accessor, such as `theme::fg1()`. The active scheme is set once per frame from
  the theme choice, so never cache a colour across frames.
- Use `theme::brand()` only as a fill under `fg_on_brand` text, and
  `theme::brand_fg()` for brand-coloured text, links, outlines, indicators and
  icon tints. Presence colours come from the `PRESENCE_*` tokens.
- A unit test checks WCAG AA contrast of the text tokens on every surface in both
  palettes; add a pair there when you add a token.
- Check a visual change in both schemes and with the overlays open (see
  [Screenshots](#screenshots)).

### Settings

`src/features/settings/` only displays and edits preferences. The shell builds a
`SettingsView` each frame and applies the returned `SettingsAction`s, so the
dialog never touches a store.

### Local time

Microsoft 365 times are shown in the user's local zone, formatted in one place
(`src/shared/local_time.rs`): `Today 17:14`, `Yesterday 09:05`, a weekday name
within the last or next week, otherwise `Oct 12, 14:36` (with the year when it is
not this one). The integration seam stays zone-free: a projection carries the
provider text plus a parsed `RemoteMessageInstant`, and a time that cannot be
placed (an unparsable stamp, or a calendar time in a non-UTC named zone Graph did
not convert) shows the provider text unchanged. Tests see a fixed clock, Friday
2026-11-06 12:00 UTC shown in UTC, through `local_time::clock()`.

## Accessibility

- A clickable region built from a frame or painted rectangle has no accessible
  name by itself. Make it clickable through `ui::a11y::clickable`, which exposes
  its label to assistive technology as a button.
- Message rows register a keyboard focus target while drawn, and
  `ui::message_nav` moves focus between them (Up/Down or J/K, Enter or Space opens
  a row's action toolbar, focus returns to the composer). It acts only while a row
  already holds focus (or on an explicit entry), so typing is never captured.
- Text tokens must pass the contrast test in [Theme & UI](#theme--ui).

## Keyboard shortcuts

- Every app-level shortcut is listed once, in `ui::shortcuts::groups`; the
  shortcuts dialog and Settings both read it, so they cannot drift apart.
- Display text comes from the platform-aware helpers (`primary_chord`,
  `submit_chord`): Cmd on macOS, Ctrl elsewhere. A test rejects hard-coded
  modifier text outside `src/ui/shortcuts.rs`.
- Current chords: Cmd/Ctrl+K command palette, Cmd/Ctrl+, Settings, Cmd/Ctrl+E
  focus search, F5 or Cmd/Ctrl+R refresh Microsoft 365 data, Enter send a chat
  message or activity reply, Cmd/Ctrl+Enter create a chat or post to a channel,
  Esc close the current local dialog or detail view, and Cmd/Ctrl+1 to 5 for
  Activity, Chat, Teams, Calendar and Calls.
- Cmd/Ctrl+K and Cmd/Ctrl+, are consumed before any widget sees them (also over
  a focused text field) and never match with Shift or Alt held. Take Esc through
  `take_escape` so one press is handled by one surface.

## Testing

- **Unit and kittest tests** run with `cargo test --locked --no-fail-fast`;
  `egui_kittest` drives the real UI in-process. Tests never touch a real user
  profile directory (storage is redirected to a temp directory under test).
- **Snapshots.** UI snapshot tests compare the real UI against golden images in
  `tests/snapshots/sections/`: the six sections (activity, chat, teams, calendar,
  calls, files) in light and dark, plus the settings, palette and search overlays
  in light:

  ```sh
  cargo test --features ui-snapshots snapshot_tests
  ```

  The goldens are GPU renders and are compared on macOS only. After an intended visual change, update them on macOS with
  `UPDATE_SNAPSHOTS=1` (rewrites only the failing goldens) or
  `UPDATE_SNAPSHOTS=force` (rewrites all), then shrink them with
  `oxipng -o4 --strip safe tests/snapshots/sections/*.png`. Each golden must stay
  under 400 KB; a test enforces it. The `.new`,
  `.diff` and `.old` PNGs written next to the goldens are git-ignored scratch.
- **Screenshots** (<a id="screenshots"></a>). `cargo run --features screenshot -- --screenshot [activity|chat|teams|calendar|calls|files]`
  renders the real app off-screen at 1440x900 from isolated local-preview data
  and replaces `artifacts/latest.png`. Retain only that latest visual-check image
  and do not add visual-history artifacts. Only the screenshot renderer reads
  `TEAMSPACE_THEME=light|dark|system` (default `light`) and
  `TEAMSPACE_OVERLAY=settings[:section]|palette|search`.
- **Fuzzing.** The `fuzz/` crate (cargo-fuzz, nightly) targets `graph_page`,
  `loopback_callback`, `message_body`, `next_link` and `stored_snapshot`, each
  with a dictionary in `fuzz/dicts/` and seeds in `fuzz/seeds/`. The targets call
  `teamspace::fuzz` (feature `fuzzing`): every entry point takes arbitrary bytes,
  must never panic, and asserts the invariant its production counterpart
  promises. Run one with `cd fuzz && cargo +nightly fuzz run <target>`; run every
  target after touching `fuzz/`, `src/fuzz.rs`, `src/integrations/` or a
  feature's `persistence.rs`.
- **Coverage.** Measure with `cargo llvm-cov`. The target floors are 88% line
  coverage for `src/integrations/` and 85% for `src/app/workspace_transport/`;
  raise them as coverage grows.

## Security & tokens

- Never persist, log or render Microsoft Graph access tokens, authorization
  codes, refresh tokens, PKCE verifiers, state or nonce values.
- Keep browser authorization and raw-token movement inside
  `src/integrations/entra_auth/` and `src/integrations/`. App and feature code
  may only move the opaque `DelegatedTokens` handoff into the transport
  controller.
- Keep remote network work off the egui update thread.
- Do not identify this independent app as Microsoft Teams, reuse a first-party
  client ID, or attempt to bypass Entra application registration or delegated
  consent. Keep the public-client configuration, PKCE flow and user-visible
  identity truthful.
- A secret URL (a preauthenticated download or upload-session URL) is never
  rendered, logged, persisted, copied into feature state or `Debug` output.
- See [SECURITY.md](SECURITY.md) for the reporting process and the properties
  users can rely on.

## Microsoft 365 integration

Route Microsoft 365 targets only through the background transport worker; never
downgrade them into a local-preview success. At the integration boundary, mark
fixture and local keys with `ProviderResourceRef::local_preview`; use a
Microsoft 365 resource reference only after a remote projection has supplied a
provider-issued ID.

### Clouds

- Never hard-code a Graph, Entra login or download host outside
  `src/integrations/cloud.rs`. Every host comes from `GraphCloud`
  (`TEAMSPACE_CLOUD`: `global`, `usgov`, `usgovdod`, `china`); build routes with
  `GraphCloud::graph_route` and accept continuations only through
  `GraphCloud::validated_next_link`. A unit test fails if a stray cloud host
  appears in `src/integrations`.
- The cloud is fixed for a session and travels with its `DelegatedTokens` into the
  integration-private `GraphSession`, so a token is never presented to another
  cloud.
- Preauthorized download hosts are a per-cloud allowlist
  (`content_host_suffixes`); do not add commercial-only suffixes such as `svc.ms`
  to a national cloud.
- Non-global clouds use resource-qualified scopes (`GraphCloud::qualify_scope`),
  while required-scope checks stay on bare names (`REQUIRED_SCOPES` is
  `offline_access` and `User.Read`).

### Auth & token refresh

- The refresh token lives only in an in-memory `TokenRefresher` inside
  `GraphSession`, used only by the serial worker that owns the adapter, and is
  zeroized when the session drops or Entra rejects it (after which the token
  endpoint is never called again).
- `GraphSession` renews ahead of expiry, and a read that gets `401` renews and
  retries exactly once. A mutation is never replayed: its `401` renews the session
  and returns the known rejection "renewed the session before delivery; send
  again", so the draft is restored. Never add a retry around `post_json_status` or
  any other write.
- A read-class `POST` (`GraphSession::post_json_query`, used by presence and
  message search) gets one retry after a `401` renewal, like a read.
- Optional scopes are never added to `REQUIRED_SCOPES`; gate each affordance on
  `RemoteCapabilities` (see the feature subsections below).

### Transport & polling

Never poll Microsoft 365 on a timer: Microsoft's
[Teams API polling requirements](https://learn.microsoft.com/en-us/graph/api/resources/teams-api-overview?view=graph-rest-1.0#polling-requirements)
forbid loop-polling Teams resources, and a 30 s loop violates the Terms of Use.
Do not add timers or background polling.

Refresh is event-driven only (`src/app/remote_refresh.rs`): window focus regained,
a remote selection opened, or an explicit F5 / Ctrl+R / header Refresh (an explicit
trigger outranks the others in the same frame).

- It reads nothing while the window is unfocused.
- It keeps a 60 s minimum between directory refreshes and 30 s between refreshes
  of one selected conversation. An explicit request bypasses the minimum, never
  the `Retry-After` cooldown.
- It is a no-op while that kind of read is active, queued or cooled down.
- It uses the ordinary bounded reads, never cancels or completes a delivery or
  reconciliation read, and never replays a write.
- Loaded chat history stays visible while it refreshes, but only an exact `Ready`
  read unlocks a reaction, quote or edit; a failed refresh keeps the earlier rows,
  marked stale.
- Window focus has one definition (`window_focused`); do not read the viewport
  elsewhere.
- The People presence poll (`SyncInterest::ChatMemberPresence`) and Microsoft 365
  message search (`SyncInterest::MessageSearch`) are the only other reads, each
  with its own cooldown kind (see [Chats](#chats)).

### Writes & reconciliation

- Remote `POST` operations are non-idempotent: retry only a provider-confirmed
  throttle, and treat a network or server ambiguity as a delivery-uncertain
  result followed by a provider refresh.
- A write adds no optimistic local row or overlay, accepts only its documented
  success status, and reconciles only against the exact opaque target it wrote.
  An ambiguous result is never automatically replayed.
- An ambiguous chat send or quote reply reconciles only against a fresh
  `ChatMessages` read of that exact chat, never the Chats directory preview (it
  carries no priority or quote reference); the read may confirm the draft even if
  that chat is no longer selected.
- A bounded channel root-post or reply feed is never proof that a mutation was
  absent: do not automatically replay an ambiguous channel post or reply. Preserve
  that text for explicit user review while the same remote session remains
  available.
- If the adapter is lost, no refresh is possible: preserve only enough state to
  map the terminal outcome, then require reconnect and channel or reply-thread
  inspection before another send.
- Add remote mutations only with provider-issued targets, least-privilege consent
  and an explicit reconciliation contract.

### Chats

A selected Microsoft 365 chat history is a separate, target-bound `ChatMessages`
read under `Chat.Read`, not an expansion of the global chat directory or an
implicit read receipt. Keep its Loading/AccessDenied/Ready state session-only,
cancel only an obsolete selected-chat history poll, reject a result whose opaque
chat target is no longer selected, and never persist the returned message rows.
Its Graph continuation must remain HTTPS on the configured cloud's Graph host and
on the exact selected chat's `/v1.0/chats/{id}/messages` path; do not accept a
cursor that can steer the bearer token to another chat or resource.

#### Reactions

- The remote chat-message reaction palette is limited to the app's six typed
  standard kinds on a visible selected-history message, each through
  `WorkspaceCommand::SetChatMessageReaction`.
- It uses the existing delegated `ChatMessage.Send` scope, adds no optimistic
  local reaction, and accepts only `204 No Content` as delivery confirmation.
- Bind each gate to opaque chat/message targets, its reaction kind and its
  controller intent, and retain it until the exact selected `ChatMessages`
  projection reports the requested selected state for that kind. A known
  rejection may unlock only that exact gate; a confirmed or ambiguous result
  refreshes only its original `ChatMessages` target and never replays the write.
- A missing or malformed provider message ID is display-only: never infer a route
  from its row index.
- Route only the exact canonical Unicode values this app writes. Legacy names and
  variation-selector forms are read-side display aliases of the same kind and are
  never written; other Unicode, custom and tenant-configured reactions stay
  provider-private and are never sent through the local lifecycle.

#### Pinned messages

Selected-chat Pinned messages are a separate, target-bound `ChatPinnedMessages`
read under the same delegated `Chat.Read` grant.

- Expose it only through the current remote chat's read-only contextual panel; it
  must not create a pin/unpin route, message action, local cache, read receipt or
  new Entra consent requirement.
- The adapter may make one bounded `pinnedMessages?$expand=message` request and
  retain at most 50 normalized rows.
- Require each expanded row's opaque message ID to literally equal its outer pin
  ID, discard malformed or duplicate rows, reject stale chat targets, and never
  surface pin IDs, message targets, HTML, attachments, reactions or raw Graph
  objects outside the integration seam.
- Closing or replacing the panel cancels only its selected-pin poll; a
  cancellation-pending same-target read must be replaceable so an immediate reopen
  cannot stay Loading.

See Graph's [list pinned messages API](https://learn.microsoft.com/en-us/graph/api/chat-list-pinnedmessages?view=graph-rest-1.0).

#### People and presence

Selected-chat People is a separate, target-bound `ChatMembers` read under the same
delegated `Chat.Read` grant (Graph lists it among the permitted permissions for
chat members, so do not add `ChatMember.Read` or a new consent requirement).

- Expose it only through the current remote chat's read-only contextual panel; it
  must not add or remove members, change roles or cache a roster.
- The adapter makes one bounded `members` request without `$top`, never follows
  `@odata.nextLink`, and retains at most 100 rows.
- Only a sanitized display name, an Owner/Guest label and an opaque
  `MentionHandle` (see [Mentions](#mentions)) may leave the adapter; discard
  member IDs, emails, tenant data and unknown roles.
- Closing or replacing the panel cancels only its members poll.

Presence is optional (`Presence.Read.All`, never in China; see
`RemoteCapabilities::member_presence`).

- Member user IDs are collected only from `aadUserConversationMember` rows during
  the explicit People open and kept solely in the adapter (a per-transport map of
  at most 100 IDs for each of 16 chats, cleared on reauthentication and never
  logged or sent across the seam).
- The roster read fills that one map whether or not presence is granted, and a
  presence `403` only stops presence queries: it does not clear the map, because
  the same map is the mention directory.
- That open makes one bounded `getPresencesByUserId` request. While the panel is
  open and the window is focused, the shell refreshes at most every 60 s with a
  presence-only poll (`SyncInterest::ChatMemberPresence`, its own cooldown kind)
  that sends only that query: it never re-reads the Teams chat roster, and with no
  remembered IDs it does nothing until People is reopened. The query is a
  read-class `POST` (`GraphSession::post_json_query`).
- Presence is never persisted, logged or treated as a read receipt.

See Graph's [list chat members API](https://learn.microsoft.com/en-us/graph/api/chat-list-members?view=graph-rest-1.0)
and [get presences by user ID](https://learn.microsoft.com/en-us/graph/api/cloudcommunications-getpresencesbyuserid?view=graph-rest-1.0).

#### Mentions

The remote Chat Mentions quick view is read-only presentation derived only from a
`chatMessage`'s structured mentioned-user ID exactly matching the verified `/me`
account ID.

- Never infer it from rendered `@name` text or a display name, or retain a
  provider recipient ID in feature state.
- Do not add a polling request or turn viewing it into a read receipt.
- Missing, null or malformed mention metadata must fail closed while preserving
  otherwise valid message history.

Mentions of other people use opaque, session-only `MentionHandle`s, never user IDs.

- The adapter issues one random handle per mentionable member (an Entra user with
  an ID and a display name) when it reads that chat's People roster, in the same
  per-chat map as the presence IDs, so there is no second ID store. A handle is
  stable across re-reads of the roster, resolves only within the chat it was
  issued for, and `Debug` reveals nothing. Handles are cleared with the map on
  reauthentication, together with the history cache.
- A message read from the selected chat carries `mentioned_handles`: its mentions
  in order (a repeat repeats), at most 20, mapped from the structured mention's
  user ID through that chat's remembered roster. A mention of anyone not in that
  roster, or in a chat whose roster has not been read, is dropped.
- Sending a mention is a chat send whose `ComposedBody` holds `Mention` inlines.
  The composer's `@` picker offers only members of the People roster the feature
  already holds (it never reads one itself, because that read also asks for
  presence, which belongs to an explicit People open). At send each intact `@Name`
  of a picked member becomes one mention, at most 20; a quote reply or an edit
  carries none.
- The adapter refuses before any request a handle it did not issue for that chat,
  or one whose remembered name is not the inserted one, and otherwise sends
  `<at id="n">Name</at>` with a `mentions` entry per element (`aadUser`, from the
  remembered user ID), under the existing `ChatMessage.Send` scope.
- An ambiguous mention send is confirmed only by an own row with the same visible
  words and the same multiset of `mentioned_handles` (a plain send never matches a
  row that mentions anyone), and an unconfirmed one restores the draft and its
  picks.

#### Sending, priority and rich text

- Remote chat sends carry a typed Normal, High or Urgent priority through the
  existing `WorkspaceCommand::SendChatMessage` and delegated `ChatMessage.Send`
  scope; do not request new consent for it.
- Preserve the selected priority with the protected remote draft while a send is
  pending, rejected, cancelled or awaiting reconciliation, and confirm an
  ambiguous write only against an own message in that chat's selected
  `ChatMessages` history with both the same body and priority.
- Render only exact `normal`, `high` and `urgent` provider values; absent,
  malformed, unknown or case-variant values are Normal.
- A chat send or edit body travels as a structured `ComposedBody`, not a string.
  An unformatted body keeps the exact `text` wire form; a body with bold, italic,
  code or links is rendered only by the adapter (`rich_html.rs`) into the escaped
  HTML subset `p`, `br`, `strong`, `em`, `code`, `a` and `at` (a mention), where a
  link needs an `https` URL that is revalidated there. Quote replies stay plain
  text.
- Received HTML is converted once, inside the adapter, into at most 256 formatted
  runs (`b`/`strong`, `i`/`em`, `code`, `pre`, `https` links and paragraph
  breaks); everything else becomes its text, malformed HTML yields no runs, and
  `plain_text` stays authoritative.
- A link click is only a request: the feature queues the destination and the app
  revalidates it at `src/app/browser.rs`.
- An ambiguous formatted send is confirmed on its plain projection (visible
  words), an unformatted one on its exact text.

#### Quote replies

Remote quote replies are a separate, deliberately narrower
`WorkspaceCommand::ReplyToChatMessageWithQuote` path.

- Expose them only for a non-deleted, opaque-target row in the current Ready
  selected-chat history; never add a generic local reply action or derive a
  message route from rendered text or an index.
- Send one to ten distinct opaque source message IDs through Graph's
  `replyWithQuote` endpoint with a plaintext `replyMessage` body; reject an empty,
  duplicate or eleven-plus set before any network call.
- Quote replies are always Normal priority: do not add the generic send
  `importance` field to that payload or request new consent beyond delegated
  `ChatMessage.Send`. Quote mode and edit mode refuse each other with a notice.
- Accept only `201 Created`, add no optimistic row, and preserve the quote, body
  and prior composer priority across a known rejection, cancellation or uncertain
  outcome when every source remains valid. An unsent selection drops a vanished
  source with a notice; a staged or ambiguous set is never shrunk, so if any
  source disappears restore an ordinary editable draft instead of retaining an
  unstageable quote.
- An uncertain quote reply refreshes only its exact `ChatMessages` target and is
  confirmed only when an own message matches the body, Normal priority and the
  exact set (order ignored) of expected opaque source targets parsed from its
  bounded `messageReference` attachments. A duplicate, malformed or more than ten
  references fail closed, and a plain message and a quote reply can never confirm
  each other.
- Normalize the reference sender/preview into bounded inert text, use generic
  display fallbacks when optional safe context is absent, and never expose the raw
  attachment JSON or provider IDs to UI, persistence or logging.

See Graph's [reply with quote API](https://learn.microsoft.com/en-us/graph/api/chatmessage-replywithquote?view=graph-rest-1.0).

#### Editing and deleting

Editing or deleting the signed-in user's own remote chat message is a separate,
deliberately narrow path (`WorkspaceCommand::EditChatMessage` and
`DeleteChatMessage`), distinct from sends and reactions.

- It needs the OPTIONAL delegated `Chat.ReadWrite` scope, which is broader than
  `Chat.Read`: never add it to `REQUIRED_SCOPES`, and gate the affordances on
  `RemoteCapabilities` (computed in the adapter from the granted scope the token
  response reported and `GraphCloud::supports_chat_message_update`). A development
  token is `Unknown` and degrades on a `403`, which marks only that action
  `NotGranted` for the session.
- Edit is a `PATCH` to `chats/{id}/messages/{id}` and exists in the global cloud
  only; it is refused before any request elsewhere. Delete is a body-less `POST` to
  `users/{account}/chats/{chat}/messages/{message}/softDelete` and needs the
  verified account ID.
- Both accept only `204 No Content`, add no optimistic row, and are never
  replayed: a `401` renews the session and returns the known rejection, so the
  user's text comes back.
- Edit is offered only for a row the adapter proved editable (own, not deleted, an
  ordinary message, no attachments, mentions or policy tags, body text or HTML
  limited to paragraphs, bold, italic, code and `https` links), because it rewrites
  the whole body. The composer edits the row as markdown-lite, and an edit is
  withheld when the row's formatting would not read back identically.
- A `204` keeps the gate until the next exact `ChatMessages` read. An ambiguous
  result reconciles only against that exact chat's fresh history (an edit is
  confirmed by an own, live row with the requested text, a deletion by that row
  being marked deleted); a missing row is never proof.
- Channel posts and replies have no edit or delete.

#### Read receipts

The only read receipt is the opt-in `WorkspaceCommand::MarkChatRead`, off by
default (the "Send read receipts" preference in Settings > Privacy, kept in the
per-profile `chat-prefs-v1.json`).

- It posts `/chats/{chat}/markChatReadForUser` with `{"user":{"id","tenantId"}}`
  under the OPTIONAL `Chat.ReadWrite` scope (never in `REQUIRED_SCOPES`; gated on
  `RemoteCapabilities::chat_mark_read`, available in every cloud).
- It is sent at most once per selection view, and only when the preference is on,
  the window is focused (the one `window_focused` signal), the provider still
  reports the chat unread, and its history is `Ready` from an exact read.
- It accepts only `204 No Content`, adds no optimistic state, and is never retried
  or reconciled: a throttle or ambiguity is dropped silently and a `403` marks it
  `NotGranted` for the session.
- The tenant ID is read lazily once per adapter from `/organization?$select=id`
  (falling back to the user's own membership of that chat) and lives only in the
  adapter; with no verified identity or tenant, no request is made.

#### Notifications

New-message notifications are in-app only by default and session-derived.

- A new arrival is detected from the directory preview
  (`features/chat/remote/arrivals.rs`) and never persisted. The first projection
  after a connect seeds silently; later a chat counts only when its newest preview
  is a different message strictly later than the one already seen (compared
  through the integration's opaque `RemoteMessageInstant`, parsed once at the
  seam; an unparsable time proves nothing), is not the user's own or deleted, and
  is not the open chat while the window is focused.
- A mention is labelled only from the structured mention of the newest row of an
  exact history read, never from preview text.
- A notification never contains a message body, and by default not even the chat
  name: a count.
- Choices (notifications on, show chat names, per-chat mute) live in
  `chat-prefs-v1.json` (see [Persistence](#persistence)). A muted chat is stored
  only as a 16-byte digest, `SHA-256("teamspace-mute-v1" || secret || account ||
  0 || chat id)`, under a random per-profile secret in that same file, bounded to
  500 (oldest dropped); the file never contains a chat ID, and the secret and
  digests are never logged.

System notifications exist only behind the `os-notifications` cargo feature
(`src/app/os_notify.rs`, `notify-rust` with its pure-Rust `z` backend) and the
separate per-profile `os_notifications_enabled` choice, which is off by default
and offered only when a notifier could run.

- An OS notification names the app that raised it, so it is raised only under a
  truthful identity: Linux with the application name `Teamspace`; macOS only when
  the process runs from a bundle whose `CFBundleIdentifier` is
  `io.github.vakesz.teamspace` (a bare `cargo run` binary is refused, because it
  would be attributed to another app); Windows only when the AppUserModelID
  `io.github.vakesz.teamspace` is registered under
  `HKCU\Software\Classes\AppUserModelId`. Anywhere else, and in a build without
  the feature, there is no notifier and the in-app toast is all the user gets.
- It carries the same count-only text as the in-app toast (never a message body),
  is sent only while the window is unfocused, and is delivered on its own thread
  behind a bounded queue that drops overflow, so the UI thread never waits on the
  OS.
- Do not widen the gate or pass message text to it.

#### Attachments & sharing

Chat attachments are one file of at most 100 MiB per message, in Microsoft 365
chats only, behind `RemoteCapabilities::file_attach`: the OPTIONAL delegated
`Files.ReadWrite` scope (the documented higher `Files.ReadWrite.All` is accepted
too), never in `REQUIRED_SCOPES`, available in every cloud, matched as a bare name
or qualified only with exactly this cloud's own `https://{graph host}/` (any other
resource does not grant). A `403` turns it off for the session.

*The picked file*

- It is a `LocalUploadSource` from the system file dialog only: its path lives for
  one upload job, is never persisted, logged, shown or put in a `Debug` string, and
  the file is refused if its size or modified time changed since it was picked.
- The path is resolved once at the pick, then opened with
  `O_NONBLOCK | O_NOFOLLOW` (unix) and checked on the opened handle (a regular
  file, the picked size, modified time, and device and inode), so a link, a FIFO or
  a swapped file is refused without blocking.
- `WorkspaceCommand::UploadChatFile` validates the name (no `/ \ : * ? " < > | # %`,
  controls, Unicode format characters, variation selectors, tag characters, U+180E
  or U+2028/2029, leading or trailing dot or space, dot segments,
  `CON`/`PRN`/`AUX`/`NUL`/`COM0-9`/`LPT0-9` stems even with trailing spaces, more
  than 255 UTF-16 units).

*Small upload (at most 4 MiB)*

- The upload goes to `me/drive/root:/Microsoft Teams Chat Files/{name}:/content`
  without ever replacing a file: one `GET` of the exact path must answer `404` (a
  `200` is a name conflict), then one `PUT` with
  `?@microsoft.graph.conflictBehavior=fail` (a `409` is a name conflict), then one
  `GET` of the exact path for the `eTag` and `webDavUrl`, because a `PUT` answer
  lacks the latter.
- The `PUT` is a write: it is never replayed or retried, not even after a `429`,
  which becomes a terminal rejection (the command queue never schedules an upload
  command, a session creation, a fragment or a cancellation for retry) with the
  draft and the file kept.

*Large upload (over 4 MiB, at most 100 MiB)*

- It goes by upload session instead of one request, as separate worker jobs.
  `UploadChatFile` then only opens the session (the same name check, then
  `createUploadSession` with `{"item": {"@microsoft.graph.conflictBehavior": "fail"}}`)
  and reports progress; each 3.75 MiB fragment (a multiple of 320 KiB) is its own
  `ContinueChatUpload` command, so a cancellation, another command or a refresh can
  run between them, and `CancelChatUpload` sends `DELETE` to the session.
- The session's `uploadUrl` is a secret: it must be HTTPS on a subdomain of the
  configured cloud's content hosts (the same check as the preview hop) with no
  credentials, port or fragment; it lives only in the adapter's bounded sessions
  (two open, zeroized, never logged, absent from every `Debug`), and the fragment,
  status and cancel calls are sent WITHOUT the Graph bearer token.
- A fragment `PUT` to a session is idempotent by protocol, so after a network
  error, `5xx`, `429` or `416` the adapter asks the session for its
  `nextExpectedRanges` once and resumes from there, and abandons it after three
  fragments in a row without progress; if the session cannot be asked, the upload
  is delivery-uncertain and the feature cancels the session.
- The picked file must still have its size and modified time before every fragment
  or the session is cancelled. A `409` on the last fragment is a name conflict. A
  fragment that reports progress for a composer that has gone cancels its session.
- Losing the worker mid-upload ends the command in flight as delivery-uncertain
  (no reconciliation read, no replay), rejects what was queued behind it, and
  leaves a failed composer, never a stuck one.

*Read-back and the receipt*

- The finished file is read back by its exact path. The read-back must be proof
  that the file is this upload: its size always equals the picked file's and a
  reported hash never differs from the picked bytes' (QuickXorHash, the one hash
  Graph guarantees for work or school, and SHA-256 when reported; `sha1Hash` and
  `crc32Hash` never vouch for a file).
- With the item ID the `PUT` (or the last fragment) answered with, the read-back
  must carry that same ID. Without it, or after a network error or `5xx` (exactly
  one `GET` of the same path looks for the file), the file is adopted only on an
  exact hash match and comes back marked recovered, so the user must choose Attach;
  a same-size file that reports no comparable hash, or a different one, is never
  adopted and the upload is delivery-uncertain. Nothing is ever re-uploaded.
- The `webDavUrl` must be HTTPS on a subdomain of the configured cloud's content
  hosts (the preview hop's label-boundary check) with no credentials, port or
  fragment.
- The receipt carries only an opaque `UploadedFileHandle`; the drive item, its URLs
  and ETag stay in the adapter's bounded registry, which is cleared on
  reauthentication.

*Sending the message*

- `SendChatMessage.attachment` accepts only a handle the adapter's own session
  issued and then sends HTML ending in `<attachment id="{GUID}"></attachment>` plus
  one `reference` attachment whose `id` is the GUID of the file's ETag (lowercase,
  hyphenated, validated before it can enter markup), `contentUrl` its HTTPS
  `webDavUrl`, and `name`. With a file the text is optional. A quote reply or an
  edit carries no file.
- An ambiguous file send reconciles only against a fresh `ChatMessages` read of
  that exact chat and is confirmed by an own message with the same visible text (or
  none), the same priority, and exactly one reference attachment whose id equals
  that GUID; a plain message and a file message can never confirm each other. A
  rejected, cancelled or uncertain send restores the draft with the same handle.

*Sharing*

- Sending a message with a file first shares the file with the chat's other
  members, as `ShareUploadedFileWithChat` queued ahead of the send, so the two run
  in order. One job reads the chat's members once (the bounded `members` read, at
  most 100) and, for each batch of 20, sends one `POST me/drive/items/{item}/invite`
  with `roles: ["read"]`, `requireSignIn: true`, `sendInvitation: false` (a direct
  grant, no email) and each member's directory `objectId` as the one field of its
  `driveRecipient`; `200` grants everyone and `207` marks each recipient that
  failed.
- The signed-in user and any member without a verified Entra user ID are left out.
  The IDs stay inside that job: they never cross the seam or reach a log or
  `Debug`, and the receipt carries only a granted and a failed count. Only a file
  this session uploaded can be shared. It needs no scope beyond `Files.ReadWrite`.
- No request is replayed, a throttle stops the remaining batches, and the share is
  never queued for retry. A shortfall is a warning only ("may not be able to open
  it", with counts and no names); the message is sent regardless, because the file
  is the user's to send and the warning says what to expect. Sending again after a
  rejected send shares again, which Graph accepts as a repeat grant.

*Received attachments*

A received `reference` attachment is normalized to a bounded display name and its
GUID (a malformed id or nameless reference is skipped, at most ten) and shown as a
read-only chip: no URL leaves the adapter, nothing is downloaded or opened, and a
message that carries one is not editable.

#### Message search

Microsoft 365 message search lives in the global search overlay's trailing
"Microsoft 365" group and runs only when the user presses Enter on its "Search
Microsoft 365 messages for ..." row (or "Show more results"), never per keystroke,
on a timer, or from a refresh.

- It is `SyncInterest::MessageSearch`: one bounded `POST /search/query`
  (`entityTypes: ["chatMessage"]`, 25 hits per page, at most four pages, a 256 KiB
  response cap) under the delegated `Chat.Read` grant, so it needs no new consent,
  in every cloud.
- It is a read-class `POST` (`GraphSession::post_json_query`), with its own
  cooldown kind and cancellation, replaced by a newer query and never sharing a
  lifecycle with chat history.
- The query is a `RemoteSearchQuery` (1 to 256 characters, no control characters)
  whose `Debug` prints only its length; it is never persisted or logged.
- A hit is parsed leniently, accepts only the exact `microsoft.graph.chatMessage`
  type, and crosses the seam as opaque chat/message (or team/channel) targets, the
  sender's display name, a display time, and a snippet reduced to inert text
  (highlight markup stripped, 512 bytes); the sender's address, the web link and
  the etag are dropped in the adapter.
- Results are bound to the exact query and page that produced them, ignored if
  stale, and cleared when the overlay closes, the typed query changes, the session
  disconnects, or the profile changes.
- Graph sorts by date only, reports `total` for the page alone, and refuses guest
  accounts; a `403` becomes a quiet "not available" row.
- Opening a hit selects the chat (or channel) by its opaque target in the current
  directory, scrolls to and focuses the message once the chat's history is loaded,
  and otherwise says so in one notice. It never invents a local row or read
  receipt.

See Graph's [search API](https://learn.microsoft.com/en-us/graph/api/search-query?view=graph-rest-1.0)
and [Teams message search](https://learn.microsoft.com/en-us/graph/search-concept-chat-messages).

### Teams channels

Keep a Microsoft 365 Teams/channel selection distinct from local catalog keys, and
render unsupported remote surfaces as read-only rather than routing them through
local post, thread, file, note or meeting actions.

- The remote Teams mutations currently allowed are a selected channel's text-only
  root post through `WorkspaceCommand::SendChannelPost`, a selected root post's
  text-only reply through `WorkspaceCommand::ReplyToChannelPost`, and a visible
  selected-channel root post or selected-root reply's bounded typed reaction
  palette through the route-complete `WorkspaceCommand::SetChannelMessageReaction`
  or `WorkspaceCommand::SetChannelMessageReplyReaction`. All use delegated
  work-or-school `ChannelMessage.Send`.
- Root posts and replies have no optimistic local rows; only `201 Created`
  confirms their delivery. A reaction has no optimistic local overlay either; only
  `204 No Content` confirms it.
- Success or ambiguity must refresh only the exact opaque `ChannelMessages` or
  `ChannelReplies` target while the adapter remains available. An unexpected worker
  loss cannot schedule that refresh, so do not claim an absent row or prompt an
  automatic retry.
- Listing either bounded feed, including its best-effort ambiguity refresh,
  requires delegated `ChannelMessage.Read.All` with tenant admin consent.
- Bind every root-post feed to opaque team/channel targets, and every reply feed to
  opaque team/channel/post targets. Bind a root-post reaction gate to all three
  route targets, its reaction kind and its controller intent, and a reply-reaction
  gate to all four route targets, its reaction kind and its controller intent. A
  matching route alone is not reconciliation: retain each gate until the
  provider-owned selected state for that same kind matches its requested value, so
  a stale or cross-kind read cannot unlock it.
- Reaction values follow the same canonical-only rule as chats (see
  [Reactions](#reactions)).
- Do not route reply-to-reply mutations, post edits or deletions, attachments,
  mention authoring or other unsupported post mutations through the local
  lifecycle.

### Calendar

The Microsoft 365 default-calendar agenda uses delegated `Calendars.ReadBasic`
only.

- Keep its events session-only and read-only even if provider metadata says the
  calendar is editable.
- An existing integration-normalized HTTPS join URL may cross only an explicit
  user-click path to the app shell, which must revalidate it immediately before
  opening the system browser; never route it through local event, local-call,
  persistence or workspace-transport state.
- Do not route remote calendar create, edit, delete or attendee actions through the
  local event lifecycle.

### Files

**OneDrive root.** The Microsoft 365 Files projection is a bounded, session-only
metadata view of the signed-in user's OneDrive root using delegated `Files.Read`.
Keep it separate from local paths and uploads; do not use the deprecated Graph
`recent` endpoint or present root metadata as a Teams channel-file view.

**Preview.** The one narrow preview exception is an explicit click on an exact
current root item normalized as a `File`: it may make one target-bound content read
for allow-listed UTF-8 text (at most 256 KiB) or image bytes (at most 12 MiB and 16
million decoded pixels).

- Never prefetch this content from listing or search rendering.
- Retain it only in memory, clear it on close, replacement, disconnect or session
  change, and reject late, missing, folder, unknown, oversized or mismatched
  results.
- It must never acquire a local path, disk write/download, file cache, log value,
  persistence lifecycle, upload identity or mutation capability.
- The selected root preview may follow exactly one integration-validated Graph
  content redirect internally: send the bearer token only to Graph, and never
  render, log, persist, copy into feature state or otherwise expose the redirected
  preauthenticated URL. Do not use `@microsoft.graph.downloadUrl`.

**Open in browser.** The root adapter may retain an optional bounded, explicit-HTTPS
`webUrl` only for a direct **Open in browser** click. Keep that URL private to the
session, bind the click to the current opaque item target, clear a queued handoff
when the projection changes, and revalidate HTTPS immediately before launching the
system browser. Browser `webUrl` values must never become preview or download
targets.

**Channel files.** The selected Microsoft 365 channel Files surface is a separate,
session-only metadata projection.

- Bind it to its opaque team/channel selection, reject stale responses, and read no
  more than one bounded direct-child page of that channel's backing folder.
- Its documented least-privilege delegated work-or-school permission is
  `Files.Read.All`; retain `Files.Read` for the OneDrive-root flow rather than
  conflating the two.
- An optional typed `webUrl` may cross that projection only for a direct **Open in
  browser** action: bind it to the current opaque item target, discard it when a
  folder projection or selection changes, and revalidate it at the final app
  browser boundary. Do not render, log, persist or use it for content transfer.
- Do not add channel-file preview, download, upload, rename, move, share, delete or
  any other channel-file mutation without an explicit provider target, permission
  and delivery contract.

See [docs/research/teams-channel-files.md](docs/research/teams-channel-files.md).

## Release & packaging

- Versions come from `Cargo.toml`; a release tag must equal
  `v<Cargo.toml version>`. Releases are built by hand from a clean checkout
  (`cargo build --locked --release`), never from a reused build cache.
- macOS: both architectures are built, merged with `lipo` into a universal binary
  and packaged by `scripts/bundle-macos.sh <binary> <version> <output-dir>`
  (`Teamspace.app` with `Info.plist` from `packaging/macos/`, an icon rendered from
  `assets/app-icon/teamspace-1024.png`, and `licenses/`). `CODESIGN_IDENTITY`
  selects the signing identity; the default `-` is ad-hoc, and a real identity also
  enables the hardened runtime and a secure timestamp for notarization. The bundle
  is also what gives the app a truthful identity for system notifications.
- Linux and Windows: `cargo build --locked --release` archives (`.tar.gz` and
  `.zip`) that ship `LICENSE` and `licenses/` (Inter OFL, Fluent UI System Icons).
  Windows Authenticode signing is still a placeholder.
- Publish a `SHA256SUMS` file alongside the archives in the GitHub release.
- Third-party licence files (Inter OFL, Fluent UI System Icons) ship in every
  archive; see `assets/icons/README.md` for the vendored icons.
