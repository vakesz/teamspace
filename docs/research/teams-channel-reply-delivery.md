# Teams channel-reply delivery: Microsoft Graph v1.0 contract

Scope: a normal, interactive, user-authored reply to one existing Teams
**channel root post** from Teamspace. This is not a bot message, a bulk import,
or a background app-only sender. Sources below are official Microsoft
documentation and were checked on **2026-11-06**.

The sections headed **Documented fact** describe the published Graph/Teams
contract. Sections headed **Implementation recommendation** are deliberately
conservative application choices; Graph does not provide an exactly-once reply
protocol.

## Normal wire contract and receipt

**Documented fact.** Create a reply with the root message as the route target:

```http
POST https://graph.microsoft.com/v1.0/teams/{team-id}/channels/{channel-id}/messages/{root-message-id}/replies
Authorization: Bearer {delegated access token}
Content-Type: application/json

{
  "body": {
    "contentType": "text",
    "content": "I can take this."
  }
}
```

Only `body` is mandatory. `itemBody.contentType` supports `text` and `html`.
For the normal create flow, success is **`201 Created`** with the newly created
`chatMessage` in the response body. The endpoint and normal response are
documented at [Send replies to a message in a
channel](https://learn.microsoft.com/en-us/graph/api/chatmessage-post-replies?view=graph-rest-1.0);
the valid body types are documented by
[itemBody](https://learn.microsoft.com/en-us/graph/api/resources/itembody?view=graph-rest-1.0).

The returned normal reply has a server-issued `id`, a `replyToId`, and
`channelIdentity`. `chatMessage.id` is not globally unique: Microsoft documents
uniqueness only within a chat/channel/reply-to-message scope. Keep the target
as the opaque tuple `(teamId, channelId, rootMessageId)`, and retain a returned
reply ID only as a child of that tuple. [chatMessage
resource](https://learn.microsoft.com/en-us/graph/api/resources/chatmessage?view=graph-rest-1.0)

**Implementation recommendation.** Treat an observed `201` as the delivery
receipt even if parsing its JSON body later fails. Retrying after an accepted
non-idempotent POST risks a duplicate; parsing the receipt is useful for a
future richer projection, not a condition for declaring delivery.

### Do not confuse normal delivery with migration

**Documented fact.** The same route has a distinct import/migration example
whose response is **`200 OK`**, not normal-flow `201`; it uses migration fields
such as `from` and `createdDateTime`. Import is app-only, requires
`Teamwork.Migrate.All`, requires the target channel to be in migration mode,
and requires `from`. If supplied, `createdDateTime` must be valid for the
target and unique to the millisecond; a collision returns `409 Conflict`.
[Reply API migration example](https://learn.microsoft.com/en-us/graph/api/chatmessage-post-replies?view=graph-rest-1.0)
[Teams message import requirements](https://learn.microsoft.com/en-us/graph/teams-import-messages)

**Implementation recommendation.** Normal Teamspace delivery must send only
current-user content and accept only `201`. Do not send `from` or
`createdDateTime`, and never interpret migration's `200` as a normal reply
receipt.

## Authorization and target access

| Need | Least-privileged normal permission | Account / consent boundary |
| --- | --- | --- |
| Send a user-authored reply | Delegated `ChannelMessage.Send` | Work or school account only; personal Microsoft accounts are unsupported. The permission reference marks delegated admin consent as **No**. |
| Read the selected root's replies for UI refresh or best-effort reconciliation | Delegated `ChannelMessage.Read.All` | Work or school account only; delegated admin consent is **Yes**. |

The reply endpoint lists `Group.ReadWrite.All` only as a higher,
backward-compatibility option. Its application permission is
`Teamwork.Migrate.All`, and the endpoint documentation limits application use
to migration; it is not an unattended normal-send alternative.
[Reply permission table](https://learn.microsoft.com/en-us/graph/api/chatmessage-post-replies?view=graph-rest-1.0)
[ChannelMessage.Send reference](https://learn.microsoft.com/en-us/graph/permissions-reference#channelmessagesend)
[ChannelMessage.Read.All reference](https://learn.microsoft.com/en-us/graph/permissions-reference#channelmessagereadall)

**Documented fact.** Delegated access acts on behalf of the signed-in user and
cannot access data the user cannot access. Even when a scope has no default
admin-consent requirement, tenant app-consent policy can prevent user consent
and require an administrator. [Microsoft Graph permissions
overview](https://learn.microsoft.com/en-us/graph/permissions-overview)

Therefore, the delegated scope does not bypass channel membership, licensing,
Conditional Access, or moderator rules. A `403` can mean insufficient access,
licensing, or Conditional Access claims; present a generic access failure
rather than guessing a cause from the provider's error text.
[Graph error responses](https://learn.microsoft.com/en-us/graph/errors)

## Text, HTML, mentions, and attachments

**Documented fact.** A text-only composer can send `body.contentType: "text"`.
Rich content needs an HTML body; Teams client HTML has restrictions, so some
elements and inline styles do not behave as expected. [Send
chatMessage](https://learn.microsoft.com/en-us/graph/api/chatmessage-post?view=graph-rest-1.0)

A mention is not merely an `@name` string: the body must be HTML with
`<at id="N">…</at>` and the `mentions` collection must contain the matching
integer `id` and mentioned identity. Microsoft states that a message containing
mentions has an HTML body. [chatMessageMention
resource](https://learn.microsoft.com/en-us/graph/api/resources/chatmessagemention?view=graph-rest-1.0)

Attachments are likewise structured `chatMessage.attachments`, with an
`<attachment id="…"></attachment>` placement in the body. For a file-reference
attachment, Microsoft's send-message example requires that the file already be
in SharePoint and describes how its reference data is obtained. The generic
send-message document explicitly applies its URL syntax examples to replies as
well as root posts. [Message-body and attachment
model](https://learn.microsoft.com/en-us/graph/teams-messaging-overview)
[File-attachment example](https://learn.microsoft.com/en-us/graph/api/chatmessage-post?view=graph-rest-1.0)

**Implementation recommendation.** Start reply delivery with a text-only
payload. If rich text, mentions, files, cards, or hosted content are added,
give them their own validated composition path: escape/allowlist HTML, ensure
the body placeholders and metadata agree, and do not treat arbitrary HTML or a
local file path as a Graph attachment.

## Size and rate limits

**Documented fact.** Teams publishes an approximate **100 KB per channel
conversation post** limit. It includes the message, text/image links,
@mentions, connectors, and reactions; the stated 100 KB limit excludes a
base64-encoded image. The generic Graph write-request ceiling is 4 MB, but the
channel-post limit is the relevant, lower constraint here.
[Teams limits and specifications](https://learn.microsoft.com/en-us/microsoftteams/limits-specifications-teams)
[Graph request-size limit](https://learn.microsoft.com/en-us/graph/use-the-api)

For `POST /teams/{team-id}/channels/{channel-id}/messages/{message-id}/replies`,
the published Teams Graph limits are **500 rps per app**, **50 rps per app per
tenant**, **1 rps per channel**, and **1 rps per user**. Microsoft documents
the same numbers for root-message POSTs and says the POST limits are shared by
regular sends and import; limits can change. [Service-specific throttling
limits](https://learn.microsoft.com/en-us/graph/throttling-limits#teams)

**Implementation recommendation.** Use a local serialized or per-channel
write policy and keep a conservative serialized payload cap below the
approximate Teams maximum. Do not assume that a root-post and reply POST have
independent capacity just because they use different URLs.

## Error, retry, and delivery-unknown policy

**Documented fact.** Graph errors use HTTP status codes and a JSON `error`
object. Code against machine-readable `error.code` / nested `innererror.code`,
not the provider's human-readable `message`, which Microsoft says must not be
shown directly to users or treated as stable. Relevant status meanings include:

| Outcome | Published meaning / handling |
| --- | --- |
| `400`, `413`, `422` | Malformed, too-large, or semantically invalid request. Correct the draft before a new submit. |
| `401` | Authentication is missing or invalid. Refresh sign-in/token; do not silently repost. |
| `403` | Access denied, including missing permission/license; Conditional Access may require claims. |
| `404` | The addressed resource does not exist. Do not retarget based on a display name. |
| `409` | Current-state conflict. The generic Graph retry guidance applies only to a specific `Directory_ConcurrencyViolation`; migration additionally documents a duplicate-`createdDateTime` conflict. Neither is a normal-reply idempotency promise. |
| Received `429` | The request failed because the app was throttled. Honor `Retry-After` and retry only after its delay; use bounded exponential backoff when it is absent. |
| `500` / `502` / `503` / `504`, network error, timeout | A service or transport outcome; the normal reply endpoint does not document a deduplication key or an exactly-once retry contract. |

[Graph error responses](https://learn.microsoft.com/en-us/graph/errors)
[Graph throttling guidance](https://learn.microsoft.com/en-us/graph/throttling)

The distinction for `429` is important: Graph explicitly says throttled
requests fail and instructs clients to wait for `Retry-After` before retrying.
For `5xx`, Microsoft notes that a `504` does not reveal whether the server did
the work, and that clients should not automatically retry a non-idempotent
`POST` unless the API documents a way to make it safe. The reply operation
does not document one. [Microsoft guidance on 5xx and unsafe
POSTs](https://learn.microsoft.com/en-us/microsoft-cloud/dev/dev-proxy/concepts/http-5xx-api-errors)

**Implementation recommendation.** Classify an observed `429` as a definite
failed send and retry the same queued intent after the provider-directed delay.
Classify network loss, timeout, and `5xx` as **delivery unknown**, preserve the
draft, and do not automatically POST again in an at-most-once UX. Automatic
replay after such an outcome is an explicit at-least-once product decision
that can produce duplicate replies. `503` backoff guidance does not turn this
POST into an idempotent operation.

## Request diagnostics, not deduplication

**Documented fact.** Microsoft recommends generating a unique GUID for every
Graph REST request, sending it as `client-request-id`, and logging it for
support. Graph responses include `request-id`; error `innerError` can also
contain it. [Graph best practices](https://learn.microsoft.com/en-us/graph/best-practices-concept)
[Using the Graph API](https://learn.microsoft.com/en-us/graph/use-the-api)

**Implementation recommendation.** Add a per-HTTP-request `client-request-id`
and retain it with a separate local logical-intent ID plus the safe response
diagnostics (`request-id`, status, `Retry-After`). Do not treat
`client-request-id` as an idempotency key: the v1.0 reply contract documents no
client-assigned message ID, `Idempotency-Key`, or response correlation protocol
that deduplicates a retry. Log enough for support without persisting access
tokens, raw drafts, or raw provider error bodies contrary to Teamspace's data
minimization boundary.

## Exact target and flat-thread boundary

**Documented fact.** Channel root messages are listed at
`/messages`; their replies are addressed beneath that root as
`/messages/{root-message-id}/replies/{reply-id}`. The get-reply documentation
states that a reply's `replyToId` contains the ID of the root message in its
example. [Get a channel message or
reply](https://learn.microsoft.com/en-us/graph/api/chatmessage-get?view=graph-rest-1.0)

The v1.0 documentation exposes no separate nested route such as
`.../replies/{reply-id}/replies`, and it does not explicitly promise that
substituting an existing reply ID for the POST route's `{message-id}` creates a
nested reply. The channel resource also now has `post` and `chat` layout types;
`post` is the traditional root-and-replies view, while `chat` is described as a
chat-like threaded experience. [channel
resource](https://learn.microsoft.com/en-us/graph/api/resources/channel?view=graph-rest-1.0)

**Implementation recommendation.** The current Teamspace feature should allow
replying only to its selected **root** post. If a future UI offers “reply to
this reply,” resolve and validate that reply's `replyToId` root before sending,
or reject the action; do not assume an undocumented nested-reply behavior.
Treat chat-layout channel behavior as a separately verified capability rather
than widening this root-post contract by inference.

## Bounded reconciliation using the selected-replies endpoint

**Documented fact.** A read of one root's replies is:

```http
GET /teams/{team-id}/channels/{channel-id}/messages/{root-message-id}/replies?$top=50
```

It lists only that message's replies, not the parent itself. `$top` is the only
supported OData option and its maximum is 50; `ChannelMessage.Read.All` is the
least-privileged delegated read scope. Collection responses can be paged using
an opaque `@odata.nextLink`. [List channel message
replies](https://learn.microsoft.com/en-us/graph/api/chatmessage-list-replies?view=graph-rest-1.0)
[Graph pagination guidance](https://learn.microsoft.com/en-us/graph/best-practices-concept)

**Implementation recommendation.** On a confirmed `201`, use the returned
reply as the canonical receipt. After an unknown outcome, a bounded `GET` of
the exact root can refresh the UI and locate a *possible* candidate, but cannot
prove delivery:

- There is no documented filter/correlation field for a local intent, and the
  server timestamp is not a caller timestamp.
- Matching author, body, or nearby time is ambiguous; duplicate user text is
  normal.
- Absence from a single 50-item page is inconclusive, especially when
  `@odata.nextLink` exists. It must never be a resend signal.
- Keep the pending intent, read, and result bound to the same opaque
  `(team, channel, root)` tuple. A selection change may hide a result but must
  not allow another post or channel to resolve it.

These are delivery-safety inferences from the published list contract, not a
Graph-provided deduplication protocol.

## Archived, moderated, and membership-sensitive channels

**Documented fact.** Once a channel is archived, users cannot send new
messages or react to existing ones. [Archive a
channel](https://learn.microsoft.com/en-us/graph/api/channel-archive?view=graph-rest-1.0)

Channel moderation can separately control replies: moderators can control
whether team members may reply to existing channel messages. Moderation is
available for standard channels, not General or private channels; when it is
on, only moderators can start new posts. [Microsoft Teams moderation
settings](https://support.microsoft.com/en-us/teams/teams-channels/change-moderator-roles-and-settings-in-a-channel-in-microsoft-teams)

Private/shared-channel membership matters independently of a delegated scope:
members cannot even see private or shared channels they are not members of in
the channels list. Channel membership types are standard (inherits team),
private (subset), and shared (direct members can be added without the team).
[List channels](https://learn.microsoft.com/en-us/graph/api/channel-list?view=graph-rest-1.0)
[channel membership types](https://learn.microsoft.com/en-us/graph/api/resources/channel?view=graph-rest-1.0)

**Implementation recommendation.** Disable a reply composer for a
known-archived channel, but still map a race-time provider denial generically:
state may change after the directory read. Do not infer from a `403` whether
the cause was consent, membership, licensing, Conditional Access, or
moderation. Root-post and reply permission can differ in a moderated channel,
so success at one must not be used to predict success at the other.

## Differences from root-post delivery

| Concern | Root post | Reply |
| --- | --- | --- |
| Create route | `/teams/{team}/channels/{channel}/messages` | Same prefix plus `/{root-message}/replies` |
| Target needed before send | Opaque team + channel | Opaque team + channel + selected root post |
| Normal auth / status / body model | `ChannelMessage.Send`, `201`, `chatMessage` body | Same |
| Returned parent marker | Root has `replyToId: null` | Reply should bind to the selected root through `replyToId` |
| Moderation | Can restrict who starts posts | Can independently restrict member replies |
| Narrow reconciliation read | Channel root-message listing | Exact-root replies listing, `$top` only, max 50 |

The first three rows are documented by the [send-message API](https://learn.microsoft.com/en-us/graph/api/chatmessage-post?view=graph-rest-1.0) and the [reply API](https://learn.microsoft.com/en-us/graph/api/chatmessage-post-replies?view=graph-rest-1.0). The final rows combine those contracts with the channel moderation and selected-replies read rules above.

## Repository integration seams (current implementation)

These are current repository observations, not Microsoft Graph facts:

- `WorkspaceCommand::ReplyToChannelPost` carries opaque `team`, `channel`,
  `post`, and `body` references and validates Microsoft 365 ownership in
  `src/integrations/transport.rs`. The Microsoft 365 dispatcher routes it to
  `src/integrations/microsoft365/channel_reply_posts.rs`, which posts only to
  the exact root-reply route. It accepts only normal-flow `201`, sends a
  text-only body, and keeps the successful receipt status-only; it deliberately
  does not parse the returned `chatMessage` or send a client-generated
  idempotency key.
- The reply writer mirrors the deliberately conservative lifecycle of
  `src/integrations/microsoft365/channel_posts.rs`: a `429` carries retry
  metadata, while network/server ambiguity is not automatically replayed.
  The application must keep its retained text reviewable rather than treating
  either a retry or an absent bounded-read row as proof of a safe resend.
- `src/integrations/microsoft365/channel_replies.rs` already reads one selected
  root's replies at `$top=50`, validates each returned `replyToId` against the
  root, and intentionally does not follow `@odata.nextLink`. It is suitable
  for a target-bound refresh, not proof that an ambiguous send was absent.
- Delivery orchestration keeps reply refreshes target-bound: success or an
  ambiguous result may request that exact `ChannelReplies` projection only. It
  must not fan out to every thread or change root-post reconciliation semantics.

