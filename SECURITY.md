# Security Policy

## Supported source

Security fixes are made against the latest source on `main`.

## Reporting a vulnerability

Please do not disclose a suspected vulnerability in a public issue. Report it
privately through this repository's [GitHub security advisory form](https://github.com/vakesz/teamspace/security/advisories/new),
including reproducible steps and the affected revision when possible.

Do not include access tokens, authorization codes, refresh tokens, personal data
or production credentials in a report.

## Security properties

These are the properties the code is built to keep; a violation of any of them is
a vulnerability worth reporting. The rules behind them are in
[CONTRIBUTING.md](CONTRIBUTING.md#security--tokens).

- **Local preview makes no network requests.** Microsoft 365 is opt-in and
  requires a tenant-owned Entra public-client registration; the app bundles no
  client ID and never uses a first-party Microsoft client identity.
- **Tokens stay in memory.** Access tokens, authorization codes, refresh tokens,
  PKCE verifiers, state and nonce values are never persisted, logged or rendered.
  The refresh token is held only by the worker that owns the Graph session and is
  zeroized when the session drops or Entra rejects it. Raw tokens never leave
  `src/integrations/`; app code moves only an opaque handoff.
- **Development token.** `TEAMSPACE_GRAPH_ACCESS_TOKEN` is an already-issued,
  short-lived delegated token read at startup. It is never passed to a spawned
  browser launcher and grants only what it was issued for.
- **Loopback sign-in.** Authorization code + PKCE uses a listener bound to the
  exact registered `127.0.0.1` redirect only. It accepts a few complete,
  header-only requests, validates host, path and state, waits at most five
  minutes, and closes when dropped.
- **No persistence of remote data.** Microsoft 365 messages, files, events,
  rosters, presence and search results are session-only and are cleared on
  disconnect or profile change. Local preview data is stored per profile in
  private (owner-only), atomically written files.
- **Tokens go only to the right host.** Every Graph, login and download host
  comes from the selected cloud (`TEAMSPACE_CLOUD`); a token is bound to its
  cloud, and paging cursors are accepted only on that cloud's Graph host and the
  exact path requested.
- **Preauthenticated URLs are secrets.** A file-preview redirect or upload-session
  URL must be HTTPS on the cloud's content hosts, carries no credentials, is
  followed without the Graph bearer token where required, and is never rendered,
  logged, persisted or put in a `Debug` string.
- **Browser links are revalidated.** A link or meeting URL from Graph opens in the
  system browser only after an explicit click and a final HTTPS check.
- **Writes are never replayed.** An ambiguous or rejected write is reconciled
  against a fresh provider read, never re-sent automatically.
- **Notifications leak no content.** In-app and OS notifications carry only
  counts; muted chats are stored as salted digests, never chat IDs.
- **Fuzzed boundaries.** The Graph page parser, loopback callback, message-body
  parser, next-link validator and stored-snapshot loader are fuzzed (see
  `fuzz/`).
