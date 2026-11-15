# Administration through the proxy

Jellyswarrm normally reports every user as a non-administrator. This fork lets a
user manage media and open a server's own administration **only where their
linked account on that backend is an administrator there**. The backend remains
the final authority: the proxy only decides where a request may go.

## Item management

Editing metadata, images, identifying, refreshing metadata, managing subtitles
and lyrics, and deleting items work from the usual client menus.

- Requests that only an administrator may make (`/Items/{id}/Refresh`,
  `/Images`, `/MetadataEditor`, `/RemoteSearch`, `/ExternalIdInfos`,
  `/ContentType`, subtitle and lyric writes) are sent **only** to backends where
  the user is an administrator. Anywhere else they are refused with 403 without
  contacting a backend.
- `POST|DELETE /Items/{id}` is also how users edit or delete their own playlists
  and collections, so it is only *steered* to an administrator backend when one
  owns the item; otherwise the owning backend decides with the user's own rights.
- For a movie present on several servers, an edit or delete acts on **one**
  copy: the one on the highest-priority server where the user is an administrator.

How clients learn what to show:

- **Native apps** see `IsAdministrator = true` when the user is an administrator
  on at least one backend, and get a 403 on items elsewhere.
- **The bundled web client** is told `false` and instead reads
  `JellyswarrmCanManage` on every item (set from what the proxy already knows,
  never from a network call while answering). Menu entries appear per item.
  This needs `patches/jellyfin-web/*.patch`, applied by the `Dockerfile`.

The administrator state is learned from the backend's login response, Quick
Connect, and `GET /Users/Me` (which the web client calls at startup), then
cached for ten minutes. After a restart an item may show no management entries
until the client has called `/Users/Me` once; a management request itself always
re-checks with the backend.

## Server administration (dashboard)

Server-wide administration (`System/Configuration`, `Library/VirtualFolders`,
`ScheduledTasks`, `Plugins`, `Devices`, ...) has no single target in a merged
view, so the merged API answers 403 for it. Open one server's own interface
instead: in the management UI, **Connected Servers → Administer**.

`/<ui>/backend/{server_id}/...` forwards HTTP and WebSocket traffic unchanged
to that backend. It requires a signed-in management-UI session and an
administrator account on that server. You sign in on that server's own login
page. The proxy's session cookie is never sent to the backend, and the backend's
cookies never reach the browser.

## Network discovery

Jellyfin apps find servers with a UDP broadcast on port 7359. The proxy answers
it as a Jellyfin server does. Controlled by `autodiscovery` (default `true`,
environment `JELLYSWARRM_AUTODISCOVERY`) and a switch in **Settings**; changes
apply without a restart.

Only one server per host may own the port. If another one holds it (for example
a Jellyfin on the same machine), the proxy logs a single error, keeps running
without discovery, and retries every few seconds. To let the proxy answer
instead, turn off discovery on the other server.
