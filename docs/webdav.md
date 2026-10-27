# WebDAV and virtual disk transports

Both transports use the same policy view. They differ in the interface exposed
to macOS and applications:

| Mode | HTTP resource | Client sees |
| --- | --- | --- |
| WebDAV | Individual source-relative file URLs and collection properties | A directory/share view. |
| Disk | One generated `card.img` representation | An exFAT disk attached by `hdiutil`. |

WebDAV extends HTTP with resource metadata, collections, namespace operations,
and locking. `PROPFIND` retrieves properties and can enumerate collection
members. Its standard is [RFC 4918](https://www.rfc-editor.org/rfc/rfc4918.html).
Diskproxy implements a read-only subset, not a general collaborative-authoring
server.

## WebDAV requests implemented here

```sh
diskproxy-server serve /path/to/folder --plugin passthrough --mode webdav
```

The startup JSON contains a URL like `http://127.0.0.1:PORT/TOKEN/`. A WebDAV
client can mount that URL. The usual `diskproxy start` launcher mounts disk mode;
it does not mount this alternative automatically.

| Method | Implementation |
| --- | --- |
| `OPTIONS` | Advertises DAV and read-only request methods. |
| `PROPFIND` | Supports Depth `0` and `1`; returns XML `207 Multi-Status`. |
| `GET` | Streams visible file bytes, optionally a single byte range. |
| `HEAD` | Returns file headers without a response body. |
| Other methods | Rejected with `403`; no writes, deletes, renames, or locks. |

The property layer exposes `displayname`, `getlastmodified`, `getcontentlength`,
`getcontenttype`, `getetag`, `resourcetype`, `supportedlock`, and `lockdiscovery`.
Directories have collection type; their content length is zero. Unknown
requested properties receive a `404` property status within the multi-status
response. Unbounded/infinity Depth is rejected. Property names are matched by
namespace/local name, not the client's chosen XML prefix. The request body is
bounded to 64 KiB.

Policy visibility is checked on direct opens as well as during listing. Hidden
files return `404`. A policy that excludes a directory must exclude descendants
too; the [example plugin](../examples/hide-plugin/README.md) demonstrates this.
The presented Last-Modified uses the policy's epoch timestamp. File bytes come
from read-only source descriptors. Listings escape XML values and URL-encode
path components; traversal, NUL, and symlink requests are rejected.

## HTTP backed exFAT

The default launcher starts disk mode and reads its URL internally:

```sh
diskproxy start /path/to/folder --plugin passthrough
```

The low-level equivalent is `diskproxy-server serve ... --mode disk`, followed
by `hdiutil attach -readonly -noautoopen -imagekey diskimage-class=CRawDiskImage
URL`. This uses HTTP byte-range reads, not WebDAV filesystem operations.
[RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#section-14.2) defines Range
requests and partial representation transfer.

Boot sectors, allocation tables, directory entries, and other exFAT metadata are
generated in memory. File-content extents point at source files; HTTP requests
map disk-image offsets onto those files. Unallocated regions are served as zero
bytes. No full disk-image file or copy of the source photos is required. The
virtual timestamp fields use valid UTC offsets. Plugins supply their presented
epoch times; they are encoded with UTC, including when a policy preserves a
legacy Pacific ingest convention.

Both transports support a single explicit, suffix, or open-ended byte range;
multiple ranges and unsatisfiable ranges are rejected. This implementation does
not provide a general conditional-request/If-Range cache protocol. Use a stable
source for a session and a fresh session after an intended change. Disk extents
check size/mtime against their snapshot before reading source bytes.

## Why the PMP workflow uses disk mode

In the original investigation, folder/WebDAV ingestion and disk ingestion
produced different history path forms, including an initial slash difference.
Correcting timestamps alone did not make that unfiltered folder ingest resume
as expected. A virtual disk selected under Disks did resume correctly. This is
an observed application behavior, not a WebDAV protocol requirement.

The PMP policy offers `hide_ingested` for either transport. It normalizes leading
slashes when comparing its immutable history snapshot. Filtering changes what
is exposed; it does not rewrite PMP's local history or the physical card.
The snapshot does not advance during ingest; restart to incorporate new history.

## Process and access model

The HTTP listener is localhost-only, and a fresh 192-bit token is part of each
session's URL. Treat the URL as a capability. This is a local tool with no TLS,
LAN sharing configuration, user-account authentication, or dynamic-plugin sandbox.
Use of standard HTTP does not imply that it is a remote storage service.

`diskproxy stop` verifies the recorded volume UUID, detaches, and only then
terminates its own server. A busy volume remains served; finish using it and
retry. Disk image validity and source metadata are not recoverable backups.
The [main README](../README.md) lists filename, timestamp, Unicode, and platform
limits; the [PMP guide](../plugins/pmp/README.md) covers registration semantics.
