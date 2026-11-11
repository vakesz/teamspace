# Microsoft Teams channel-files view: Microsoft Graph contract

Scope: a selection-bound, read-only view of at most one page of a selected
Teams channel's *direct* file children. It is not a OneDrive-root view, a
recursive browser, search, download/preview surface, or mutation API. Sources
were checked against Microsoft Graph v1.0 documentation on 2026-11-06.

## Required request sequence

1. Resolve the selected, opaque `team-id` and `channel-id`:

   ```http
   GET /v1.0/teams/{team-id}/channels/{channel-id}/filesFolder
   ```

   This returns a `driveItem`; its documented example contains the folder
   `id` and `parentReference.driveId`. Retain those provider identifiers only
   inside the adapter. This endpoint supports **no OData query parameters**,
   so do not append the OneDrive view's `$select` or `$top`.
   [Get filesFolder](https://learn.microsoft.com/en-us/graph/api/channel-get-filesfolder?view=graph-rest-1.0)

2. List only that folder's immediate children:

   ```http
   GET /v1.0/drives/{drive-id}/items/{folder-id}/children?
       $select=id,name,lastModifiedDateTime,size,file,folder,package,webUrl&$top=50
   ```

   This is the documented drive-qualified children route. The current
   implementation (`src/integrations/microsoft365/channel_files.rs`)
   intentionally requests exactly these metadata and facet fields (including
   `webUrl`, used only for an explicit **Open in browser** click); it does not
   request `deleted`. Do not replace it
   with `/me/drive`, derive a path from a display name, or assume that the
   Team/group drive owns every channel. Using the resolver's returned pair is
   important: for migrated and newly created private channels, `filesFolder`
   can return the drive root rather than a name-matching folder.
   [Get filesFolder](https://learn.microsoft.com/en-us/graph/api/channel-get-filesfolder?view=graph-rest-1.0)
   [List children](https://learn.microsoft.com/en-us/graph/api/driveitem-list-children?view=graph-rest-1.0)

This is a two-request read, not a supported
`/teams/{team-id}/channels/{channel-id}/filesFolder/children` shortcut.

## Permission boundary

For the combined workflow, the exact least-privilege **delegated work or
school** permission is `Files.Read.All`: it is the least-privileged permission
listed for `GET .../filesFolder`. The children API alone lists `Files.Read` as
its least-privileged delegated permission, but that does not change the
resolver's documented least-privilege choice. The resolver documentation also
lists `Files.Read` as a supported alternative; use `Files.Read.All` when the
goal is the documented least-privilege contract rather than relying on that
alternative. Do not add write or `Sites.*` permissions for this read-only
slice. [Get filesFolder permissions](https://learn.microsoft.com/en-us/graph/api/channel-get-filesfolder?view=graph-rest-1.0)
[List children permissions](https://learn.microsoft.com/en-us/graph/api/driveitem-list-children?view=graph-rest-1.0)

`Files.Read.All` delegated means the app can read all files the signed-in user
can access, even though this feature must only call the selected channel's
resolved folder. It does not require admin consent according to the permissions
reference, but tenant policy can still restrict consent. `Files.Read.Selected`
is not a substitute: Microsoft documents it for Office 365 file handlers and
says not to use it for direct Microsoft Graph API calls.
[Permissions reference](https://learn.microsoft.com/en-us/graph/permissions-reference)

The existing `Team.ReadBasic.All` and `Channel.ReadBasic.All` scopes remain
relevant to obtaining a selectable Team/channel directory, but are not the
files-folder operation's least-privilege permission.

## Bounded response and data model

`list children` supports only `$expand`, `$select`, `$skipToken`, `$top`, and
`$orderby`; do not assume `$filter`, `$search`, `$count`, or `$skip` work here.
The service's default page is 200 items and emits `@odata.nextLink` when more
are available. For this view, request `$top=50`, enforce a local 50-item cap
and a response-byte cap, never follow the cursor, and report only that the
view may be incomplete when a next link is present. A response with exactly 50
items and no next link is not proof that more exist.
[List children query and paging behavior](https://learn.microsoft.com/en-us/graph/api/driveitem-list-children?view=graph-rest-1.0)
[Microsoft Graph paging](https://learn.microsoft.com/en-us/graph/paging)

Normalize only presentation-safe metadata:

| Field/facet | Contract |
| --- | --- |
| `id` | Opaque ID, unique only within its drive; validate/bound it and never reconstruct a route from its display name. |
| `name`, `lastModifiedDateTime`, `size` | Display metadata; sanitize controls/whitespace, bound lengths, and accept only non-negative numeric sizes. |
| `file`, `folder`, `package` | Facets, not actions. `folder` and `package` can contain children; classify an ambiguous or malformed combination as unknown. |
| `parentReference.driveId` | Needed only from the resolved folder to form the second request; reject a missing/malformed value rather than guessing a user or group drive. |
| `webUrl` | Retained only as a validated, opaque `ProviderBrowserUrl` for an explicit **Open in browser** click, bound to the item target and revalidated as HTTPS at the app browser boundary; never a preview or download target. |
| Exclude | `content`, `@microsoft.graph.downloadUrl`, thumbnails, permissions, sharing state, and raw DTO/error data. |

`driveItem` represents both files and folders; folders (and packages) can have
children. The docs also call out that a download URL is bearer-like,
short-lived, and must not be cached—another reason it should not cross this
read-only metadata seam.
[driveItem resource](https://learn.microsoft.com/en-us/graph/api/resources/driveitem?view=graph-rest-1.0)
[itemReference resource](https://learn.microsoft.com/en-us/graph/api/resources/itemreference?view=graph-rest-1.0)

If conditional refresh is added, `list children` permits `If-None-Match` and
can return `304 Not Modified`; represent that explicitly or do not send the
header. Treating only `200` as success would otherwise turn a valid cached
response into an unavailable state.
[List children request headers](https://learn.microsoft.com/en-us/graph/api/driveitem-list-children?view=graph-rest-1.0)

## Errors, cancellation, and trust boundaries

- Map `401` to reauthentication, `403` to a generic access-denied state, and
  `429` to a throttled state using `Retry-After`. A `403` can mean missing
  permission, user access, licensing, or conditional-access claims; do not
  infer or render a raw provider message. `404` means the selected resource
  was not found, so clear only that selection's old rows and offer a retry.
  Treat `400` and `5xx` as unavailable/malformed/transient without exposing
  response bodies. [Graph errors](https://learn.microsoft.com/en-us/graph/errors)
  [Graph best practices](https://learn.microsoft.com/en-us/graph/best-practices-concept)
- For `429`, do not immediately retry. Wait the server's `Retry-After` value;
  if absent, use bounded exponential backoff. [Graph throttling
  guidance](https://learn.microsoft.com/en-us/graph/throttling)
- A channel name with certain special characters can make `filesFolder` return
  `400`; use provider-issued IDs as URL path segments and surface a generic
  per-selection failure. [Known issues](https://learn.microsoft.com/en-us/graph/known-issues)
- Check cooperative cancellation before resolving, after the resolver returns,
  before the children request, after it returns, while normalizing items, and
  immediately before publishing. Bind both the request and projection to the
  opaque Team and channel targets, so switching either selection cannot display
  stale rows.
- Keep the returned `driveId`, folder ID, pagination URL, Graph error body,
  token, and any raw URL out of UI state, persistence, logs, and notifications.
  If a future feature follows `@odata.nextLink`, Graph says to use the whole
  URL as an opaque value; additionally accept it only through
  `GraphCloud::validated_next_link` (HTTPS on the configured cloud's Graph
  host), reject credentials/fragments, and require the originally resolved children
  path before attaching a bearer token. This is product-local hardening to
  prevent a cursor from widening a token-bearing request.

## Relationship to the OneDrive-root adapter

`src/integrations/microsoft365/channel_files.rs` is the channel-files adapter
and is deliberately separate from `src/integrations/microsoft365/files.rs`,
which is the bounded **OneDrive-root** adapter. Do not merge or copy one into the
other; the differences below are why:

| OneDrive-root adapter | Channel-files adapter |
| --- | --- |
| Hard-codes `/me/drive/root/children` and `SyncInterest::Files`. | Resolves the selected Team/channel folder first and polls under `SyncInterest::ChannelFiles { team, channel }`, so personal-root items are never mislabeled as channel files. |
| One request with `$select` and `$top`. | Two requests: `filesFolder` with no query (it does not support OData options), then the drive-qualified children route with `$select` and `$top`. |
| Bootstrap scope `Files.Read`. | `Files.Read.All`, the documented least-privilege scope for resolving a work/school channel folder (`Files.Read` is documented as an alternative, so do not claim it categorically cannot work). The README scope table and the access-denied copy say so. |
| `RemoteFilesProjection` carries no Team/channel binding. | `RemoteChannelFilesProjection` is bound to its opaque team/channel selection, so a late result cannot overwrite the visible selection. |
| Knows `file` and `folder`. | Also classifies `package` conservatively (a package can have children); no facet becomes an open/download capability. |
| Observes but never follows `@odata.nextLink`. | Keeps the one-page policy (at most 50 items, with a truncation notice). Any future pagination must validate the opaque continuation first. |
| Cancellation is checked around one HTTP request. | Cancellation is also checked between folder resolution and children enumeration. With the current blocking client, cancellation suppresses a stale result but does not necessarily interrupt an in-flight socket call. |
| Status mapper accepts only `200`. | Same; handle `304` separately before adding `If-None-Match`. |

The response cap, item cap, identifier validation, display-string sanitization,
non-leaking failure categories, no-redirect client and one-page cursor policy are
shared safeguards.

## Minimum verification cases

1. Exact two requests: no query on `filesFolder`; the child URL uses the
   resolver's `parentReference.driveId` and folder `id`, never `/me`.
2. `Files.Read.All` scope/help text; `403` clears only the selected channel's
   data while retaining the authenticated identity.
3. Missing/invalid `id` or `parentReference.driveId`, malformed facets,
   duplicate IDs, oversized body, and control-character names never leak raw
   provider data.
4. One 50-item page plus `@odata.nextLink` produces a truncation notice and
   no second request; no recursive request occurs for a folder/package child.
5. Cancellation after the folder response prevents the child request; a
   cancellation after either response prevents publication; a new Team/channel
   selection rejects an old projection.
6. `401`, `403`, `404`, `429` with/without `Retry-After`, `400`, `5xx`, and
   optional `304` each produce the documented scoped outcome without logging
   tokens, raw Graph bodies, continuation URLs, or download URLs.
