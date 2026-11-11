# Teams channel root-post delivery: Microsoft Graph v1.0 contract

Scope: a normal, user-authored **root** post from this desktop client, not a
reply, bot message, bulk import, or background service. Sources were checked
against Microsoft documentation on 2026-11-06.

**Implemented boundary (2026-11-06):** the app sends only a text body to the
opaque team/channel route, accepts only the normal `201 Created` response as
delivery confirmation, and retains status plus `Retry-After` only. It
intentionally neither parses a successful `chatMessage` body nor emits a
diagnostic `client-request-id` yet. Recommendations below that describe a
richer receipt or request diagnostics are future work, not current behavior.

## Wire contract and receipt

```http
POST https://graph.microsoft.com/v1.0/teams/{team-id}/channels/{channel-id}/messages
Authorization: Bearer {delegated access token}
Content-Type: application/json

{
  "body": {
    "contentType": "text",
    "content": "Hello World"
  }
}
```

This route creates a new **root** channel message. A reply instead uses
`POST /teams/{team-id}/channels/{channel-id}/messages/{message-id}/replies`.
Only `body` is mandatory; `contentType: text` is an intentional safe default,
not an extra Graph requirement. [Send a channel
message](https://learn.microsoft.com/en-us/graph/api/channel-post-messages?view=graph-rest-1.0)
[chatMessage resource](https://learn.microsoft.com/en-us/graph/api/resources/chatmessage?view=graph-rest-1.0)

For the normal send flow, the documented success is **`201 Created`** with a
new `chatMessage`. The current adapter deliberately uses that status as its
entire receipt: reading or parsing a success body cannot turn an accepted,
non-idempotent post into a false failure. It does not keep Graph IDs, body,
URLs, or response DTOs in feature state. A future richer receipt could retain
the server-issued `id`, `replyToId`, `etag`, timestamps, `channelIdentity`, and
optional `webUrl` inside the integration boundary; it would need to treat the
durable message key as `(teamId, channelId, id)`, not `id` alone. `201` is
confirmation of this synchronous create; do not treat an unrelated `202
Accepted` as success for this route. The `200 OK` example on the same page is
the separate migration/import flow. [Create response and migration
example](https://learn.microsoft.com/en-us/graph/api/channel-post-messages?view=graph-rest-1.0)
[chatMessage identifiers](https://learn.microsoft.com/en-us/graph/api/resources/chatmessage?view=graph-rest-1.0)

`client-request-id` is a deferred diagnostic improvement, not an idempotency
key and not a message identifier. Microsoft recommends a new GUID on each
Graph REST request for diagnostics; the send contract documents neither an
`Idempotency-Key` nor a client-assigned message ID. [Graph best
practices](https://learn.microsoft.com/en-us/graph/best-practices-concept)

## Authorization and target boundary

The least-privileged normal-send permission is delegated
`ChannelMessage.Send`, for a **work or school** account. It does not require
admin consent by default, but tenant consent policy can still require an
administrator. A delegated call remains bounded by the signed-in user's own
Teams authorization; it does not make this app a bot or bypass membership,
licensing, moderation, or Conditional Access. Personal Microsoft accounts are
unsupported. Application `Teamwork.Migrate.All` is only for migration, not a
live unattended sender; `Group.ReadWrite.All` is backward-compatibility only.
[Send permissions](https://learn.microsoft.com/en-us/graph/api/channel-post-messages?view=graph-rest-1.0)
[ChannelMessage.Send permission](https://learn.microsoft.com/en-us/graph/permissions-reference#channelmessagesend)
[Delegated consent model](https://learn.microsoft.com/en-us/graph/permissions-overview)

An ambiguous-send reconciliation additionally needs delegated
`ChannelMessage.Read.All` to list or get channel posts. That permission is
admin-consent-required. `ChannelMessage.Send` alone can confirm a received
`201`, but cannot read a post back. [List channel-message
permissions](https://learn.microsoft.com/en-us/graph/api/channel-list-messages?view=graph-rest-1.0)
[ChannelMessage.Read.All permission](https://learn.microsoft.com/en-us/graph/permissions-reference#channelmessagereadall)

Keep the selected `team-id` and `channel-id` as opaque provider references,
not display names or a parsed Teams URL. The current adapter validates those
provider-owned targets and builds only the root-message route. If a future
adapter parses a creation body, it should additionally verify that returned
`channelIdentity` equals the target pair and that `replyToId` is null before
attaching a richer receipt to the local intent. The channel resource supports
standard, private, and shared membership types and post/chat layouts; the
route remains a root-message route. A member cannot even list private or
shared channels they do not belong to, and a shared-channel user must be a
member to retrieve it. [Channel resource](https://learn.microsoft.com/en-us/graph/api/resources/channel?view=graph-rest-1.0)
[List channels](https://learn.microsoft.com/en-us/graph/api/channel-list?view=graph-rest-1.0)

The current feature disables sending to a known archived channel:
archived-channel users cannot send new messages. Also do not blend normal creation with the migration path;
migration has its own permission, target state, authorship, and timestamp
rules. Channel moderation can prevent non-moderators from starting posts even
when the app has the delegated scope. [Archive channel](https://learn.microsoft.com/en-us/graph/api/channel-archive?view=graph-rest-1.0)
[Channel moderation](https://learn.microsoft.com/en-us/microsoftteams/manage-channel-moderation-in-teams)

## Body, markup, and size

`itemBody.contentType` is only `text` or `html`. Use `text` for arbitrary
composer input. If rich text is introduced, locally escape/allowlist it before
serializing `html`; Teams has its own HTML restrictions, and Microsoft warns
that elements such as `div` and inline styles might not behave as expected.
This is an application safety boundary, not a promise that Graph will preserve
or sanitize arbitrary markup. Mentions and attachments are separate structured
features; in particular, a mentioned message is always HTML and must have
matching `mentions` metadata. [itemBody](https://learn.microsoft.com/en-us/graph/api/resources/itembody?view=graph-rest-1.0)
[Teams messaging body model](https://learn.microsoft.com/en-us/graph/teams-messaging-overview)
[HTML caveat](https://learn.microsoft.com/en-us/graph/api/chatmessage-post?view=graph-rest-1.0)

Do not send migration-only authorship or timestamps (`from`,
`createdDateTime`) in a normal user post. The server records `createdDateTime`,
which can differ from the local submit time. Keep a conservative local request
cap below Teams' approximate 100-KB channel-post limit, including message
markup and references. [Message timestamps](https://learn.microsoft.com/en-us/graph/teams-messaging-overview)
[Teams limits](https://learn.microsoft.com/en-us/microsoftteams/limits-specifications-teams)

Microsoft explicitly prohibits using the standard create flow for migration
and says Teams must not be used as a log file; only send material people are
expected to read. [Send limitations](https://learn.microsoft.com/en-us/graph/api/channel-post-messages?view=graph-rest-1.0)

## Failure policy: delivery first, retries second

Graph error bodies are JSON `error` objects. The current adapter retains only
normalized status/retry metadata and exposes neither raw bodies nor message
content. A future diagnostic layer may retain safe values such as HTTP status,
local intent, `client-request-id`, and server request ID; it must not expose
raw provider bodies or message content in errors.
[Graph errors](https://learn.microsoft.com/en-us/graph/errors)

| Outcome | Delivery classification and action |
| --- | --- |
| `401` | Authentication is absent/invalid. Require a fresh token/sign-in; do not silently repost. |
| `403` | Access is denied: missing consent, user/channel access, license, moderation, or Conditional Access claims are possible. Show a generic scoped access failure and retain the draft; never infer which cause from text alone. |
| `404` | The selected team/channel no longer exists or is no longer addressable. Refresh that directory/selection and do not replay to a name-derived replacement. |
| `409` | Current-state conflict, not delivery success. The generic Graph guidance permits a delayed retry for a specific `Directory_ConcurrencyViolation`; the migration flow separately documents duplicate `createdDateTime` conflicts. Neither is a normal-send idempotency guarantee, so revalidate and require an explicit policy before reposting. |
| `422` | Semantically invalid request: retain for correction, then submit a new intent; no automatic replay. |
| Received `429` | A definite throttled failure. Honor `Retry-After`; if absent use bounded exponential backoff, then retry the same queued intent. Avoid immediate retries. |
| Network error, timeout, or `5xx` (`500`/`502`/`503`/`504`) | **Delivery uncertain.** Do not automatically POST again under an at-most-once UX. Refresh/reconcile the exact channel, then leave an explicit user retry decision if still unknown. |

Microsoft's generic guidance allows a delayed `503` retry, while its HTTP
retry guidance says a non-idempotent POST should be retried only when the API
documents it as safe. This endpoint supplies no such idempotency mechanism.
Therefore, automatic `5xx` replay is an explicit *at-least-once* product
choice, not reliable exactly-once delivery. `429` is different: Graph
explicitly describes it as a failed request and instructs clients to retry
after the delay. [Error status meanings](https://learn.microsoft.com/en-us/graph/errors)
[Throttling guidance](https://learn.microsoft.com/en-us/graph/throttling)
[POST retry safety](https://learn.microsoft.com/en-us/microsoft-cloud/dev/dev-proxy/concepts/http-5xx-api-errors)

Once an HTTP `201` status has been observed, a bounded response-body parse
failure must not downgrade delivery to unknown: retain the status-only receipt
and refresh normally. This matches the existing chat adapter's safe handling
of a non-idempotent success.

Teams publishes a per-channel and per-user limit of **1 request/second** for
this POST, alongside broader app limits. The current feature permits one
active remote root-post submission per session and uses the transport's serial
command queue; a received `429` honors `Retry-After` through the command retry
policy. It queues a bounded target-specific follow-up read only while the
remote connection remains available. [Teams Graph throttling
limits](https://learn.microsoft.com/en-us/graph/throttling-limits#channel-messages)

## Bounded reconciliation: what it can and cannot prove

On a `201`, the returned message is the canonical confirmation. The current
status-only adapter does not retain that DTO; a future adapter that does retain
its `id` could make an exact `GET
/teams/{team-id}/channels/{channel-id}/messages/{message-id}`. [Get a channel
message](https://learn.microsoft.com/en-us/graph/api/chatmessage-get?view=graph-rest-1.0)

After an unknown outcome, no message ID is available. A bounded refresh may
use the exact target pair and `GET .../messages?$top=50`, but only as a UI
refresh and a best-effort candidate search:

- The list endpoint returns root posts only, has a default page of 20 and a
  maximum of 50, supports only `$top` and `$expand`, and sorts roots by the
  last modification of the entire reply chain. It cannot filter on a local
  intent or impose a correlation ordering.
- A unique, exact candidate can be useful as **possible** evidence, but a
  repeated plaintext body, normalized HTML, display name, or near timestamp
  cannot safely prove it came from this intent. The server timestamp is not
  the client's submit timestamp.
- Absence from the capped page is **inconclusive**, not proof the server did
  not create it. This follows from the page cap and lack of an intent filter.
  Do not auto-restore/replay a draft on that basis.
- Bind the pending intent, refresh, and result to the same opaque team/channel
  pair. A selection change may hide the result, but must never let a different
  channel resolve the intent. Keep reconciliation state session-local and
  avoid persisting raw content, Graph URLs, tokens, or error bodies.

The last two points are conservative implementation inferences from the
documented list contract, rather than a Graph-provided deduplication protocol.
[List channel messages](https://learn.microsoft.com/en-us/graph/api/channel-list-messages?view=graph-rest-1.0)

## Repository integration considerations (inspection only)

- `src/integrations/transport.rs` supplies `WorkspaceCommand::SendChannelPost`
  and opaque team/channel references, and `Microsoft365Transport::dispatch`
  now implements it alongside `SendChatMessage`. Preserve the
  provider-ownership validation rather than deriving routes outside the
  adapter.
- `src/integrations/microsoft365/channel_posts.rs` is the root-post writer and
  the mutation template for the reply writer: text-only JSON, a local 64 KiB byte
  cap, zeroized request bytes, strict `201`, `429` handling, and no automatic
  replay for ambiguous network/5xx outcomes. (`messages.rs` is the separate
  chat-send adapter.) Its status-only transport deliberately avoids turning a confirmed
  `201` into a false failure because a success body could not be parsed.
- `src/integrations/microsoft365/channel_messages.rs` already provides a
  selection-scoped, bounded root-post read (`$top=25`, at most two pages / 50
  posts), validates continuation hosts/paths, and converts HTML to inert text.
  It currently retains display author/text rather than a stable source-user
  identity and raw body, so it cannot be used as an exact delivery matcher.
- `src/app/workspace_transport/` now queues a target-specific
  `ChannelMessages` refresh after a confirmed or ambiguous channel post while
  the adapter remains available. It never uses the chat reconciliation path,
  treats feed absence as inconclusive, and does not replay the POST.
- The configured README scope example already contains both
  `ChannelMessage.Send` and `ChannelMessage.Read.All`; document the latter's
  admin-consent requirement prominently. Graph hosts are not
  hard-coded in the adapter: routes come from `GraphCloud::graph_route`, so the
  `graph.microsoft.com` URLs in this document are the global-cloud form.

## Implemented checklist

1. The configured delegated scope includes `ChannelMessage.Send`; existing
   selected-channel root-post reads and the bounded best-effort refresh use
   admin-consented `ChannelMessage.Read.All`.
2. Send an explicit text-only `chatMessage.body` to provider-issued
   team/channel IDs with a 64-KiB byte cap; known archived channels are blocked
   before staging.
3. Treat only normal-flow `201` as delivered, with a status-only receipt that
   does not leak Graph data into feature/UI state.
4. Retry only `429` after provider-directed/backoff timing; retain definite
   failures for editing; map network/5xx to delivery-unknown with no automatic
   repost.
5. Keep one active root-post submission per session and bind its opaque
   team/channel targets to a bounded `ChannelMessages` refresh. Feed absence
   remains unknown, never a resend signal.
6. Cover the Graph route/payload/status map, exact draft preservation,
   selection changes, archived channels, transport loss, retry, and explicit
   manual review.

Deferred improvements include per-request `client-request-id` diagnostics and
a richer success receipt. Neither would supply an idempotency guarantee.
