# Teamspace

Teamspace is a native Rust desktop collaboration workspace inspired by Microsoft
Teams, built with [egui](https://github.com/emilk/egui). It runs on macOS, Linux
and Windows. The interface is English only.

It is an independent app, not Microsoft Teams. By default it makes no Microsoft
365 requests and keeps local-preview data on the device. Connecting a Microsoft
365 account is opt-in.

## Features

- **Activity, Chat, Teams, Calendar, Calls and Files** sections, a global search,
  a command palette, threads, replies and reactions.
- **Local preview** (the default): every section works on fictional seed data, and
  your changes are saved per profile on this device. Nothing leaves the device.
- **Microsoft 365** (opt-in): the same sections backed by Microsoft Graph, shown
  for the current session only. Remote data is never written to disk.
- **Light, dark or system theme.**
- **Keyboard-first**: command palette, Settings, refresh and app switching
  shortcuts (see [Keyboard shortcuts](#keyboard-shortcuts)).

## Run

Install the Rust toolchain (the version is pinned in `rust-toolchain.toml`), then
run from the repository root:

```sh
cargo run
```

### Platforms and data locations

Teamspace builds on macOS, Linux and Windows. Local-preview
state for each profile is stored under `teamspace/profiles/<profile>/` in the
state directory:

| OS      | State directory (unless `XDG_STATE_HOME` is set to an absolute path) |
| ------- | -------------------------------------------------------------------- |
| macOS   | `~/Library/Application Support`                                      |
| Linux   | `~/.local/state`                                                     |
| Windows | `%LOCALAPPDATA%`                                                     |

The session file lives in `XDG_CONFIG_HOME` when it is set to an absolute path,
otherwise `%LOCALAPPDATA%` on Windows and `~/.config` on macOS and Linux. Files
are private to your user. Microsoft 365 messages, files, events and tokens are
never stored there.

## Settings

Open Settings from the account menu, the command palette ("Open settings") or
Cmd/Ctrl+, . It holds the theme (Light, Dark or System), the notification and
read-receipt choices, the Microsoft 365 connection and the permissions it was
granted, the keyboard shortcut list, and the version and licenses.

## Keyboard shortcuts

Cmd on macOS, Ctrl elsewhere.

| Keys                     | Action                                       |
| ------------------------ | -------------------------------------------- |
| Cmd/Ctrl+K               | Command palette                              |
| Cmd/Ctrl+,               | Settings                                     |
| Cmd/Ctrl+E               | Focus workspace search                       |
| Cmd/Ctrl+1 to 5          | Activity, Chat, Teams, Calendar, Calls       |
| F5 or Cmd/Ctrl+R         | Refresh Microsoft 365 data                   |
| Enter                    | Send a chat message or activity reply        |
| Cmd/Ctrl+Enter           | Create a chat or post to a channel           |
| Esc                      | Close the current local dialog or detail view |

The full list is always available in Settings > Keyboard shortcuts.

## Microsoft 365 (optional)

Teamspace uses a tenant-authorized Entra public-client registration with
authorization code + PKCE and a loopback redirect. It does not reuse, claim or
impersonate the Microsoft Teams client identity, so an app registration and
user/tenant consent cannot be bypassed. Register
`http://127.0.0.1:<port>/oauth/callback` as the redirect URI, then choose
**Connect Microsoft 365** from the app menu.

```sh
export TEAMSPACE_ENTRA_CLIENT_ID='your-public-client-id'
export TEAMSPACE_ENTRA_TENANT='your-tenant-id-or-domain'
export TEAMSPACE_ENTRA_LOOPBACK_PORT='8400'
export TEAMSPACE_ENTRA_SCOPES='offline_access User.Read Chat.Read ChatMessage.Send Team.ReadBasic.All Channel.ReadBasic.All ChannelMessage.Read.All ChannelMessage.Send Calendars.ReadBasic Files.Read Files.Read.All'
cargo run
```

### Environment variables

| Variable | Purpose |
| -------- | ------- |
| `TEAMSPACE_ENTRA_CLIENT_ID` | Client ID of your Entra public-client registration. |
| `TEAMSPACE_ENTRA_TENANT` | Tenant ID or domain. |
| `TEAMSPACE_ENTRA_LOOPBACK_PORT` | Port of the loopback redirect (`127.0.0.1` only). |
| `TEAMSPACE_ENTRA_SCOPES` | Space-separated delegated scopes to request, by plain name. |
| `TEAMSPACE_CLOUD` | `global` (default), `usgov`, `usgovdod` or `china`; see below. |
| `TEAMSPACE_GRAPH_ACCESS_TOKEN` | Development only: an already-issued delegated Graph access token, used instead of sign-in. |
| `TEAMSPACE_THEME`, `TEAMSPACE_OVERLAY` | Screenshot renderer only (`--features screenshot`); the app itself never reads them. |

The four `TEAMSPACE_ENTRA_*` values are all-or-nothing: a partial set is rejected
rather than guessed, and with none set the app stays in local preview. Teamspace
bundles no client ID, tenant or scopes; those belong to the tenant owner.

`TEAMSPACE_GRAPH_ACCESS_TOKEN` is read at startup, is short-lived, is never
passed to a spawned browser launcher, and available behavior is limited to the
scopes actually granted to that token. It honors `TEAMSPACE_CLOUD` too.

### Scopes

`offline_access` and `User.Read` are required. Teamspace never reads an
`id_token`, so it does not request `openid` (and sends no `nonce`); `openid`,
`profile` and `email` in an older scope list are accepted and ignored. Everything
else is delegated and requested only if you list it; without a scope, the matching
feature is hidden, disabled or fails safely with a notice.

| Scope | Required | What it enables |
| ----- | -------- | --------------- |
| `offline_access`, `User.Read` | Yes | Sign-in, silent token renewal, your identity. |
| `Chat.Read` | For chats | Chat directory and history, Mentions view, Pinned messages, People panel (names and Owner/Guest labels), Microsoft 365 message search. |
| `ChatMessage.Send` | For sending | Chat sends (Normal, High or Urgent priority, formatting, @mentions), quote replies and reactions. |
| `Team.ReadBasic.All`, `Channel.ReadBasic.All` | For Teams | Teams and channel lists. |
| `ChannelMessage.Read.All` | For channels | Channel posts and replies (needs tenant admin consent). |
| `ChannelMessage.Send` | For channel writes | Channel root posts, replies and reactions. |
| `Calendars.ReadBasic` | For Calendar | Read-only calendar agenda and meeting-link handoff. |
| `Files.Read` | For Files | OneDrive-root metadata and the read-only text/image preview. |
| `Files.Read.All` | For channel files | The selected channel's file list. |
| `Chat.ReadWrite` | Optional | Edit and delete your own chat messages (edit in the global cloud only) and the opt-in "Send read receipts" setting. Broader than `Chat.Read`. |
| `Presence.Read.All` | Optional | Availability in the People panel (not in 21Vianet). Admin consent depends on tenant policy. Without it People shows names and roles only. |
| `Files.ReadWrite` (or `Files.ReadWrite.All`) | Optional | Attaching one file of up to 100 MiB to a chat message. Without it Attach is disabled. |

Details of the optional features:

- **Edit and delete** apply to your own chat messages only. You can edit a message
  that uses only the supported formatting and has no mentions or attachments.
  Channel posts and replies cannot be edited or deleted.
- **Read receipts**: with `Chat.ReadWrite` granted, "Send read receipts" in
  Settings > Privacy is enabled. It is off by default and per profile. When on,
  opening an unread chat tells Microsoft 365 you read it, once per view and only
  while the window is focused.
- **Attachments** are uploaded to your OneDrive folder "Microsoft Teams Chat
  Files" (an existing file with that name is never replaced) and referenced from
  the message. Just before a message with a file is sent, the file is shared
  read-only with the other members of the chat (no email is sent); if that fails
  for some people you are warned they may not be able to open it, and the message
  is still sent. Received file attachments show as read-only name chips.
- **Mentions**: typing `@` in a chat offers the members of the People roster you
  already opened for that chat (open People once to enable it). Mentions need no
  new permission and are not available in channels.

### Tokens and sign-in

Tokens stay in memory only and are never saved, rendered or logged. The access
token is renewed silently in memory shortly before it expires (and once after a
rejected read); if renewal is refused you are asked to sign in again. A message
that is rejected while the session is renewed is not re-sent automatically: send
it again. See [SECURITY.md](SECURITY.md).

### National clouds

Set `TEAMSPACE_CLOUD` to use a sovereign cloud. Accepted values are exactly
`global` (the default when unset), `usgov` (GCC High / US Government L4),
`usgovdod` (US Government DoD L5) and `china` (21Vianet). Any other value,
including a different letter case or surrounding whitespace, is rejected and the
value is never echoed; sign-in then reports an invalid configuration. An invalid
value with `TEAMSPACE_GRAPH_ACCESS_TOKEN` falls back to local preview rather than
guessing a cloud.

| Value      | Graph host                        | Sign-in host                       |
| ---------- | --------------------------------- | ---------------------------------- |
| `global`   | `graph.microsoft.com`             | `login.microsoftonline.com`        |
| `usgov`    | `graph.microsoft.us`              | `login.microsoftonline.us`         |
| `usgovdod` | `dod-graph.microsoft.us`          | `login.microsoftonline.us`         |
| `china`    | `microsoftgraph.chinacloudapi.cn` | `login.partner.microsoftonline.cn` |

The Entra app registration and consent must be created in that cloud's own portal;
a global registration does not work there. Keep listing scopes by their plain
names: outside the global cloud Teamspace qualifies them as
`https://{graph host}/{Scope}`. The cloud is fixed for the session and bound to the
tokens it issued, so a token is only ever sent to that cloud's Graph host, and file
previews download only from that cloud's SharePoint hosts. The connect menu shows
the selected cloud.

### What works with Microsoft 365

- **Chats**: bounded directory and history; a read-only Mentions view; Pinned
  messages and People panels; sends with priority, **bold**, _italic_, `code`,
  `https` links and @mentions; quote replies to up to 10 messages; a six-reaction
  palette (`👍`, `❤`, `😄`, `😮`, `😢`, `😡`; other reactions are not shown);
  message search from the global search (press Enter on the "Microsoft 365" row);
  file attachments.
- **Teams**: teams and channels, root posts, replies and channel files; text-only
  posts and replies; the same reaction palette.
- **Calendar**: a bounded metadata view, plus an explicit system-browser handoff
  for an existing safe meeting link.
- **Files**: bounded OneDrive-root metadata and an explicit-click, read-only
  preview of allow-listed text (up to 256 KiB) and images (up to 12 MiB and 16
  million pixels). Preview bytes stay in memory; nothing is saved, uploaded,
  renamed, moved, shared or deleted. A safe provider link can be opened in the
  browser.

This is not full Microsoft Teams parity. Unsupported remote operations include
editing or deleting channel posts and replies, attachments and rich formatting and
@mentions in channels, reply-to-reply, custom reactions, meeting controls and file
content mutations.

Remote writes use exact provider targets, no optimistic local overlay and targeted
reconciliation; an ambiguous result is never automatically replayed. Removing a
reaction that Graph stored under a legacy name is not verified: the control stays
locked until a refreshed history shows the requested state.

### Refresh: event-driven, no timers

Microsoft 365 data refreshes when you return to the window, open a chat or
channel, or press F5 (Cmd/Ctrl+R, or the header Refresh button). There is no
background timer, because Microsoft's Teams API polling rules forbid polling
loops. See Microsoft's
[polling requirements](https://learn.microsoft.com/en-us/graph/api/resources/teams-api-overview?view=graph-rest-1.0#polling-requirements).
Loaded messages stay visible while a refresh runs and cannot be reacted to, quoted
or edited until the fresh read confirms them. Times are shown in your local time
zone.

## Notifications

New messages show a count badge in the chat list and one in-app toast. They never
show message text, and by default not the chat name (a "Show chat names" choice
is in Settings > Notifications; a per-chat "Mute this chat" is in a chat's "..."
menu). Muted chats are remembered only as keyed digests in a private per-profile
file.

System notifications are an opt-in build feature (`cargo build --features
os-notifications`) plus an "Also use system notifications" choice in Settings >
Notifications, off by default. They appear only while the window is unfocused and
only when the app can truthfully identify itself to the OS (Linux; the bundled
macOS `.app`; Windows with a registered AppUserModelID). They show the same
count-only text and never a message.

## Cargo features

| Feature | Purpose |
| ------- | ------- |
| `screenshot` | Off-screen real-UI screenshot renderer (`--screenshot`). |
| `os-notifications` | System notifications, under a truthful app identity only. |
| `ui-snapshots` | Golden-image UI tests (implies `screenshot`); compared on macOS only. |
| `fuzzing` | Internal: exposes `teamspace::fuzz` for the separate `fuzz/` crate. |

## Verify

There is no hosted CI; run these locally before pushing:

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

UI snapshot comparison: `cargo test --locked --features ui-snapshots snapshot_tests`
(macOS). See [CONTRIBUTING.md](CONTRIBUTING.md#testing) for updating goldens.

Use `cargo run --features screenshot -- --screenshot teams` (or `activity`, `chat`,
`calendar`, `calls`, `files`) to replace the single visual-check artifact at
`artifacts/latest.png`. Prefix it with `TEAMSPACE_THEME=dark` (or `light`,
`system`) to choose the colour scheme and `TEAMSPACE_OVERLAY=settings` (or
`palette`, `search`) to open an overlay.

### Fuzzing

The `fuzz/` crate holds cargo-fuzz targets for the Graph page parser, loopback
callback, message body, next-link validation and stored-snapshot loading:

```sh
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run next_link
```

## Releases

Releases are built by hand: tag `v<Cargo.toml version>`, build with
`cargo build --locked --release` on each platform, and attach the archives and a
`SHA256SUMS` file to a GitHub release. To build the macOS app bundle from a
compiled binary:

```sh
scripts/bundle-macos.sh <binary> <version> <output-dir>
```

Set `CODESIGN_IDENTITY` to sign with a real identity (the default is ad-hoc).

## Layout

- `src/app/` - shell, authentication lifecycle, storage and coordination
- `src/features/` - feature behavior, projections, persistence and tests
- `src/integrations/` - transport, Entra authorization and provider adapters
- `src/data/` - immutable local-preview seed data
- `src/shared/` and `src/ui/` - shared models and reusable presentation

See [CONTRIBUTING.md](CONTRIBUTING.md) for architecture and safety rules.

## License

Copyright (c) 2026 Gabor Pinkova. All rights reserved. No licence is granted to
use, copy, modify or distribute this software without the written permission of
the copyright holder; see [LICENSE](LICENSE). Third-party components keep their
own licences and are shipped in each release under `licenses/`: Inter
(SIL OFL 1.1, `assets/fonts/OFL.txt`) and Fluent UI System Icons (MIT,
`assets/icons/LICENSE`); Rust dependencies are checked with `cargo deny`.
