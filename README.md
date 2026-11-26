# Dufs

[![Docker](https://github.com/lunatmay/dufs/actions/workflows/docker.yaml/badge.svg)](https://github.com/lunatmay/dufs/actions/workflows/docker.yaml)

Dufs is a standalone utility file server that supports static serving, uploading, searching, access control, and WebDAV. This project is based on [upstream Dufs](https://github.com/sigoden/dufs).

![demo](https://user-images.githubusercontent.com/4012553/220513063-ff0f186b-ac54-4682-9af4-47a9781dee0d.png)

## Features

- Serve static files
- Download folder as zip file
- Upload files and folders (Drag & Drop)
- Create/Edit/Search files
- Resumable/partial uploads/downloads
- Access control
- Support https
- [WebDAV](#webdav-protocol-support) (Class 1 and Class 3; no Class 2 locking)
- Easy to use with curl

## WebDAV protocol support

Dufs implements a filesystem-backed, non-locking WebDAV server. It follows standard HTTP and WebDAV interfaces rather than requiring a specialized client.

### Compliance classes

| Class ([RFC 4918](https://www.rfc-editor.org/rfc/rfc4918)) | Status | Description |
| --- | --- | --- |
| 1 | Supported | Resource and collection management, property queries, property-update responses, and conditional requests. |
| 2 | **Not supported** | No WebDAV resource locks. `LOCK` and `UNLOCK` return `405 Method Not Allowed`; no fictitious lock tokens are issued or acknowledged. |
| 3 | Supported | RFC 4918 revision of the WebDAV protocol, without Class 2 locking. |

`OPTIONS` advertises `DAV: 1, 3` for directory-backed WebDAV resources, with `extended-mkcol` where collection creation is available. `Allow` describes the available methods. Tagged and untagged `If` conditions support ETags and collection synchronization tokens; lock tokens never match.

The supported classes describe the implemented protocol behavior, not independent certification. Development and protocol regression tests currently target Linux amd64; full third-party WebDAV conformance and other operating system behavior have not been independently verified.

### Protocol and RFC extensions

| RFC | Implemented behavior | Limitations |
| --- | --- | --- |
| [9110](https://www.rfc-editor.org/rfc/rfc9110) — HTTP semantics | GET, HEAD, ranges, PUT, DELETE, ETags, and conditional requests. | Strong ETags are derived from filesystem metadata rather than content hashes. |
| [4918](https://www.rfc-editor.org/rfc/rfc4918) — WebDAV | PROPFIND (`prop`, `allprop`, `include`, `propname`), PROPPATCH, MKCOL, COPY, MOVE, DELETE, and WebDAV `If`. | Class 2 is excluded; arbitrary dead properties are not persisted; infinite-depth PROPFIND and recursive copying of symlink-containing collections are not supported. |
| [5789](https://www.rfc-editor.org/rfc/rfc5789) — HTTP PATCH | Atomic offset-based patch documents (`application/vnd.dufs.offset-patch`). | A Dufs-specific document format, not JSON Patch or a generic byte-range PATCH standard. |
| [4331](https://www.rfc-editor.org/rfc/rfc4331) — Quota and size properties | `DAV:quota-available-bytes` and `DAV:quota-used-bytes` on collections; storage exhaustion returns 507. | Reports operating-system volume capacity; no separate per-user or per-directory quota database. |
| [8144](https://www.rfc-editor.org/rfc/rfc8144) — Prefer | `return=minimal` for property queries and successful property/extended collection changes; `depth-noroot` for Depth 1 PROPFIND; `return=representation` for supported writes and failed conditional writes. | Preferences are applied only where supported and reported in `Preference-Applied`; a representation is returned only where readable and available. |
| [9530](https://www.rfc-editor.org/rfc/rfc9530) — Digest fields | SHA-256 and SHA-512 `Content-Digest` / `Repr-Digest` for requested file GET, HEAD and ranges; supplied PUT/PATCH digests are validated before publishing. | Multipart content digests and digests for generated directory/XML responses are omitted. |
| [6578](https://www.rfc-editor.org/rfc/rfc6578) — Collection synchronization | `REPORT` with `DAV:sync-collection`, `DAV:sync-token`, `DAV:supported-report-set`, and synchronization-token `If` conditions. | Immediate members only (`sync-level` 1); no infinite-level traversal or partial-token paging; bounded process-local history. |
| [5323](https://www.rfc-editor.org/rfc/rfc5323) — SEARCH | `DAV:basicsearch`, property selection, scope/depth, comparison and Boolean operators, LIKE, sorting, and limits. Discovered through `DASL: <DAV:basicsearch>`. | Requires `--allow-search`. No optional full-text search, language matching, typed literals, case-folded search, query-schema discovery, or multiple scopes. |
| [5689](https://www.rfc-editor.org/rfc/rfc5689) — Extended MKCOL | XML MKCOL for the ordinary DAV collection type; property errors roll back the entire creation. | No special collection types or custom property persistence. Unsupported types and properties are rejected explicitly. |

### Operational and interoperability limits

- Collection PROPFIND supports `Depth: 0` and `Depth: 1`. An infinite depth (including the implicit default when Depth is absent) is rejected with the standard `DAV:propfind-finite-depth` error. Single-file serving mode does not advertise the collection protocol.
- PROPPATCH reports per-property status transactionally. Protected live properties and arbitrary dead-property writes are rejected; optional properties such as `DAV:creationdate` are not implemented.
- MKCOL, PUT, COPY, and MOVE require an existing parent collection. COPY and MOVE operate within the server. Recursive operations remain subject to the configured access rules. A failed recursive COPY is rolled back rather than completing a partial copy. Recursive COPY of a collection containing symbolic links is rejected. Links outside the serving root are denied by default; following them is a separate administrator-configured server policy, not a WebDAV requirement.
- PUT and offset PATCH use temporary files and atomic replacement. Interrupted updates leave the previous complete file intact. Conditional write serialization is process-local: use one server instance per writable storage area, and do not expect it to coordinate unrelated processes or provide transactions spanning multiple resources.
- RFC 6578 history is bounded: up to 16 collection/user views, 4,096 immediate members, 128 revision tokens, and 8,192 changed-member records per view. Restart, eviction, or external changes that cannot be reconstructed reliably invalidate tokens. Clients receive `403` with `DAV:valid-sync-token` and must make a new initial report. An unrepresentable result limit returns 507. Direct external file edits are observed during scans; transient external changes concurrent with server writes cannot be guaranteed without a filesystem change journal.
- SEARCH is bounded to 10,000 visited resources, 1,000 returned records, and 8 MiB of retained result data per request. Truncated results include a 507 status for the search arbiter. SEARCH and REPORT do not grant additional access privileges.

Other WebDAV extensions are not currently implemented, including DeltaV versioning (RFC 3253), WebDAV ACL (RFC 3744), current-user-principal (RFC 5397), redirect references (RFC 4437), bindings (RFC 5842), POST-based member creation (RFC 5995), CalDAV (RFC 4791), and CardDAV (RFC 6352). Dufs's own access controls remain separate from WebDAV ACL.

## Install

This project publishes Linux amd64 Docker images to GitHub Container Registry on version tags.

### With docker

```
docker run -v `pwd`:/data -p 5000:5000 --rm ghcr.io/lunatmay/dufs:latest /data -A
```

## CLI

```
Dufs is a distinctive utility file server - https://github.com/sigoden/dufs

Usage: dufs [OPTIONS] [serve-path]

Arguments:
  [serve-path]  Specific path to serve [default: .]

Options:
  -c, --config <file>        Specify configuration file
  -b, --bind <addrs>         Specify bind address or unix socket
  -p, --port <port>          Specify port to listen on [default: 5000]
      --path-prefix <path>   Specify a path prefix
      --hidden <value>       Hide paths from directory listings, e.g. tmp,*.log,*.lock
  -a, --auth <rules>         Add auth roles, e.g. user:pass@/dir1:rw,/dir2
  -A, --allow-all            Allow all operations
      --allow-upload         Allow upload files/folders
      --allow-delete         Allow delete files/folders
      --allow-search         Allow search files/folders
      --allow-symlink        Allow symlink to files/folders outside root directory
      --allow-archive        Allow download folders as archive file
      --allow-hash           Allow ?hash query to get file sha256 hash
      --enable-cors          Enable CORS, sets `Access-Control-Allow-Origin: *`
      --render-index         Serve index.html when requesting a directory, returns 404 if not found index.html
      --render-try-index     Serve index.html when requesting a directory, returns directory listing if not found index.html
      --render-spa           Serve SPA(Single Page Application)
      --assets <path>        Set the path to the assets directory for overriding the built-in assets
      --log-format <format>  Customize http log format
      --log-file <file>      Specify the file to save logs to, other than stdout/stderr
      --compress <level>     Set zip compress level [default: low] [possible values: none, low, medium, high]
      --completions <shell>  Print shell completion script for <shell> [possible values: bash, elvish, fish, powershell, zsh]
      --tls-cert <path>      Path to an SSL/TLS certificate to serve with HTTPS
      --tls-key <path>       Path to the SSL/TLS certificate's private key
  -h, --help                 Print help
  -V, --version              Print version
```

## Examples

Serve current working directory in read-only mode

```
dufs
```

Allow all operations like upload/delete/search/create/edit...

```
dufs -A
```

Only allow upload operation

```
dufs --allow-upload
```

Serve a specific directory

```
dufs Downloads
```

Serve a single file

```
dufs linux-distro.iso
```

Serve a single-page application like react/vue

```
dufs --render-spa
```

Serve a static website with index.html

```
dufs --render-index
```

Require username/password

```
dufs -a admin:123@/:rw
```

Listen on specific host:ip 

```
dufs -b 127.0.0.1 -p 80
```

Listen on unix socket
```
dufs -b /tmp/dufs.socket
```

Use https

```
dufs --tls-cert my.crt --tls-key my.key
```

## API

Upload a file

```sh
curl -T path-to-file http://127.0.0.1:5000/new-path/path-to-file
```

Download a file
```sh
curl http://127.0.0.1:5000/path-to-file           # download the file
curl http://127.0.0.1:5000/path-to-file?hash      # retrieve the sha256 hash of the file
```

Download a folder as zip file

```sh
curl -o path-to-folder.zip http://127.0.0.1:5000/path-to-folder?zip
```

Delete a file/folder

```sh
curl -X DELETE http://127.0.0.1:5000/path-to-file-or-folder
```

Create a directory

```sh
curl -X MKCOL http://127.0.0.1:5000/path-to-folder
```

Move the file/folder to the new path

```sh
curl -X MOVE http://127.0.0.1:5000/path -H "Destination: http://127.0.0.1:5000/new-path"
```

List/search directory contents

```sh
curl http://127.0.0.1:5000?q=Dockerfile           # search for files, similar to `find -name Dockerfile`
curl http://127.0.0.1:5000?simple                 # output names only, similar to `ls -1`
curl http://127.0.0.1:5000?json                   # output paths in json format
```

With authorization (Both basic or digest auth works)

```sh
curl http://127.0.0.1:5000/file --user user:pass                 # basic auth
curl http://127.0.0.1:5000/file --user user:pass --digest        # digest auth
```

Resumable downloads

```sh
curl -C- -o file http://127.0.0.1:5000/file
```

### Offset PATCH extension

Dufs also supports an offset-based patch document (not a WebDAV method):
`Content-Type: application/vnd.dufs.offset-patch` and an
`X-Update-Range` header of `append` or `bytes=N-`. The document contains
raw replacement bytes starting at that offset. Existing bytes beyond the
replacement are preserved. PATCH publishes the entire update atomically;
an interrupted PUT or PATCH leaves the previous complete file unchanged.

Use a known base representation and its strong ETag in an `If-Match`
header before sending an offset update. The server's current file length
alone cannot identify which upload produced its contents. Retrying a full
PUT is the appropriate recovery after an interrupted upload.

Health checks

```sh
curl http://127.0.0.1:5000/__dufs__/health
```

<details>
<summary><h2>Advanced Topics</h2></summary>

### Access Control

Dufs supports account based access control. You can control who can do what on which path with `--auth`/`-a`.

```
dufs -a admin:admin@/:rw -a guest:guest@/
dufs -a user:pass@/:rw,/dir1 -a @/
```

1. Use `@` to separate the account and paths. No account means anonymous user.
2. Use `:` to separate the username and password of the account.
3. Use `,` to separate paths.
4. Use path suffix `:rw`/`:ro` set permissions: `read-write`/`read-only`. `:ro` can be omitted.

- `-a admin:admin@/:rw`: `admin` has complete permissions for all paths.
- `-a guest:guest@/`: `guest` has read-only permissions for all paths.
- `-a user:pass@/:rw,/dir1`: `user` has read-write permissions for `/*`, has read-only permissions for `/dir1/*`.
- `-a @/`: All paths is publicly accessible, everyone can view/download it.

Recursive operations respect nested rules: deleting or moving a directory is refused when its subtree contains a path with stricter permissions. An account with `:rw` on `/` can therefore not remove or relocate a child that is restricted to `:ro` through `DELETE /` or a directory `MOVE`. Such a refusal returns `403 Forbidden` with `X-Dufs-Error-Code: readonly-descendant`; a plain permission denial returns `X-Dufs-Error-Code: forbidden`.

**Auth permissions are restricted by dufs global permissions.** If dufs does not enable upload permissions via `--allow-upload`, then the account will not have upload permissions even if it is granted `read-write`(`:rw`) permissions.

Permissions are evaluated against both the request path and the canonical path it resolves to through symbolic links; a link cannot grant more access than its target. See [SECURITY.md](SECURITY.md) for the detailed rules.

#### Hashed Password

DUFS supports the use of sha-512 hashed password.

Create hashed password:

```sh
$ openssl passwd -6 123456 # or `mkpasswd -m sha-512 123456`
$6$tWMB51u6Kb2ui3wd$5gVHP92V9kZcMwQeKTjyTRgySsYJu471Jb1I6iHQ8iZ6s07GgCIO69KcPBRuwPE5tDq05xMAzye0NxVKuJdYs/
```

Use hashed password:

```sh
dufs -a 'admin:$6$tWMB51u6Kb2ui3wd$5gVHP92V9kZcMwQeKTjyTRgySsYJu471Jb1I6iHQ8iZ6s07GgCIO69KcPBRuwPE5tDq05xMAzye0NxVKuJdYs/@/:rw'
```
> The hashed password contains `$6`, which can expand to a variable in some shells, so you have to use **single quotes** to wrap it.

Two important things for hashed passwords:

1. Dufs only supports sha-512 hashed passwords, so ensure that the password string always starts with `$6$`.
2. Digest authentication does not function properly with hashed passwords.


### Hide Paths

Dufs supports hiding paths from directory listings via option `--hidden <glob>,...`.

```
dufs --hidden .git,.DS_Store,tmp
```

> The glob used in --hidden only matches file and directory names, not paths. So `--hidden dir1/file` is invalid.

```sh
dufs --hidden '.*'                          # hidden dotfiles
dufs --hidden '*/'                          # hidden all folders
dufs --hidden '*.log,*.lock'                # hidden by exts
dufs --hidden '*.log' --hidden '*.lock'
```

### Log Format

Dufs supports customize http log format with option `--log-format`.

The log format can use following variables.

| variable     | description                                                               |
| ------------ | ------------------------------------------------------------------------- |
| $remote_addr | client address                                                            |
| $remote_user | user name supplied with authentication                                    |
| $request     | full original request line                                                |
| $status      | response status                                                           |
| $http_       | arbitrary request header field. examples: $http_user_agent, $http_referer |


The default log format is `'$time_iso8601 $log_level - $remote_addr "$request" $status`.
```
2022-08-06T06:59:31+08:00 INFO - 127.0.0.1 "GET /" 200
```

A json log format is also supported.
```
dufs --log-format '{"time":"$time_local","addr":"$remote_addr","uri":"$request_uri", "method":"$request_method","status":$status}'

{"time":"2022-08-06T06:59:31+08:00","addr":"127.0.0.1","uri":"/", "method":"GET","status":200}
```

Disable http log
```
dufs --log-format=''
```

Log user-agent
```
dufs --log-format '$remote_addr "$request" $status $http_user_agent'
```
```
2022-08-06T06:53:55+08:00 INFO - 127.0.0.1 "GET /" 200 Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/104.0.0.0 Safari/537.36
```

Log remote-user
```
dufs --log-format '$remote_addr $remote_user "$request" $status' -a /@admin:admin -a /folder1@user1:pass1
```
```
2022-08-06T07:04:37+08:00 INFO - 127.0.0.1 admin "GET /" 200
```

## Environment variables

All options can be set using environment variables prefixed with `DUFS_`.

```
[serve-path]                DUFS_SERVE_PATH="."
    --config <file>         DUFS_CONFIG=config.yaml
-b, --bind <addrs>          DUFS_BIND=0.0.0.0
-p, --port <port>           DUFS_PORT=5000
    --path-prefix <path>    DUFS_PATH_PREFIX=/dufs
    --hidden <value>        DUFS_HIDDEN=tmp,*.log,*.lock
-a, --auth <rules>          DUFS_AUTH="admin:admin@/:rw|@/" 
-A, --allow-all             DUFS_ALLOW_ALL=true
    --allow-upload          DUFS_ALLOW_UPLOAD=true
    --allow-delete          DUFS_ALLOW_DELETE=true
    --allow-search          DUFS_ALLOW_SEARCH=true
    --allow-symlink         DUFS_ALLOW_SYMLINK=true
    --allow-archive         DUFS_ALLOW_ARCHIVE=true
    --allow-hash            DUFS_ALLOW_HASH=true
    --enable-cors           DUFS_ENABLE_CORS=true
    --render-index          DUFS_RENDER_INDEX=true
    --render-try-index      DUFS_RENDER_TRY_INDEX=true
    --render-spa            DUFS_RENDER_SPA=true
    --assets <path>         DUFS_ASSETS=./assets
    --log-format <format>   DUFS_LOG_FORMAT=""
    --log-file <file>       DUFS_LOG_FILE=./dufs.log
    --compress <compress>   DUFS_COMPRESS=low
    --tls-cert <path>       DUFS_TLS_CERT=cert.pem
    --tls-key <path>        DUFS_TLS_KEY=key.pem
```

## Configuration File

You can specify and use the configuration file by selecting the option `--config <path-to-config.yaml>`.

The following are the configuration items:

```yaml
serve-path: '.'
bind: 0.0.0.0
port: 5000
path-prefix: /dufs
hidden:
  - tmp
  - '*.log'
  - '*.lock'
auth:
  - admin:admin@/:rw
  - user:pass@/src:rw,/share
  - '@/'  # According to the YAML spec, quoting is required.
allow-all: false
allow-upload: true
allow-delete: true
allow-search: true
allow-symlink: true
allow-archive: true
allow-hash: true
enable-cors: true
render-index: true
render-try-index: true
render-spa: true
assets: ./assets/
log-format: '$remote_addr "$request" $status $http_user_agent'
log-file: ./dufs.log
compress: low
tls-cert: tests/data/cert.pem
tls-key: tests/data/key_pkcs1.pem
```

### Customize UI

Dufs allows users to customize the UI with your own assets.

```
dufs --assets my-assets-dir/
```

> If you only need to make slight adjustments to the current UI, you copy dufs's [assets](https://github.com/sigoden/dufs/tree/main/assets) directory and modify it accordingly. The current UI doesn't use any frameworks, just plain HTML/JS/CSS. As long as you have some basic knowledge of web development, it shouldn't be difficult to modify.

Your assets folder must contains a `index.html` file.

`index.html` can use the following placeholder variables to retrieve internal data.

- `__INDEX_DATA__`: directory listing data
- `__ASSETS_PREFIX__`: assets url prefix
- `__PATH_PREFIX__`: path prefix

> A customized 404.html page is also supported.

Here are some Third-party customize UI project:

- https://github.com/TransparentLC/dufs-material-assets
- https://github.com/cercky/dufs_web
- https://github.com/52funny/dufs-tabler-web
- https://github.com/mikigo/dufs-shadcn-assets

</details>

## License

Copyright (c) 2022-2024 dufs-developers.

dufs is made available under the terms of either the MIT License or the Apache License 2.0, at your option.

See the LICENSE-APACHE and LICENSE-MIT files for license details.
