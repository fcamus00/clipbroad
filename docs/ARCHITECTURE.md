# clipbroad: architecture

Share the clipboard across the owner's machines (Linux/Hyprland, macOS, Windows).
Inspired by [ClipCascade](https://github.com/Sathvik-Rao/ClipCascade), but in Rust and without a server.

This document records the **decisions** and their reasons. If a decision changes, it is updated here in the
same PR that changes it.

## Scope

- **Platforms:** Linux (Wayland, Hyprland as the main target), macOS and Windows.
- **Network:** same LAN; optionally over Tailscale. The open internet without Tailscale is not considered.
- **Content:** MVP text only. Then images, and then files.
- **Out of scope (indefinitely):** mobile (Android/iOS), sync with third parties, user accounts.

## Architecture

**Full-mesh P2P, without a server.** The same `clipbroad` binary runs as a daemon on each machine and
connects directly to each paired peer.

Why not a central server (like ClipCascade):
- One hop instead of two (faster, especially for images and files).
- No single point of failure: if a machine is off, the others keep syncing.
- A single artifact to build and deploy.

### Transport: iroh

[`iroh`](https://github.com/n0-computer/iroh) 1.x (QUIC). What it provides:
- Identity by Ed25519 public key (`EndpointId`); the `SecretKey` is persisted on disk so the
  identity is stable.
- Peer encryption and authentication (TLS over QUIC); we do not write our own cryptography.
- Direct connections over the LAN or over Tailscale.

Chosen configuration:
- **`presets::Minimal` preset**: no n0 relays and no DNS/pkarr discovery. No traffic goes out to
  third-party services.
- **LAN discovery** with mDNS (see "LAN discovery (mDNS)" below).
- **Tailscale**: mDNS does not cross the tailnet, so each peer can also have static addresses in the
  configuration (`EndpointAddr::new(id).with_ip_addr(...)`), for example `100.x.y.z:port` or the
  resolved MagicDNS hostname. Fixed, configurable listen port.
- **Allowlist**: only connections from paired `EndpointId`s are accepted, using `EndpointHooks`
  (see iroh's `auth-hook` example). Everything else is rejected.
- **iroh features**: `tls-ring` and `fast-apple-datapath`, without the default ones. No `portmapper`
  (UPnP is not needed on LAN/Tailscale and brings an MPL-2.0 dependency) and no `metrics`. `cargo deny` only
  checks the targets we publish (so iroh's wasm-only crates do not count) and allows
  CDLA-Permissive-2.0 only for `webpki-roots`. Advisory RUSTSEC-2024-0436 (`paste` is
  unmaintained, not a vulnerability) is ignored because iroh needs it through `netwatch`.

### LAN discovery (mDNS)

Each node announces its `EndpointId` and its addresses over mDNS, and listens to the others' announcements.
That way a paired peer is found even if its LAN IP has changed (DHCP), without touching `addrs`.
Static addresses are still useful for what mDNS does not reach, such as Tailscale.

- **Crate: `iroh-mdns-address-lookup`** (from n0, the authors of iroh). We discarded
  `iroh-mdns-peer-lookup`: its license says "TBD" (it does not pass `cargo deny`) and it requires iroh with the default
  features, which bring relays and `portmapper`. The chosen one uses the same iroh version as the rest
  (there are not two iroh in `Cargo.lock`), is MIT or Apache-2.0 and adds no new advisories or licenses.
- **Integration:** it is registered as the endpoint's *address lookup* (the `Minimal` preset has
  none). When dialing a peer, iroh asks mDNS for the peer's current addresses and
  tries them along with the static ones. Because of that, with mDNS on, the node also dials peers without `addrs`.
- **Custom service name:** `clipbroad` (records `<id>._clipbroad._udp.local`) instead of the
  default `irohv1`, so it does not mix with other iroh apps on the same network. All nodes
  must use the same one.
- **It only finds addresses, it never grants access.** The allowlist does not change: a node that announces itself on
  the LAN and is not in the config is still rejected after TLS.
- **Privacy:** the announcement is multicast on the local network, so anyone on the same LAN sees this node's public
  id, its addresses and that it runs clipbroad. The id is not secret (it cannot be used to
  connect without being paired), but it lets someone recognize the machine on that network. No
  third-party service is used: the traffic does not leave the LAN.
- **Config:** `mdns = true` by default. With `mdns = false` no multicast socket is opened and nothing is
  sent, and only peers with `addrs` are dialed.
- **If it cannot start** (for example, no interface allows multicast), the daemon logs a
  warning and continues with the static addresses only; it does not try again until it restarts.
- **Tests:** the test of two nodes that find each other without addresses needs multicast between local
  sockets, which CI runners may not allow, so it is marked `#[ignore]` and is run by hand
  (`cargo nextest run -p clipbroad-net --run-ignored only`).

### Connections (`net` crate)

- **One connection per peer**, long-lived. Each node dials all the peers it can find
  (with mDNS on, all of them; otherwise, the ones that have addresses in the config) and accepts the connections that
  arrive from them. Without mDNS, a peer without addresses can only connect if it is the one that dials.
- **Streams:** each side opens a unidirectional stream and writes frames: first `Hello`, then one
  frame per clip. It reads the other side's stream the same way. Each bidirectional stream is the transfer of
  one file (see "File transport").
- **Duplicate connections:** if both dial at the same time, there are two connections. Only the node with the
  lower id decides: it keeps the one that finished the `Hello` exchange most recently and closes the
  other. The node with the higher id closes nothing. That way both converge without agreeing on anything, and "the newest
  wins" also covers a peer that restarted while its old connection seemed alive.
- **Reconnection:** with exponential backoff between 0.5 s and 30 s per peer. It goes back to the minimum after a
  session that got to exchange `Hello`; a handshake that fails (for example, a different version)
  keeps increasing the delay. `Hello` has a 10 s limit.
- **No forwarding:** the mesh is full, so a clip must come from the machine that copied it. If
  `Clip.origin` is not the peer that sends it, it is discarded.
- **API:** `Node::spawn(identity, config)` returns the node and a channel of received clips (with the id
  of the peer). `Node::broadcast(clip)` sends it to the connected peers; those that are not connected
  miss it. `Node::broadcast_files(clip, offer)` does the same with a files clip and from that
  moment serves those files; `Node::fetch_files(peer, list, cache)` downloads them. `Node::links()` is a `watch` channel with the status of each peer (connected or not, and the
  address of the path that the newest connection uses when connecting), for `clipbroad status`.

### Pairing

The user runs `clipbroad pair` on one machine (A), which shows a ticket and waits, and
`clipbroad pair <ticket>` on the other (B). Each adds the other to its configuration and exits.
Code in `crates/net/src/pair.rs` and `crates/daemon/src/pair.rs`.

- **Ticket:** one base32 line with a version byte, A's `EndpointId`, its addresses (on
  its listen port) and a random 16-byte secret.
- **Flow:** B dials A with a separate ALPN (`clipbroad-pair/1`), outside the allowlist, and sends it the
  secret, its hostname and its addresses. A compares the secret in constant time; if it matches, it saves
  B and only then replies with its hostname, and B saves A. That way B only saves A if A already
  saved B.
- **Port:** pairing listens on the normal `listen_port` on both machines, so the
  addresses exchanged (and the address B connects from, which A also saves) are
  the ones `clipbroad run` will use. Because of that, `clipbroad run` must be stopped while pairing; if it is
  running, `pair` fails with a message that says so. It costs nothing: the daemon reads the config only at
  startup, so it has to be restarted anyway to see the new peer.
- **Config:** a `[[peers]]` block is appended as text at the end of the file (comments are
  kept), with `id`, `name` and `addrs`. The file is created if it does not exist, the result is validated
  before writing, and a peer that was already there is neither duplicated nor modified.
- **Threats:** iroh's TLS already authenticates both `EndpointId`s; the secret only proves that B saw the
  ticket on A's screen. A accepts a single valid request and stops listening, or stops listening after
  5 minutes: a ticket leaked after it was used or expired is useless, because nobody answers anymore. A
  wrong secret only closes that connection and the session keeps waiting (guessing 128 bits in 5
  minutes is not feasible), so a foreign attempt does not ruin the user's pairing. If someone
  uses a leaked ticket *before* B, they get paired with A; the user notices because their
  `clipbroad pair <ticket>` fails, and must remove that peer from the config.
- **Limit:** the saved addresses are the ones from that moment. If a machine's LAN IP changes,
  mDNS finds the new one; the saved ones just become outdated. With `mdns = false` you have to edit
  `addrs` by hand (or pair again after deleting the peer).

### Protocol

- A custom ALPN (`clipbroad/1`). Messages serialized with `serde` + `postcard`, framed with a
  length prefix.
- Main message `Clip`: id, origin (`EndpointId`), timestamp, content type, MIME, size,
  BLAKE3 hash and payload.
- The payload goes **inline** if it is small (configurable threshold; text and typical images). For large
  files the metadata is announced and the content is transferred separately (see "Files").
- `PROTOCOL_VERSION` is exchanged on connect; incompatible versions are rejected with a clear error.
- The protocol is designed from the MVP with the type/MIME/size/hash fields, so adding images and
  files does not break it.

Details (code in `crates/core/src/protocol.rs`):
- **Frame:** 4 bytes with the length (big-endian u32) and then the message in `postcard`. `core` only
  encodes and decodes bytes (`encode`, `decode`); `net` does the reading and writing on the QUIC streams.
  `decode` distinguishes "missing bytes" from an error.
- **Maximum frame size: 16 MiB**, a protocol constant (not configurable, because both sides must
  agree). The receiver checks it as soon as the 4 length bytes arrive, before allocating memory; the
  sender does not encode a larger frame. Because of that, the config rejects an `inline_max_bytes` greater than
  16 MiB minus 64 KiB (margin for the clip metadata), and the daemon does not sync copied text
  larger than `inline_max_bytes`: it skips it with a warning.
- **Messages:** `Hello { protocol_version }`, `Clip`, `FileRequest` and `FileResponse`. Each side sends
  `Hello` first; `decode` returns a typed error if the version does not match, and the connection is
  closed. The last two only travel on the bidirectional file streams; on the
  clip stream they are a protocol error.
- **`Hello` is stable across versions:** it is always variant 0 and `protocol_version` is always its
  first field; fields can only be added at the end. `decode` ignores leftover bytes inside a
  frame, so an old node can read the version of a new `Hello` and reject it with a clear error.
- **`Clip`:** `id` (a number the origin chooses), `origin` (the 32 bytes of the `EndpointId`, opaque to
  `core`), `timestamp_ms` (Unix milliseconds by the origin's clock, moved forward if needed to land
  after the latest one it saw; see "Sync semantics"), `kind`, `mime`, `size`, `hash` and
  `payload`. The payload is inline (text and images) or a list of files (see "File
  transport").
- **Verification on receipt:** the content can only be obtained with `Clip::into_content`, which checks that the
  declared size and BLAKE3 hash match the payload, that text is valid UTF-8, that an
  image starts as PNG, that a file list is valid and that the payload matches the type.
- The `Debug` of a `Clip` shows the payload length, never its content, so it does not leak into the logs.

### Sync semantics

The logic lives in `crates/core/src/sync.rs` (`SyncEngine`), with no IO and no clock: the daemon passes it the time.
On a local event, the daemon reads the text and calls `local_changed`; if it returns `Send(clip)`, the daemon sends it
to all peers. On a remote clip, it calls `remote_clip`; if it returns `Apply(content)`, the daemon writes it
to the local clipboard.

- **Last writer wins.** Each new local copy is sent to all peers; on receipt, it is written
  to the local clipboard if it is newer than the latest one seen. Clips are ordered by
  `(timestamp_ms, origin)`: the origin `NodeId` breaks ties, so all machines pick the same one.
- **Clock:** machine clocks differ. With the clock timestamp as is, a machine
  10 s behind would lose against a copy made 3 s earlier on another machine, even on itself. Because of that
  a local copy carries `max(now, newest seen + 1 ms)`: it always beats everything its machine
  has already seen, regardless of the clocks. For a person who copies on one machine at a time, their
  latest copy always wins.
- **Anti-echo:** when remote content is written to the local clipboard, the OS fires a change event.
  The hash of the last content sent or applied is stored, and the local event that matches it is ignored.
  Without this, two machines resend the same content to each other forever. It is needed from the first
  sync (phase 2).
- **Same text copied twice in a row:** it is not resent, because the hash matches the last one sent.
  The peers already have it: if any of them had copied something later, that clip would have arrived and changed the
  stored hash. Because of that, if A copies X, B copies Y and A copies X again, X is sent again and wins on
  all of them.
- **Bounded state:** only the last hash and the last `(timestamp_ms, origin)` mark, with no history and no
  persistence.
- **Known limits:**
  - Two copies on different machines, so close together that neither saw the other, are concurrent: the one with the
    higher mark wins (the machine whose clock is ahead). They all end up with the same content.
  - A clock that is ahead drags everyone's marks forward; ordering still works, but
    `timestamp_ms` stops being the exact time.
  - A freshly started daemon has seen nothing. If a peer's clock is ahead, its first copy can
    lose against that peer's last clip, at most for the duration of the clock difference.
  - There is no catch-up on reconnect: a peer that missed a clip keeps the previous content
    until the next copy, and copying the same text again does not resend it.
- **Sensitive content: never synced** (see the next section).

### Daemon (`clipbroad run`)

A single loop joins the pieces (`crates/daemon/src/run.rs`), with the same flow the `sync`
module describes: a local change with formats is read (off the async thread), and if it is new content it is
sent to everyone (see "Files" and "Images" for which one wins if a copy carries several); a received clip
is written to the clipboard if the engine returns `Apply` (files, after downloading them;
see "Files on the clipboard"). An error while reading, writing or processing a clip is logged and
the daemon continues; only a failure at startup or in the clipboard watcher stops it. It shuts down with
Ctrl+C or SIGTERM.

- The logs show the node id at startup, the connections and each clip sent or received with its
  size and the first 8 characters of the hash, never the content. `run` logs at info level by
  default.
- `--config <file>` (on `run` and `pair`) uses another config file, and `secret.key` in the same
  folder as the identity. It is useful to run two instances on one machine (testing).
- Content marked as sensitive is not sent: the daemon skips it with a debug-level log that
  shows the marker, never the content.

### Daemon status (`clipbroad status`)

The daemon writes its status to a TOML file, `status.toml`, in the same folder as `secret.key`
(code in `crates/daemon/src/status.rs`). `clipbroad status` reads it and shows it.

- **A file and not a socket or named pipe:** the same code works on all three OSes, the daemon does not have to
  serve anyone, and a dead daemon shows because the file stops being updated. A socket would give
  the status instantly, but it needs a server inside the daemon and different code on Windows.
- **When it is written:** after each event (a peer connects or leaves, a clip sent or
  received) and every 5 s. It is written to a temporary file and renamed, so a reader never sees a
  half-written file. When it stops, the daemon deletes it. It is only written once the node is up:
  a second daemon with the same config fails earlier (the port is taken) and does not overwrite the file.
- **Alive or not:** with no file, the daemon is not running. If the file is more than 15 s old, the daemon
  died without deleting it (`kill -9`, a crash, a power outage) and `status` shows it as stopped. The
  PID is not checked: that would need per-OS code and the age is enough.
- **Location:** next to the identity, so two instances with their own `--config` (and therefore
  their own `secret.key`) never share the file. `status --config <file>` reads the one for that
  instance.
- **What it shows:** whether the daemon is running (and its PID), its `EndpointId`, the port, whether mDNS is on,
  each peer with its name, short id and whether it is connected (and through which address), and the last clip
  sent and received with time, size and short hash. Never the content. On macOS 15.4 or newer,
  also the pasteboard `accessBehavior` as the daemon sees it, with instructions if it is not
  set to "Allow".

## Clipboard per platform

| Platform | Detect changes | Read/write |
|---|---|---|
| Linux Wayland | `wl-clipboard-rs::watch` (`ext-data-control` or `wlr-data-control`; Hyprland supports both) | `wl-clipboard-rs` |
| Linux X11 | XFixes (through `clipboard-master` or `x11-clipboard`) | `arboard` |
| macOS | Polling of `NSPasteboard.changeCount` (there is no notification API) | `arboard` / `objc2` |
| Windows | `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` in a message-only window | `windows-sys` directly (`arboard` does not expose the format list) |

Everything sits behind a `Clipboard` trait in the `clipboard` crate, with an in-memory fake implementation
for tests. X11 has low priority (the target is Hyprland).

Trait contract (full detail in the doc comments of `crates/clipboard`):
- `read_text` / `write_text` are synchronous and can block a little; the daemon calls them with
  `spawn_blocking`. An empty clipboard or one without text is `Ok(None)`, not an error.
- `read_image` / `write_image` follow the same rules with PNG images (see "Images"). They are
  optional: by default `read_image` returns `Ok(None)` and `write_image` returns
  `ClipboardError::Unsupported`, so a backend without images still compiles.
- `read_files` / `write_files` follow the same rules with local file paths (see "Files
  on the clipboard"). They are also optional, with the same defaults: today only Wayland
  implements them.
- No read or write blocks for more than 2 seconds: if the app that owns the content does not respond, the
  backend returns an error. The backend sets the limit because the daemon cannot cancel a blocked thread.
- `watch` returns a **channel** (not a callback): the backend pushes events from its own OS thread
  (Wayland, Windows) or from a polling thread (macOS), with a non-blocking `send`. The channel is
  unbounded because the events are small and arrive at human speed.
- An event does not carry the content, only the **offered formats** with their native name (MIME on Linux,
  UTI on macOS, format name on Windows).
- `read_text`, `read_image` and `read_files` never read sensitive content: they check the markers on the same
  content they read and return `ClipboardError::Sensitive` without reading the data (see "Sensitive
  content").
- **All** changes are reported, including the ones the daemon itself causes when writing; anti-echo
  is done in the daemon by hash, the same on all platforms.
- `ContentHash` is BLAKE3 of the content bytes (text: UTF-8 as is, not normalized; image:
  the PNG bytes).

### Sensitive content

These markers are detected and respected on **all** platforms:

- **Windows:** format `ExcludeClipboardContentFromMonitorProcessing` (present = do not sync),
  `CanUploadToCloudClipboard` = 0 and `CanIncludeInClipboardHistory` = 0.
- **Linux:** MIME `x-kde-passwordManagerHint` (value `secret`). KeePassXC uses it, among others.
- **macOS:** `org.nspasteboard.ConcealedType`, `org.nspasteboard.TransientType`,
  `org.nspasteboard.AutoGeneratedType` (nspasteboard.org standard; 1Password uses it) and the legacy
  `com.agilebits.onepassword`.

How it is applied (code in `crates/core/src/sensitive.rs` and in each backend):

- **A single list in `core`.** `sensitive_marker(formats)` compares exact names (case
  included). The two Windows flags store a DWORD and only mark if they are 0, so the Windows backend
  reads the value and passes them as `CanUploadToCloudClipboard=0`.
- **Linux by presence.** `x-kde-passwordManagerHint` marks even if its value is not read: KeePassXC
  sets `secret` and no app offers it to say "this is not secret". Reading the value would be one more
  transfer from the owning app.
- **The filter is in `read_text`, not in the event.** Another copy can arrive between the event and the read:
  if the event's formats were checked, a secret copied right after would be read with the
  formats of the previous content. Because of that, each backend lists the formats and reads the text from the same
  content, and if it finds a marker it returns `ClipboardError::Sensitive` without reading the text. Making it
  an error means that whoever forgets to handle it still ends up without the text. `read_image` and
  `read_files` do the same with the image and the file list.
  - **Wayland:** asks the compositor for the current offer, which carries its fixed MIME list, and reads the text
    from that same offer. A new copy is another offer, so the list and the text always belong to the
    same content.
  - **Windows:** with the clipboard open (`OpenClipboard`) nobody else can change it. It lists the
    formats, reads the flag DWORDs and the text before closing it.
  - **macOS:** there is no way to do it in one step. It reads `changeCount`, checks the types, reads the text,
    checks the types again and compares `changeCount`. If it changed, it starts over (up to 3 times) and
    then returns an error.
- **Fail closed.** If the formats cannot be listed or a flag value cannot be read, `read_text`
  returns an error and the daemon sends nothing.
- `clipbroad watch` shows the event's formats and, if the content is sensitive, the marker instead
  of the text. It is useful to check what each password manager marks.

Remaining risks:

- **Apps that do not mark.** Whatever a password manager copies without any marker (or copying a
  password by hand from another app) gets synced. This also happens with a manager that uses a marker
  that is not on the list.
- **macOS, late marker.** Writing types one by one does not change `changeCount`. If an app writes
  the text and adds the marker after the daemon checked the types the second time, the text
  is sent. The second check shrinks that window to the time it takes to read the text. Apps that follow
  nspasteboard.org declare all types before writing data, and do not have this problem.
- **Windows, events without a value.** The event shows `CanUploadToCloudClipboard` without its value: reading it
  on the watcher thread could block it while waiting for the app that copied. The value is only read in
  `read_text`, which has a time limit, and `watch` shows it in the marker (`=0`).
- **Real behavior on macOS and Windows** with 1Password, KeePassXC and Bitwarden: only tested by
  hand (the CI tests write the markers themselves).

### macOS: pasteboard privacy (main risk)

Since macOS 15.4, Apple has a prompt ready for when an app **reads** the pasteboard
programmatically. As of today it is not on by default, but a daemon that reads on every change is exactly the case
that triggers the prompt once it is turned on. Planned mitigations:
- Query `NSPasteboard.accessBehavior` and report the state in `clipbroad status`. The daemon queries it
  every time it updates its status, because the permission belongs to the process that reads.
- Use the `detect` methods to know which types are there **without reading** before deciding to read.
- Document the permission in System Settings → Privacy & Security → "Paste from Other Apps"
  (Allow). There is no entitlement that avoids it.
- Manual test result (October 2026, `v0.1.0-rc.1`): on the owner's Mac, `watch` and `run`
  read text, images and files without macOS showing any prompt. It was not tested with the
  `EnablePasteboardPrivacyDeveloperPreview` developer preference on.

## Images

They are normalized to PNG on the wire (macOS uses TIFF/PNG; Windows uses DIB/PNG). They go inline while they are under
the size threshold. Code in `crates/core/src/content.rs` (`Png`) and `protocol.rs`.

- **Always PNG on the wire.** `ClipContent::Image(Png)` travels as `ContentKind::Image` with MIME
  `image/png`. The hash is BLAKE3 of the PNG bytes as they travel. A text and an image never have
  the same bytes: a PNG starts with `0x89`, and no UTF-8 text can start that way.
- **The conversion lives in the backends, not in `core`.** Each backend converts its native format to PNG
  on read (TIFF on macOS, DIB on Windows; Wayland already offers `image/png`). That way `core` does not depend on
  any codec. macOS converts with AppKit, with no dependencies. Windows uses the `png` crate (no extra
  features; MIT/Apache) and not the `image` crate, which is much heavier: it only needs to read and write
  uncompressed bitmaps.
- **`core` only checks the header.** `Png::try_from` accepts bytes that start with the PNG signature
  followed by the `IHDR` chunk, and rejects the rest with `NotPng`. It does not decode the image: a PNG broken
  past the header fails in the app that pastes, the same as if it had been copied on the same machine.
  `Clip::into_content` rejects an image clip that does not start as PNG (`ClipError::NotPng`), so a
  peer cannot put arbitrary bytes on the clipboard as an image.
- **`PROTOCOL_VERSION` goes to 2.** A node with version 1 cannot decode `ContentKind::Image`:
  the frame looks malformed to it and it closes the connection on every copied image. With the new version, two
  nodes with different versions reject each other on connect with the clear `Hello` error, and all
  machines have to be updated. The ALPN stays `clipbroad/1`: if it changed, the connection would fail
  in the handshake without saying why.
- **An image larger than `inline_max_bytes` is not synced:** the daemon skips it with a warning,
  the same as text. Once the file channel exists (see "Files"), sending it
  that way can be reconsidered.
- **Requirement for the backends (anti-echo):** anti-echo compares hashes. If a backend writes a PNG and
  reading it back returns other bytes (for example, because it re-encodes it from TIFF), the daemon sees it
  as a new copy and resends it. Two machines that re-encode could resend the image to each other endlessly.
  Because of that, a backend must return the same PNG bytes it wrote, for example by writing the PNG type
  and reading it first when it is there.
- **Text and image in the same copy: text wins.** The daemon calls `read_text` and, only if there is no
  text, `read_image` (`read_content` in `crates/daemon/src/run.rs`; before both it looks for
  files, see "Files on the clipboard"). Apps that offer both in
  one copy (spreadsheet cells, an office document) do it mostly so the text can
  also be pasted as a drawing; what the person meant to copy is the text, which is also much
  smaller. A clip carries a single content, so both cannot be sent.
  - **Exception, the browser's "Copy image":** Chrome offers the image and `text/html` with an
    `<img>` tag, without plain text. By contract, if the copy carries a readable image, `read_text`
    only counts plain text (on Wayland: `text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`,
    `STRING`, `TEXT`), so there the image wins and the HTML is not synced.
  - **Known limit:** Firefox, when copying an image, also offers the URL as plain text. There
    the URL wins. Fixing it would require sending several formats in one clip.
- **Wayland:** reads and writes only `image/png`, byte for byte, from the same offer whose MIME list
  it checked (same as text). A copy that only offers other types (`image/jpeg`, `image/bmp`)
  counts as having no image: converting it would require a codec. Screenshot tools (`grim`) and
  browsers offer `image/png`.
- **macOS:** reads `public.png` byte for byte. If there is only `public.tiff` (some apps, like Preview),
  AppKit converts it to PNG (`NSBitmapImageRep`). It writes only `public.png`: current macOS apps
  paste PNG. Same `changeCount` and marker check before and after reading as with text.
  `read_text` only reads `public.utf8-plain-text`, so the HTML next to an image does not count as text.
- **Windows:** first reads the registered `PNG` format (browsers, Snipping Tool), byte for byte. If it is not
  there, it converts to PNG the `CF_DIB` or `CF_DIBV5` the app put there (Print Screen, Paint; Windows
  derives each one from the other and from `CF_BITMAP`, and lists them after the originals). It only converts uncompressed 24- or 32-bit bitmaps (`BI_RGB`, or
  `BI_BITFIELDS` with 8-bit channels), top-down or bottom-up; anything else counts as
  having no image. In 32-bit `BI_RGB` the fourth byte counts as transparency unless it is 0 in all
  pixels (then it was not used). When writing it puts `PNG` first and a 32-bit `CF_DIB` copy
  for apps that do not paste PNG; since reading prefers `PNG`, what was written is read back with the same
  bytes. When reading `PNG` it cuts the bytes at the end of the `IEND` chunk, because a clipboard
  memory block can be larger than what was put in it. The marker check and the read
  happen in the same clipboard open, same as with text.
- `clipbroad watch` shows an image as `(image/png, 12345 bytes)` and `clipbroad status` shows the
  type of the last clip (`text` or `image`) next to the size and the hash. Files show as
  `(3 files, 41.9 MB)` in `watch` and `3 files, 41.9 MB` in `status`.

## Files

The OS clipboard stores **references** to files (URIs, `CF_HDROP`, file URLs), not their
content. Syncing files means transferring the content, storing it in a local cache
(`<cache>/clipbroad/files/<hash>/<name>`) and putting the local paths on the clipboard.

- First version: **immediate transfer** on copy, with a configurable size limit. Above
  the limit, the file is not synced.
- Lazy transfer (fetching the file only on paste) exists on each OS (Wayland is lazy by
  design; macOS has `NSFilePromise`; Windows has delayed rendering), but it is complex. It is postponed.
### File transport

**Decision: custom transfer, without `iroh-blobs`.** Each file travels on a bidirectional QUIC
stream inside the `clipbroad/1` connection that already exists with the peer. The receiver requests it by hash and
writes it to a temporary file while computing the BLAKE3. It only moves it to the cache if the size and the
hash match.

**Measurement** (spike 4.3, throwaway prototype). Two processes on the same machine over localhost, release
build, btrfs disk, 3 runs. Memory is the peak RSS of the whole process, iroh included.

| | 100 MB | 1 GB | Sender RSS | Receiver RSS | Receiver disk |
|---|---|---|---|---|---|
| Custom | 140-155 ms (650-715 MB/s) | 1.37 s | 17 MiB | 18 MiB (20 with 1 GB) | the file |
| `iroh-blobs` with `fs-store` | 147-155 ms with the export (645-680 MB/s) | 1.33 s | 20 MiB | 21-22 MiB (23 with 1 GB) | the file in its store and the exported copy |

Both options have the same speed and their memory does not grow with the file. The data does not
favor either one, so complexity and risk decide. Preparing the send takes 17 ms per
100 MB with the custom one (BLAKE3 only) and 54 ms with iroh-blobs (importing into the store also computes the verification
tree). Localhost does not measure the network: on a real LAN the limit is the link (about 110 MB/s on
gigabit, less on Wi-Fi). It was not measured on macOS or Windows.

Why not `iroh-blobs` (0.103.1, the only line compatible with iroh 1.x):
- **It is not production-ready, according to its README.** It recommends 0.35 for production, but that
  version requires iroh 0.35 and cannot coexist with our iroh 1.4.
- **Security bug fixed only recently.** Up to 0.103.0, with the default configuration any
  connected peer could write blobs into the other's store (*push* requests without permission,
  n0-computer/iroh-blobs#270). It was fixed in 0.103.1 (October 2026) and versions 0.97 to 0.103.0
  were yanked from crates.io.
- **Open bugs in the on-disk store** that would affect us: GC deletes blobs that are still in use (#247,
  #256, #314), a GC failure stops it forever (#291), corruption of the `redb` database under heavy
  I/O (#261), RSS that grows on reimport (#266). A SIGKILL in the middle of a download loses the progress
  (#254), so resumption does not survive a crash.
- **Unstable API.** It published 9 minor versions between October 2025 and June 2026, and in 0.x each
  one can break the API. It does not promise store format stability.
- **Weight.** With the minimal features it adds 29 crates, but that way it only brings the in-memory store, which
  loads the whole file into RAM. With `fs-store` it adds 31 (adding `redb` and `reflink-copy`). The licenses
  pass, but `cargo deny` fails on RUSTSEC-2024-0370 (`proc-macro-error` unmaintained, through
  `genawaiter`) and we would have to ignore another advisory. It also adds `syn` 1.x and another version of `spin`.
- **Fit.** The store is a separate database, with tags and GC to manage on top of the
  `<cache>/clipbroad/files/<hash>/<name>` cache. The receiver saves into the store and then exports to the
  cache: on btrfs or APFS it is a clone, but on ext4 or NTFS it writes the file twice. The provider
  hands any blob in the store to any connected peer, and `EventMask` has to be configured to
  reject *push* requests.
- **What it provides is not needed here:** range verification, resumption, download from several
  peers and collections. On the LAN a 100 MB file takes around one second, and retrying from
  scratch is acceptable.

Why the existing connection and not a separate ALPN (`clipbroad-file/1`):
- The connection is already authenticated and passed the allowlist. With a separate ALPN, the receiver would have to
  dial the sender, and without mDNS a peer without addresses only connects if the other one dials it (see
  "Connections"). In that case the receiver could not request the file.
- QUIC multiplexes the streams, so a large file does not hold back the clip frames. The clip
  stream has higher priority (`SendStream::set_priority`) so copied text does not wait behind
  a file.
- Streams are told apart by type. Each side still opens a single unidirectional stream for
  clips, and each bidirectional stream is a file request.
- Cost: if the connection drops or is replaced (see "Duplicate connections"), the transfer in progress
  is cut and retried on the new connection.

Implementation (code in `crates/core/src/files.rs`, `crates/net/src/files.rs` and
`crates/net/src/cache.rs`):

- **Announcement.** `ContentKind::Files` and `Payload::Files(Vec<FileEntry>)`, with
  `FileEntry { name, size, hash }`. `kind` is `Files`, `mime` is `text/uri-list` and `size` is the sum
  of the sizes. In `core`, `ClipContent::Files(FileList)` is the already checked list: not empty, with
  valid and unique names, sorted by name and with a sum of sizes that does not overflow.
- **Clip hash.** BLAKE3 in key derivation mode (context `"clipbroad 2026-10 file list"`)
  over each file in name order: the name length (little-endian u64), the name in UTF-8,
  the size (little-endian u64) and the 32 bytes of the file hash. The list is sorted by name
  because the OS can return the files in another order when reading them back, and anti-echo needs
  the same hash. Key derivation mode separates it
  from the plain BLAKE3 of a text or an image, so they never collide. It does not depend on where the files are
  on each machine: when the cache paths are put on the clipboard, the local event
  computes the same hash and the engine recognizes it as an echo.
- **Names.** A valid name is a single component that all three OSes can create as is:
  not empty, up to 255 bytes, without `/`, `\` or the other characters Windows does not allow
  (`: * ? " < > |`), without control characters (NUL included), without a trailing dot or space (that
  also rejects `.` and `..`) and without Windows device names (`CON`, `nul.txt`, `COM1`, ...).
  The same rules on all OSes, so a name one machine accepts is accepted by all. The sender
  skips files with such a name with a warning; the receiver rejects the whole clip. The
  errors never include the received name, which could put control characters into the logs.
- **Request.** The receiver opens a bidirectional stream and sends `FileRequest { hash }`. The sender
  responds `FileResponse::Ok { size }` followed by exactly `size` bytes and closes the stream, or
  `FileResponse::NotOffered`. The clip stream has priority 1 and the file streams have 0 by
  default, so copied text does not wait behind a file.
- **The sender only serves the files of its last clip** (`Offer`: hash → path and size). It reads them from
  their original path, without copying them. A hash that is not in the offer gets `NotOffered`, and so does a
  file whose size changed since it was copied (if it changed without changing size, the receiver notices
  through the hash). Any new `broadcast` replaces the offer; a text one empties it. The offer is
  published before the clip is sent, so a quick request finds it.
- **The sender checks on copy** (`Offer::read`): it skips with a warning folders and anything that is not
  a regular file, files larger than `file_max_bytes`, invalid names and a second
  file with the same name. If what remains adds up to more than `file_cache_max_bytes`, it announces nothing.
- **The receiver checks the limits before requesting:** each file up to `file_max_bytes` and all of them
  together up to `file_cache_max_bytes`. Otherwise, it fails without opening any stream. The `size` of the
  response must be the announced one.
- **The receiver verifies before exposing.** It writes to a temporary `.clipbroad-<random>.part` inside
  `<cache>/<hash>/`, updates the BLAKE3 with each block and cuts off as soon as one extra byte arrives.
  When done it compares size and hash: if they match, it renames the temporary file to the final name, and if not, it
  deletes it. A stream that brings nothing for 30 s is cut. Only paths of verified files are returned.
- **A file that is already in the cache is not requested.** Its hash is recomputed first, in case an
  app edited it in place (it costs about 17 ms per 100 MB); if it does not match, it is deleted and requested.
- **Canceling means dropping the download.** Dropping the `fetch_files` future closes the stream (the sender
  receives `STOP_SENDING`) and deletes the temporary file. The daemon drops the download in progress
  when a newer clip arrives.
- **Retries.** If the connection drops or is replaced in the middle of a file, it is requested again from
  scratch on the next connection, up to 3 attempts, waiting up to 10 s for the peer to come back. Other
  errors (different hash, `NotOffered`, limits) are not retried.
- **Folders:** out of the first version, they are skipped with a warning. Walking them would require names with
  a relative path (and validating them); compressing them, a format and a dependency. It will be decided when
  needed.
- `PROTOCOL_VERSION` goes to 3.

Cache (`FileCache`):

- **Location:** `files/<hash>/<name>` inside each OS's cache folder:
  `$XDG_CACHE_HOME/clipbroad/files` (`~/.cache/...`) on Linux, `~/Library/Caches/clipbroad/files` on
  macOS and `%LOCALAPPDATA%\clipbroad\files` on Windows. With `--config <file>` it is the `files` folder
  next to that file, so two instances on one machine never share the cache. Opening the cache
  deletes the temporary files a crash left behind, so only one process can use each folder.
- **Permissions:** on Unix the folders are created `0700` and the files end up `0600`, because what is copied
  can be private. A file pasted from the cache can keep those permissions.
- **Cleanup** after each download: deletes files older than `file_cache_max_days` days and
  then the oldest ones until it is under `file_cache_max_bytes`. Age is the modification
  date, which is renewed when a file is reused from the cache. It never deletes the
  files of the clip that was just downloaded, which is what the clipboard holds; it does not track what
  the clipboard points to afterwards. Temporary files of a download in progress are not touched, and it only looks at
  folders with a hash name, so nothing foreign that ends up in the cache gets deleted.
- **Config:** `file_max_bytes` (512 MiB by default), `file_cache_max_bytes` (2 GiB, also the
  limit for all the files of one copy) and `file_cache_max_days` (7). `file_max_bytes` cannot
  be greater than `file_cache_max_bytes`.

Risks and limits:
- **Custom code to maintain.** About 750 lines in `net` with comments and without tests (the spike
  estimated 300). Almost half is the cache and its cleanup.
- **No resumption.** On a slow network, a large file that gets cut starts from scratch. If this happens in
  real use, it can be requested from an offset (the temporary file already has the beginning) without changing the announcement.
- **Verification happens at the end.** A paired peer that sends garbage makes us write up to `size`
  bytes before detecting it. It is a trusted peer and the limit bounds the damage.
- **The engine marks the clip as applied before it has the files.** If the download fails, that
  clip is not retried until the next copy, as happens today with an image the OS cannot
  write.
- `iroh-blobs` can be evaluated again when its iroh 1.x compatible line is declared ready for
  production.

### Files on the clipboard

Code in `crates/clipboard` (`read_files`, `write_files`) and `crates/daemon/src/run.rs`.

- **The trait works with paths.** `read_files` returns the local paths of the copied files
  (`Ok(None)` if there are none) and `write_files` puts local paths on the clipboard. Same rules as
  text and images: 2 s at most, the daemon's own writes are reported and the sensitive marker
  check is done on the same content that is read. They do not check the file
  system: a path can be a folder or no longer exist, and `Offer::read` handles that.
- **Which one wins if a copy carries several: files, then text, then image.** A file manager
  also offers the paths as text, but what the person copied are the files. Between text and
  image the rules of "Images" apply (plain text wins, except the browser's "Copy image").
  `read_content` calls `read_files`, `read_text` and `read_image` in that order.
- **Wayland, reading.** Reads `text/uri-list` (RFC 2483) from the same offer whose MIME list it checked.
  Only `file://` URIs with an empty host, `localhost` or this machine's name count, and the
  `%XX` sequences are decoded. If a single URI is of another kind (a copied web link), the copy is not
  files and stays text. Lines that start with `#` are comments. A badly
  encoded URI, or one with a NUL, a query or a fragment, does not count either.
- **Wayland, writing.** Offers the same paths in three ways: `text/uri-list`,
  `x-special/gnome-copied-files` (`copy` and one URI per line, which is what GNOME Files reads) and plain text
  with one path per line, as GNOME Files does when copying, so pasting into a terminal or an editor
  gives the paths. The URIs encode with `%XX` everything that is not a letter, digit, `/`, `-`, `.`, `_` or
  `~`, so spaces and non-ASCII characters give the same path on a round trip.
- **When copying files here:** `Offer::read` checks and hashes each file off the
  async thread (it skips folders and files over the limit with a warning), the daemon builds the files
  clip and sends it with `broadcast_files`. `inline_max_bytes` does not apply to files.
- **When receiving a files clip:** the cache (`FileCache`) is opened at startup. The download is a
  future that the main loop handles along with everything else, so a large download does not hold back
  clips or local copies. There is at most one: any newer clip (received with
  `Apply`, or a local copy that is sent) drops it, and that cancels it (see "Canceling means dropping the
  download"). When it finishes, it is still the newest one by construction, and the daemon writes the
  cache paths with `write_files`. It is not a separate tokio task because it would have to share
  the `Node`, and canceling it and waiting for it to release the node on shutdown is more code.
- **Anti-echo.** Writing the cache paths fires a local event. The daemon reads them, and
  `Offer::read` computes the same list hash as the received clip: the hash depends only on the name,
  size and hash of each file, not on where they are. The engine returns `Unchanged` and nothing is resent.
  It costs reading the files again (about 17 ms per 100 MB).
- **macOS, reading.** Finder puts one item per file on the pasteboard, each with a
  `public.file-url` (`NSPasteboardTypeFileURL`). The one from each item is read and `NSURL` turns it into a path;
  that way reference URLs (`file:///.file/id=...`, which name the file
  by id) also resolve to the file's current path. Only `file:` URLs without a host or with `localhost` count. If
  an item does not have one (a copied web link carries `public.url`, not `public.file-url`), the copy is not
  files and stays text. Same `changeCount` and marker check before and
  after reading as with text.
- **macOS, writing.** Writes one file `NSURL` per path (`writeObjects`), the same thing Finder
  pastes. Paths must be UTF-8, like macOS paths.
- **Windows, reading.** Reads `CF_HDROP`: a `DROPFILES` header and the paths, each one terminated
  by NUL, with an extra NUL at the end. It is decoded by hand (`crates/clipboard/src/hdrop.rs`), without
  `DragQueryFileW`, so the format is tested on all three OSes. ANSI paths (`fWide` = 0, from
  very old apps) are only read if they are ASCII, because the rest depends on the system
  code page. The marker check and the read happen in the same clipboard open.
- **Windows, writing.** Writes `CF_HDROP` with UTF-16 paths (`fWide` = 1) and `Preferred
  DropEffect` = `DROPEFFECT_COPY`, so Explorer pastes a copy and never moves the files out
  of the cache. Paths with spaces and non-ASCII characters go as is.

## Daemon lifecycle

- `clipbroad service install|uninstall|status` installs start at login with each OS's
  mechanism. The service runs the absolute path of the current executable with `run` (and `--config` only
  if it was passed to `install`), so moving the binary requires installing again. `install` is
  idempotent: it rewrites the definition and restarts the daemon.
- Linux: systemd user unit at `~/.config/systemd/user/clipbroad.service`, bound to
  `graphical-session.target` (`WantedBy`, `After`, `PartOf`) and with `Restart=on-failure` every 5 s. The
  logs go to journald. It needs the session to import `WAYLAND_DISPLAY` into the systemd user manager
  and to activate `graphical-session.target` (uwsm does this); `install` warns if either one is missing.
- macOS: LaunchAgent in `~/Library/LaunchAgents/` (domain `gui/<uid>`), with `KeepAlive` only on
  failures. The logs go to `~/Library/Logs/clipbroad.log`, which grows without rotation.
- Windows: `clipbroad` value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` instead of a
  scheduled task: it does not ask for administrator permissions (`schtasks /sc onlogon` does) and does not inherit the
  task defaults (they stop on battery and after 3 days). In exchange, nothing
  restarts it if it crashes. It is launched with `conhost.exe --headless` so it does not open a console window;
  because of that its logs do not end up anywhere. `install` and `uninstall` stop any other
  `clipbroad.exe` that is running.
- TOML configuration in each OS's configuration directory; identity (`SecretKey`) in a
  separate file with restricted permissions. Commented example with all the options and paths:
  [`config.example.toml`](config.example.toml).
- Dedicated crate `clipbroad-config`, used by the daemon, `net` (peers and identity) and pairing,
  which writes to this configuration.
- Identity: the raw 32 bytes of the Ed25519 key (`iroh::SecretKey::from_bytes` converts them).
  It is generated the first time. On Unix the file is created with `0600` permissions and rejected if other
  users can read it; on Windows it inherits the permissions of `%LOCALAPPDATA%` (only the user,
  administrators and SYSTEM).
- On Windows `%LOCALAPPDATA%` is used (not the *roaming* folder): the identity and the peer list belong to
  one machine and must not be copied to another with the roaming profile.
- Paths are computed with the standard library instead of the `directories` crate, because that crate depends
  on `option-ext` (MPL-2.0), which the `cargo deny` license policy does not allow.

## Distribution

- Each `v*` tag publishes a GitHub Release with the 3 binaries (`.tar.gz` / `.zip`) and `SHA256SUMS`. The
  process is in [`RELEASE.md`](RELEASE.md).
- A custom workflow instead of `cargo-dist`: 3 archives and checksums are enough, and a short hand-written
  workflow is easier to maintain than a generated one. It will be reviewed if installers or Homebrew are needed.
- The version lives in `[workspace.package]`; the workflow fails if the tag does not match
  `clipbroad --version`.
- macOS: no signing or notarization for now (that requires an Apple Developer account).

## Phases

| Phase | Content | Done when |
|---|---|---|
| 0 | Workspace, CI on 3 OSes, AGENTS.md, this document | CI is green on the 3 OSes |
| 1 | Clipboard backends (text) + `clipbroad watch` that prints the changes + macOS privacy spike | It prints what was copied on each OS; the macOS behavior is documented here |
| 2 | P2P network, pairing, text sync with anti-echo | Copy on one machine and paste on the other two |
| 3 | Sensitive content filter, startup service, `clipbroad status` | One day of real use without problems |
| 4 | Images, then files | Screenshot on one machine, paste on another; 100 MB file |
| 5 | Tray icon, history (optional) | n/a |

## Testing strategy

- The logic (protocol, anti-echo, filters) is tested with the in-memory fake clipboard.
- Integration tests that start 2-3 nodes in the same process and verify propagation.
- CI runs on Linux, macOS and Windows; it is the only way to validate macOS/Windows from the devbox.
- What depends on the real OS (clipboard events, macOS permissions) is validated with a list of
  manual tests per platform, kept in `docs/` starting with phase 1.
