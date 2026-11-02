# Additional Cloud Providers: WebDAV, S3-compatible, Dropbox — Design

Date: 2026-10-21
Status: Draft for approval
Scope owner: SaveSync core (+ thin desktop wiring)

## Intent

Google Drive is the only remote provider today. It works, and the provider
boundary (`savesync-core/src/provider.rs`, `SyncProvider`) has proven the
abstraction: `LocalFolderProvider` and `GoogleDriveProvider` implement the same
six methods, and `SaveSyncEngine::sync(provider, game_id)` treats them
identically. But real users do not all use Google. People running homelab
services want their saves on their own NAS via WebDAV, and cheap object storage
(S3-compatible endpoints) is the most cost-effective remote for large archives.

This design generalizes the proven Drive experience into three additional
providers, in priority order:

1. **WebDAV** — self-hosted, no vendor, one HTTPS endpoint plus credentials.
2. **S3-compatible** — arbitrary endpoint + region + access/secret keys; covers
   AWS S3, MinIO, Backblaze B2, Cloudflare R2, Wasabi without per-vendor code.
3. **Dropbox** — a managed option mirroring the Drive OAuth flow.

The guiding rule is unchanged: **the remote is optional and additive**.
`SyncProvider` implementations only add revisions, manifests, and blobs. Local
backup, restore, revisions, and the DAG never depend on any of them being
reachable. Out of scope by user decision: an iCloud API client (no public API;
folder-bridging a synced iCloud directory is a separate future idea) and
OneDrive/rclone delegation.

## Goals

1. Three new `SyncProvider` implementations, each passing the same
   immutable-write and digest-verification contract as the existing two.
2. A shared provider runtime built first, so per-provider code only expresses
   what is genuinely different (URL schemes, auth, listing, upload protocol).
3. Credentials always in the OS keyring through the existing `keyring`
   machinery; endpoint/username/bucket configuration in SQLite settings like
   other non-secret options.
4. Retry with bounded exponential backoff + jitter for idempotent requests
   only, and unambiguous outcomes for both branches of an "already exists"
   case (idempotent success when content verifies, a loud integrity error
   when it does not).
5. On download, the caller's digest verification remains the last word; each
   provider adds only cheap sanity checks (content length, ETag, content hash)
   it can see for free.
6. Each provider ships with a transport-level fake (in the spirit of
   `tests/google_drive_sync.rs`) and full engine round-trip tests.

## Non-goals

- No iCloud API client, no iCloud folder bridge.
- No rclone delegation or shelling out to any external sync tool.
- No automatic conflict resolution. Sync still records DAG-head conflicts for
  the user; providers transport bytes, they do not decide anything.
- No deletion of any committed object on any remote; retention control is a
  future project. The only deletion this design ever permits is cleanup of
  *this device's own uncommitted staging objects* (WebDAV `.part-*` temps,
  abandoned non-completed S3 multipart uploads) — things that are not and
  never were referenced by any revision, manifest, or blob, and are never
  applied to anything the local database lists as an object.
- No UI beyond what wiring needs: one provider account at a time is enough
  (see "Multiple concurrent providers" under Risks).
- No changes to the sync protocol, DAG semantics, or manifest format. Providers
  treat manifests and revisions as opaque JSON and blobs as opaque archives —
  exactly what `GoogleDriveProvider` and `LocalFolderProvider` do today.

## Anti-goals

Any design that lets a remote be required for local backup or restore is
rejected. Enrolling WebDAV/S3/Dropbox changes nothing in
snapshot/restore/revision paths; a provider outage produces sync errors, never
local failures.

Any retry behavior that could duplicate or mutate an immutable object is
rejected. Concretely:

- Retries apply only to operations that are safe to repeat: GET/HEAD/PROPFIND,
  metadata listings, and upload steps that are resume-safe by construction
  (same byte range re-sent, or "already there" detected and verified).
- A retry loop may never re-issue a replace-type write against an existing
  object. If "does it exist?" and "is the content right?" are uncertain, the
  design downloads and verifies, never overwrites.
- Uploads that partially committed server-side are finished or detected in a
  way that ends with a verified object, never with a guessing rewrite.

## Constraints

From the root `AGENTS.md` (governing):

> 4. **LOCAL FAILOVER:** Remote operations, including Google Drive, must not
> become a runtime requirement. Local backup and restore must work offline.

> 3. **VERSIONED REVISIONS:** Represent history as a DAG of immutable
> revisions, not as timestamps.

> Retry transient failures with bounded exponential backoff and jitter [...]
> Do not retry permanent failures indefinitely. [...] Respect API quotas and
> rate limits. [...] Keep credentials out of logs, errors, source control, and
> user-visible diagnostic output.

> The provider interface must be mockable for tests and support an optional
> real provider. [...] Avoid unnecessary object downloads, uploads, database
> reads, and repeated manifest processing.

From `provider.rs` (the contract the new code must satisfy, verbatim):

> Implementations must be idempotent: uploading an existing revision or blob
> is a success. They must never mutate or delete existing objects.

From ADR-0001 (accepted): SQLite as the only local store; Google Drive was
chosen "per user's ecosystem" with the explicit note that "orthogonal
providers can be added later"; content hashing is SHA-256 everywhere; Drive
retry policy ("bounded exponential backoff with jitter on 5xx, 403 rate
limits, and transient errors") is the pattern to generalize.

## The `SyncProvider` trait stays as-is

**[Explicit statement] `SyncProvider` is expected to remain UNCHANGED by this
project.** The trait has six methods, and every capability these providers need
maps onto them exactly as Drive does today:

| Trait method | WebDAV | S3 | Dropbox |
|---|---|---|---|
| `list_revisions(game_id)` | `PROPFIND` depth-1 on `games/<id>/revisions/`, then `GET` each JSON | `ListObjectsV2` with prefix | `list_folder` on a prefix, then `download` each |
| `get_manifest(hash)` | `GET` one file, re-hash and compare | same | same |
| `put_revision(rev, manifest)` | create-if-absent on two JSON objects | conditional `PutObject` | session upload, commit, then duplicate-check |
| `has_blob(hash)` | `HEAD` | `HeadObject` | `get_metadata` |
| `put_blob(hash, source)` | `PUT` (temp + `MOVE` where supported) | single `PutObject` or multipart | `upload_session` start/append/finish |
| `get_blob(hash, dest)` | `GET` streamed to `dest` | same | `download` streamed to `dest` |

The streaming hooks the trait relies on already exist in the transport layer:
`RequestBody::FileRange` streams a byte range from a file (used by Drive's
resumable upload) and `HttpRequest::download_to` streams a response body
straight to disk rather than buffering it (`savesync-core/src/http.rs`). Nothing in the trait
forces buffering, so none of the three providers needs a new method.

If implementation work uncovers a genuine gap (not a convenience), the change
comes back to the orchestrator as a **marked deviation** with the concrete
call path that fails, and only then is the trait touched. Guessing at a
signature change now, before a provider exists, would break
`LocalFolderProvider` and `GoogleDriveProvider` for no benefit.

## Common infrastructure (built before any provider)

### Existing pieces that are reused, not rebuilt

- `HttpTransport` / `HttpRequest` / `HttpResponse` and
  `RequestBody::{Bytes, FileRange}` — the mock boundary (`ScriptedTransport`
  in Drive unit tests; the whole `tests/google_drive_sync.rs` fake). New
  providers get their fakes the same way.
- `ReqwestTransport` (rustls, no OpenSSL), 15 s connect / 120 s total timeout.
- `RetryPolicy` — `max_attempts: 5`, 500 ms base, 30 s cap, equal jitter,
  `Retry-After` honored by `execute_with_retry`, and the bounded assertion
  tests already in `http.rs`.
- `TokenStore` (`KeyringTokenStore` / `MemoryTokenStore`) — OS keyring with
  service `savesync`; today it stores one item,
  `google-drive-refresh-token`. New entries are added as additional accounts
  under the same service (see per-provider sections).
- Settings via `store::get_setting`/`set_setting` — endpoint URLs, usernames,
  bucket/region/prefixes, app keys: all non-secret, all in SQLite like the
  Google client id today.
- Provider-side input validation: the `validate_hash` (64 hex chars) and
  `validate_segment` (single traversal-free segment, ≤128 chars) helpers in
  `provider.rs`. Every remote path/key built by a new provider passes through
  these first; an `Error::UnsafePath` is produced before any HTTP request.
- `Error::Provider { status, message }`, `Error::Auth`, `Error::Network`,
  `Error::IntegrityMismatch` — the taxonomy these providers map into.

### New shared module `savesync-core/src/provider_runtime.rs` (name indicative)

1. **Retry wrapper for idempotent requests.** A generalization of
   `GoogleDriveProvider::execute` and `http::execute_with_retry` that takes a
   `RetryPolicy` and a per-attempt build closure. It is explicitly documented
   as "only for requests that are safe to repeat"; enforcing that is a review
   rule, not a runtime check. One behavioral extension over Drive's version:
   `is_retryable_status` currently splits 403 handling by Drive-specific body
   strings (`rateLimitExceeded`, `userRateLimitExceeded`, `backendError`).
   The runtime keeps that behavior and adds a **provider-specific 403
   predicate** so each vendor's quota signal is classified correctly
   (Drive's 403 body strings, S3's 503 `SlowDown`, Dropbox's 429 bodies)
   without weakening the rule that a 403 without a quota signature is
   permanent.
2. **Error taxonomy.** Every HTTP status is classified once, at the boundary,
   into exactly one of: transient (retryable: 408/425/429/500/502/503/504,
   plus quota-signature 403s), permanent (401/403 otherwise, 4xx usage
   errors), and network ("no response at all" — counted as an attempt, subject
   to the same cap). This mirrors the current split where `401/403 → Error::Auth`,
   everything else numeric → `Error::Provider`, and reqwest failures →
   `Error::Network`. Quota responses carry the status code into
   `Error::Provider` so the UI can show "remote asked us to slow down".
3. **Credential plumbing per provider via `TokenStore`.** The desktop layer
   already loads a `KeyringTokenStore::default()` (service `savesync`) and
   swaps in `MemoryTokenStore` when `SAVESYNC_MEMORY_TOKENS=1`. New providers
   follow that shape: unique account names, memory-store fallback unchanged,
   and — as with Drive — nothing secret is ever logged: token/error formatting
   already redacts (see `TokenSet`'s `Debug` impl) and provider error messages
   embed only server text truncated to 200 chars.
4. **Streaming and limits.** Constants, not magic numbers in provider code:
   small-object in-memory threshold, large-blob chunk size (each provider may
   override per its protocol; defaults stated below), a hard cap on listing
   pages (Drive already enforces 100 pages; S3 and Dropbox get the same bound
   with the same error), and bounded body materialization: small JSON
   objects are built in memory; blobs always stream from disk through
   `RequestBody::FileRange`, never read wholesale into RAM.
   No caching layer is proposed beyond what Drive already has (its
   `(kind, value) → file id` map, valid because objects are immutable); new
   providers may keep an equivalent only with a stated invalidation rule —
   the simplest correct choice is `HEAD`/`HeadObject` per call and no cache
   at all.

**Deviations to the trait: none anticipated.** Stated again here because this
is the file future reviewers will check against.

## Per-provider design

### Layout (identical shape across all three providers)

The remote namespace mirrors `LocalFolderProvider`'s tree so support and
inspection stay simple:

```text
<base>/blobs/<aa>/<sha256>.tar.zst
<base>/manifests/<sha256>.json
<base>/games/<game-id>/revisions/<revision-id>.json
```

`<aa>` is the first two hex chars of the digest; game ids and revisions pass
`validate_segment`; hashes pass `validate_hash`. Blob names keep the
`.tar.zst` extension as today. There is deliberately no extra index document;
remote state is recoverable from the local database by re-listing.

### WebDAV

**Layout mapping.** A user-provided base collection
(`https://example.com/saves` or any subpath). SaveSync uploads into
`<base>/blobs/…`, `<base>/manifests/…`, `<base>/games/<game-id>/revisions/…`
exactly as above. `list_revisions` uses `PROPFIND` with `Depth: 1` on the
game's revisions collection, then a plain `GET` per JSON entry and the same
on-disk checks `LocalFolderProvider` performs (id vs. filename, game_id
consistency).

**Auth model.** HTTP Basic over HTTPS. The scheme check happens before the
first request: `http://` endpoints are rejected with a config error by
default. **[MY CALL]** The default also refuses TLS cert validation errors —
i.e., self-signed or otherwise untrusted server certs are rejected, matching
reqwest's built-in behavior; it is not a change but it is a deliberate
decision to keep it. Explicit user-approved trust for a pinned certificate
would need a documented, reviewed config knob and is **deferred**, not
rejected forever.

**Upload path.** Single `PUT` with the whole file body streamed from
`RequestBody::FileRange`. There is no server-side chunk/resume protocol in
WebDAV proper, so large blobs avoid resume protocols entirely: the temp-then-
`MOVE` sequence below is the resume story (a killed upload leaves the temp
object orphaned; a fresh retry re-`PUT`s the temp and `MOVE`s once). `MKCOL` is issued for each collection level the moment it is
needed; some servers auto-create intermediate collections on `PUT`, others
return 409 until every level exists — the provider attempts `MKCOL` for each
missing level once per session and treats the 405/409 "already exists"
results as success, so the tree is never re-created per object.

**Immutability strategy.** Two mechanisms, in order of preference:

1. Where the server advertises and honors it: upload to a uniquely named temp
   object (`blobs/<aa>/<hash>.tar.zst.part-<uuid>`), then
   `MOVE` with `Destination` and `Overwrite: F`. `Overwrite: F` is required by
   RFC 4918 against an existing destination and gives true
   create-if-absent semantics. If the `MOVE` reports "destination exists", the
   provider runs the verification path from mechanism 2.
2. Independent of MOVE support: before any create, `HEAD` the final key. If it
   exists, `GET` it and verify its SHA-256 against the expected digest; equal
   content means "already uploaded" (idempotent success); unequal content is
   `Error::IntegrityMismatch` — the object is never re-written or deleted,
   exactly like the `write_atomic` refusal in `provider.rs`.

Because ordinary WebDAV servers do not reliably honor conditional `PUT`
headers, the `HEAD`-first check is the primary path, not an optimization.
That costs one request per create — same shape as Drive's `find_file` list
call today.

**Hash verification on download.** `get_manifest` re-hashes the JSON exactly
as the Drive and local providers do and fails with `IntegrityMismatch` on
mismatch. `get_blob` streams to the caller's temp path; the caller's digest
check (the contract: "the caller verifies the contents before making it
visible") is the authority. The provider adds no authoritative check of its
own beyond a nonzero-`Content-Length` sanity abort — it has no independent
expected length in the pull case — and ETags are noted but never trusted for
equality: WebDAV servers expose weak or absent ETags (noted under Risks), so
a matching ETag proves nothing and an absent one must not be an error path.

**Offline/timeout behavior.** Transport-level: 15 s connect, 120 s total, no
silent reconnects. The shared retry wrapper applies to GET/HEAD/PROPFIND and
to repeated temp-`PUT` starts (idempotent by unique-temp-name); `MOVE` itself
is retried only once, because an interrupted MOVE can leave the final object
in either of two legitimate states (exists verified-equal, or absent), and
the code resolves the actual state by `HEAD`+`GET` on the next attempt rather
than replaying the MOVE blind.

### S3-compatible (arbitrary endpoint + region)

**Layout mapping.** One bucket plus an optional key prefix. Key layout is the
tree above as flat keys (`saves/blobs/ab/<hash>.tar.zst`, …) — S3 has no real
directories, `ListObjectsV2` with prefix stands in for `PROPFIND`, and keys
are built by the same `validate_segment` chain.

**Signing.** SigV4, hand-implemented so no AWS SDK (and no async runtime)
enters the tree — `no premature complexity`: a small `s3_signing.rs` module
using the in-tree `sha2` crate plus the small RustCrypto `hmac` crate (the
one justified new dependency, no `unsafe`, no tokio). Canonical requests per
the AWS specification; credentials sourced from the configured access key,
secret, and optional session token. Payload hashing uses
`x-amz-content-sha256: UNSIGNED-PAYLOAD`, which the listed services accept
over HTTPS; this skips prehashing large streams and keeps signing cost off
the byte path. Unit tests run against the published AWS SigV4 test vectors
so the signer is provable without any network. The signer is the one piece
of new code that could silently break a whole provider, so it lands in
stage 1 (before any provider) and is tested per-vector.

**Auth model.** Access key + secret (static, self-managed keys). Both go to
the keyring (`access_key` is technically not secret but lives with the secret
to keep one item); endpoint, region, prefix stay in SQLite settings. Session
tokens are supported but optional — MinIO-style deployments are the main
consumer.

**Upload path.** Blob size drives the strategy:

- ≤ single-upload threshold (default 64 MiB, a config knob): one `PUT`
  (S3 `PutObject`), body streamed via `RequestBody::FileRange`. The
  threshold is not a protocol limit — S3's hard single-PUT ceiling is 5 GiB,
  far above any blob SaveSync produces today — it exists so an interrupted
  large upload can resume per-chunk instead of restarting from byte zero.
- > threshold: `CreateMultipartUpload`, one `UploadPart` per 8 MiB chunk
  (the same chunk size Drive uses, so no per-provider tuning creep),
  `CompleteMultipartUpload` sent as its own request only after the final
  part succeeds. On any failure the in-progress multipart upload is aborted
  server-side — that discards only uploaded parts of an object that was
  never visible, never a committed object — and the operation stays failed
  until the caller retries it.

Manifest/revision uploads are always single-shot `PutObject` (they are
kilobytes).

**Immutability strategy.** Layered, honest about who supports what:

- **[VERIFIED from vendor docs]** AWS S3 and Cloudflare R2 honor
  `If-None-Match: *` on `PutObject` ("create-once" semantics; R2 explicitly
  documents that racing creates resolve to at most one success with all
  others failing `412 PreconditionFailed`).
- A `412` on a conditional create means "key already existed": that counts as
  success only after the existing object is downloaded and verified to equal
  the expected digest. A 412-with-content-mismatch is an integrity error,
  never a rewrite trigger.
- **B2 and other non-honoring services [VERIFY — evidence is community-level,
  not vendor docs]** Backblaze B2's S3 API appears not to honor
  `If-None-Match` on `PutObject` (repeated user reports; no vendor doc found
  in this pass). For services that skip it, the provider falls back to the
  same `HeadObject`-first dance as WebDAV: pre-check, plain create, post-create
  verification. Two racing writers committing *different* bytes to the same
  key (which for content-addressed keys means a SHA-256 collision — a global
  disaster independent of this design) is caught by the post-verify check and
  reported as `IntegrityMismatch`; racing writers committing identical bytes —
  the normal case for the same content-addressed blob — is harmless. (That
  fallback is necessary because S3's default semantics replace the object;
  a plain create without the pre-check would *not* be safe.)
- The multipart path uses `If-None-Match: *` on `CompleteMultipartUpload`
  **[VERIFY per-endpoint — AWS documents it, R2 evidence is unclear]**;
  where unsupported, the multipart path uses HeadObject-first as above.

This table gets a test harness where each vendored behavior is a fixture, so
the fallback chain is proven rather than assumed.

**Hash verification on download.** Same as WebDAV: `get_manifest` re-hashes;
`get_blob` streams to the caller's temp path with a nonzero-`Content-Length`
sanity abort; the caller's digest check is the binding one. `ETag` is treated
as an opaque, untrusted string — S3 ETags are not content digests for
multipart objects, so equality claims based on them prove nothing.

**Trusted endpoint hard rule.** User-typed `endpoint` values must be HTTPS in
the default configuration, same rule as WebDAV. Plain HTTP only matters for
MinIO-style LAN servers with no certificate at all; **[MY CALL]** SaveSync
neither enables it silently nor ships the knob in this project — unencrypted
credential transport stays behind an explicitly reviewed config option,
same decision as the WebDAV self-signed-cert case.

**Offline/timeout behavior.** Same shape as WebDAV: the shared backoff for
idempotent requests, plus `UploadPart` retries that are safe by construction
because the same sliced byte range is re-read from disk (`FileRange`) and
re-sent. `CompleteMultipartUpload` is never blindly replayed: a replayed
finish against an already-completed upload Braves `NoSuchUpload`, which is
surfaced rather than restated as success. A created-but-never-completed
multipart upload lingers server-side; **[USER DECISION]** whether an explicit
user-invoked "clean abandoned uploads" sweep is required for stage 3 or an
accepted deferred one-off.

### Dropbox

**Layout mapping.** Uses a Dropbox app with **app-folder access** (the
Dropbox equivalent of `drive.file`: the app token never sees paths created
by other apps). Paths under the app folder use the same tree exactly,
including the `.tar.zst` blob names. No `MOVE`-like operation is needed — a
session commit is atomic at `finish`.

**[USER DECISION] The user must register a Dropbox app** and paste the app
key SaveSync asks for. There is no unset default; until the user provides one,
the Dropbox option is simply not offered in the UI (the same gate the
Google client id has today: `sync_game` refuses with "Save a Google OAuth
client id first"). At connection time SaveSync requests only
`files.content.read/write` scopes.

**Auth model.** OAuth 2 + PKCE with a localhost loopback redirect, mirroring
the Drive flow as closely as the code allows, because `OAuthConfig` already
carries `client_id`, optional `client_secret`, auth/token endpoints, and
scopes — so an `OAuthConfig::dropbox(app_key)` constructor fits the existing
shape:

- Endpoints: `https://www.dropbox.com/oauth2/authorize` and
  `https://www.dropbox.com/oauth2/token`; with
  `token_access_type=offline` the token exchange returns a refresh token.
- The only endpoint-behavior difference: `oauth::authorization_url` currently
  appends Google-specific consent params (`access_type=offline`,
  `prompt=consent`, `include_granted_scopes`). Dropbox ignores unknown query
  params, so the flow can go through unchanged; if a cleaner Dropbox-specific
  param set proves necessary, a small extension to that one function —
  leaving Drive's output byte-identical — is proposed as a **marked
  deviation** with justification, per the trait-change rule above.
- The `oauth_loopback.rs` listener machinery (ephemeral `127.0.0.1` port,
  `/callback`, constant-time `state` compare, 5-minute deadline, HTML
  completion page) is reusable as-is. The success page text currently says
  "Google Drive is connected"; parametrizing that message is a text change,
  not a mechanism change.
- The refresh token is stored in the OS keyring under a Dropbox-specific
  account name, exactly the way the Drive refresh token is.

**Upload path.** API v2 chunked upload on
`https://content.dropboxapi.com`:

1. `/files/upload_session/start` with the first chunk (default 8 MiB,
   `RequestBody::FileRange`, matching the Drive chunk constant).
2. `/files/upload_session/append_v2` with `(session_id, offset)` for each
   subsequent chunk.
3. `/files/upload_session/finish` with the final cursor; the returned
   metadata confirms the committed path and (optionally) carries the
   `content_hash` described below.

Retries: every step is retried through the shared wrapper because each is
either read-only or idempotent-by-offset — `append_v2` writes at the cursor's
offset, and when the server-side session position differs it replies with the
documented `IncorrectOffset` error carrying `correct_offset`, which the
provider adopts once and continues from (server-side offset truth wins over
client bookkeeping). A session that never reaches `finish` leaves orphaned
upload-session state on Dropbox; **[VERIFY]** whether it expires on its own
(the documentation is evasive) and whether an explicit user-invoked "clean
abandoned sessions" sweep is worth building is recorded as an open
follow-up, not silently dropped.

**Immutability strategy.** Dropbox has no conditional-write primitive, but it
does have the next best thing, used deliberately: `upload_session/finish`
modes are `overwrite` (the API default — which SaveSync must *never* use, as
it replaces content), `add`, and `update`. SaveSync always passes
`mode: add`, which makes semantic overwrite impossible: when the target path
already exists, the server writes an auto-renamed sibling (for example
`save.tar (1).zst`) instead of replacing anything. On top of that:

1. Before any create, `get_metadata(path)` runs; when the path already
   exists, download and verify against the expected digest — equal means
   "already uploaded" (idempotent success), different is
   `Error::IntegrityMismatch`, and neither is ever a rewrite trigger.
2. Content addressing makes the rare raced double-commit harmless: both
   files carry identical bytes, the second lands under the server-chosen
   rename, and downloads resolve through `get_metadata` on the canonical
   layout name — ambiguity is resolved by verification, never by deleting
   either copy the contract forbids deleting.
3. Dropbox dedupe support also surfaces `content_hash` (SHA-256 over
   concatenated 4 MiB block digests) in `get_metadata` and finish responses.
   It is a different scheme from SaveSync's whole-file SHA-256, so it is a
   server-side dedupe/staleness signal only — recorded, never substituted
   for SaveSync's verified digest.

**Hash verification on download.** Same principle as the other providers:
re-hash the manifest JSON (comparing against the requested digest exactly as
Drive does), stream the blob to the caller's temp path (the caller's digest
check is the authority), and use `Content-Length` sanity aborts only. The
Dropbox `content_hash` is never, by itself, accepted as a verification basis
for a SaveSync object.

## Staged implementation plan

Each stage is independently verifiable and lands with its tests green. The
whole project runs under the standard verification commands (`cargo fmt --all`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`) and keeps the two
existing provider integrations (`tests/google_drive_sync.rs`) untouched.

### Stage 1 — shared runtime + classification

Deliver `provider_runtime.rs` (retry wrapper, error classification,
453-code predicates, streaming-limit constants) with no provider on top.

Proof tests:

- backoff bounds (already exist in `http.rs`; runtime wrapper's own delay math
  inherits them) plus a wrapper-level test that a permanent 4xx classifies as
  non-retried while a 429 retries and yet both count bounded attempts;
- quota-signature 403 handling across a Drive body, an S3 `SlowDown`-style
  body, and a `rate_limit`-eager Dropbox fat;
- SigV4 signer vectors (Stage 2 tests land here because stage 1 is the only
  runtime change in scope);
- category-of-failure table tests: given 401/403/404/408/429/5xx/network
  error, which retry/permission outcome occurs per provider flavor (no code
  path assumes).

### Stage 2 — WebDAV

Deliver `webdav.rs` backed by a `FakeWebdavServer` transport implementing
`PROPFIND`/`GET`/`PUT`/`MKCOL`/`MOVE`/`HEAD` in-memory, plus full
`SaveSyncEngine::sync()` round trip tests (mirror
`tests/google_drive_sync.rs`).

Proof tests include failure/injection, in the spirit of `phase1_safety`:

- round-trip: local-only revision reaches the provider; pull is byte-equal
  back; staged revision reflects both `put_revision` and `put_blob` paths;
- idempotent re-upload: second `put_blob` of the same digest is a success and
  does **not** change the stored object (content immutability asserted by
  byte comparison, not just status);
- pre-existing different-content object: `put_*` reports integrity error and
  the object is unchanged;
- `MOVE` supporting server (Overwrite: F honored), server without `MOVE`
  (temp-then-race path only), missing-`MKCOL` parent auto creation, weak/no
  ETag notifications — one fixture each;
- `PROPFIND` pagination behavior (or absence) against the list cap;
- `http://` endpoint refused; self-signed cert refused; a scripted TLS
  failure in the transport wrapper does not become a silent success;
- retry cap hit mid-`PROPFIND` does not leave the provider claiming data
  exists where the server disagrees; reader/existence contrast tests.

Mature round trip after stage review.

### Stage 3 — S3

Deliver `s3.rs` using the stage 1 signer, with a fake transport implementing
enough SigV4 checkplus `PutObject` / `CreateMultipartUpload` /
`UploadPart` / `CompleteMultipartUpload` / `HeadObject` / `GetObject` /
`ListObjectsV2` to exercise the code paths.

Proof tests:

- SigV4 vector tests (signed headers, payload hash modes, presign-free
  headers, getoses-style body content match);
- `If-None-Match: *` honored: concurrent create attempts land exactly one
  success; 412-with-different-content errors loudly, never overwrites;
- fallback version (conditional unsupported): pre-check/post-verify path
  produces the *same* public outcomes as above without the header, and the
  verification contract is what caught a mismatch;
- endpoint ordering: bucket-with-prefix `ListObjectsV2` paginated listing
  produces the same `Vec<Revision>` a local-folder provider would give on
  the same content, in any order;
- retry injected mid-`UploadPart` ends with either still-failed or
  completed-correct object — never a partial final object silently accepted;
- binary left half-finished on a server side: `download` stream error does
  not leave the local staged blob in place (temp file cleanup, like the
  atomic-writer code already in the tree).

### Stage 4 — Dropbox

Deliver `dropbox.rs` plus the small OAuth extension, with a fake
`DropboxTransport` handling token refresh (mirror `StaticToken` for tests),
`upload_session` `start`/`append_v2`/`finish` including the
`IncorrectOffset`-style error body; a loopback-flow fixture that doesn't
actually open the browser (the tests call the exchange/`finish` internals
directly the way `oauth_loopback` tests do).

Proof tests:

- chunked upload across several chunks with a network fault injected in the
  middle resumes from the correct offset; the `IncorrectOffset` path adopts
  the server-reported offset in one replay, not a loop of replay attempts;
- dedupe/evidence: session finishes on top of an existing object (real
  Dropbox behavior), and the content check wins over the stale metadata —
  the duplicate copy is recorded as matching-or-not without ever deleting
  either copy;
- `OAuth` flow: state mismatch, registration-not-yet-done gate behaves like
  the Google path (user-visible error, no partial keyring write), refresh
  token lands only in the provided `TokenStore` (memory store under test),
  and no code or token appears in any formatted error;
- one round-trip `SaveSyncEngine::sync()` like the other two providers.

### What "done" looks like per stage

For each provider, the acceptance command set is:

```text
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test            # includes existing phase1_safety, phase2_sync, drive
```

plus a manual `npm run build` and `cargo clippy` for the desktop (thin
wiring: settings command per provider, connect/disconnect, choose-provider
sweep in the existing cloud UI card).

## Risks and open questions

Each marked with ownership:

1. **[VERIFIED — resource note] Full risk —** none in this document claim a
   *measured* performance result. The shared `HEAD`/`HeadObject`/`metadata`
   pre-check adds one request per create. Against Drive's current
   `find_file` list call that is a wash or better; no claims beyond that are
   made without benchmarking. Per `AGENTS.md` ("MEASURE BEFORE CLAIMING",
   no invented results): if a later benchmark disagrees, fix before landing
   stage 3/4.
2. **[USER DECISION] Backblaze B2 honor of `If-None-Match: *`.** Evidence in
   this pass is community-level (reports the header is ignored). The design
   covers both worldstates (header honored → atomic create-once; header
   unsupported → HeadObject-first + create + post-verify), so either way the
   immutability contract holds; what needs verification is which path
   actually runs, and that per-endpoint config must be probed once, not
   guessed. Must be verified against a real B2 bucket before that endpoint
   class is trusted in production.
3. **[USER DECISION] Dropbox app registration.** Requires the user to
   register an app and paste an app key; no registry or bundling of keys.
   Until then, Dropbox is simply not offered.
4. **[USER DECISION] Multiple concurrent providers.** The design assumes one
   active cloud provider at a time (Drive today, one of the three new ones
   later), matching the single `google.client_id` +
   `google-drive-refresh-token` shape. Multi-remote fan-out (same revision to
   two providers) is a real feature but is a different sync engine decision,
   not a provider integration; out of scope here.
5. **[MY CALL] HTTPS-only and cert-strict by default, stated above.**
   Self-signed-cert trust via an explicit user decision is deferred, not
   forbidden; anyone holding a domain (including a LAN host with a real
   hostname) can use WebDAV/S3 with renew-cert TLS today. Flagged as a call
   so the orchestrator can override.
6. **[VERIFY] Dropbox upload-session expiry and post-restart resume.** The
   session-cursor resume semantics are documented well enough to code the
   happy path, but whether a session that outlives the process (app closed
   mid-upload) can be finished days later — or must be abandoned — needs
   either a doc read or a live probe. Either way a failed finish never stalls
   sync; it retries from `start`.
7. **[VERIFY] R2 conditional `CompleteMultipartUpload`.** AWS supports it;
   R2's extension docs are ambiguous in this pass. Per-endpoint capability
   detection (a one-shot `412` probe or vendor-documented support flag)
   decides at runtime, never guessed.
8. **[MY CALL] Wasabi/MinIO nuance coverage.** Same shared code handles
   arbitrary endpoints, but the design only *proves* AWS+S3 vectors and
   R2 docs; other vendors enter on their own agreement that conditional-PUT
   semantics are respected, and the S3 stage's fallback chain exists exactly
   so unproven vendors degrade to the safe path rather than to an overwrite
   hazard.
9. **[USER DECISION] Retention.** Nothing here adds version retention on any
   remote; remote growth is bounded only by what users push. Data-retention
   control on the remote (a future project) needs its own disk/feature; not a
   gap in this scope but flagged so nobody assumes it existed.

## Delivery order summary

1. Stage 1: runtime + SigV4 signer + classification tests (no behavior
   change; all existing providers keep passing).
2. Stage 2: WebDAV provider behind the same `SyncProvider`, with fake
   transport and engine round-trip; only then UI wiring.
3. Stage 3: S3 provider behind the same pattern; capability-detection table
   for conditional writes.
4. Stage 4: Dropbox provider + OAuth extension; user-decision gate for the
   app key.
5. Documentation: ADR noting the added providers and ADR-0001's
   providers-after-the-fact sentence becomes true.

No application code is written under this design document itself; it is a
proposal for the orchestrator to slice into implementation tasks.
