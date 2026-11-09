# clipbroad

Share the clipboard between your own machines (Linux/Wayland, macOS, Windows). Copy on one machine and
paste on any other. Peer-to-peer over your LAN or Tailscale, with no server and no account. Your clips
never leave your machines. The only outside connection is a daily check for new versions, which
[you can turn off](#updates).

> Status: early development. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

## How it works

Each machine runs the same `clipbroad` binary as a daemon (`clipbroad run`). The daemons form a
mesh: every machine connects straight to every other one, and each copy goes to all of them in one
hop. If one machine is off, the rest keep syncing.

- **What syncs.** Text, images and files. Images travel as PNG. Files are copied to the other
  machines when you copy them, stored in a local cache, and their paths there go on the clipboard,
  so pasting in a file manager pastes the files.
- **Who wins.** The newest copy wins on every machine, even if the clocks of the machines differ.
- **Who can connect.** Only machines you paired. The connections are encrypted QUIC (with
  [iroh](https://github.com/n0-computer/iroh)), and each machine is identified by a key that it
  creates the first time it starts.
- **How machines find each other.** On the same LAN, with mDNS, so a changed IP is no problem.
  Over Tailscale or another VPN, mDNS does not reach, so you give the address by hand (see
  [Tailscale](#tailscale-and-other-networks)).
- **Passwords.** Content that a password manager marks as sensitive (KeePassXC, 1Password and
  others that use the standard markers) is never read or sent.
- **No history.** A machine that is offline misses what you copy meanwhile. When it comes back,
  it gets the next copy.

## Requirements

| OS | Supported |
|---|---|
| Linux | Wayland compositors with the data-control protocol: Hyprland, Sway, KDE Plasma and other wlroots ones. GNOME and X11 are not supported. x86_64 only. |
| macOS | Apple Silicon. |
| Windows | x86_64. |

All the machines must run the same protocol version. When a release changes it, machines on
different versions stop syncing until all of them are updated.

## Install

| Platform | Command |
|---|---|
| macOS (Apple Silicon) | `brew install fcamus00/tap/clipbroad` |
| Windows | Download and open `clipbroad-<version>-windows-x86_64-setup.exe` from the [latest release](https://github.com/fcamus00/clipbroad/releases/latest) |
| Linux (x86_64) | `curl -fsSL https://github.com/fcamus00/clipbroad/releases/latest/download/install.sh \| sh` |

The Windows installer needs no admin rights. It installs to `%LOCALAPPDATA%\Programs\clipbroad`, adds
that folder to `PATH`, and by default starts the daemon at every login. To remove clipbroad, uninstall
it from Settings > Apps. The installer is not signed yet, so Windows SmartScreen shows "Windows
protected your PC": click "More info", then "Run anyway".

On Linux, the script checks the archive against `SHA256SUMS` and installs the binary to
`~/.local/bin`.

To download a release archive or a CI build by hand, see [Other downloads](#other-downloads).

## Quick start

On every machine:

1. Install clipbroad (see [Install](#install)).
2. Start the daemon at login, and right now:

   ```sh
   clipbroad service install
   ```

   The Windows installer does this already. On Linux, read [Start at login](#start-at-login) first
   if you do not use uwsm.

Then pair the machines, once:

3. On a machine that is already set up, run `clipbroad pair`. It prints a ticket and waits for 5
   minutes.
4. Bring the ticket to the new machine your own way (a message, a mail, typing it) and run
   `clipbroad pair <TICKET>` there.
5. Both commands print the result, and the daemons start syncing right away, without a restart.

Copy something on one machine and paste it on the other. `clipbroad status` shows the connected
machines and the last clip sent and received.

You can also pair from the tray icon: **Pair a machine** copies the ticket to the clipboard, and
**Pair with the copied ticket** on the other machine uses it (see [Tray icon](#tray-icon)).

## Add and remove machines

To add a third machine, or more, pair it with any one member of the mesh. The members tell each
other about the new machine, and it syncs with all of them once they meet. A ticket works once and
for 5 minutes, and clipbroad never sends it to the machines already paired.

`clipbroad remove <NAME>`, or **Remove a machine** in the tray menu, takes a machine out of the
mesh for good, on every member. Use the name that `clipbroad status` shows, or the first characters
of the id. The removed machine cannot join again with the same identity: run `clipbroad reset`
there, or choose **Reset identity** in its tray menu, and pair it again.

If someone else used a leaked ticket before you, your `clipbroad pair <TICKET>` fails. Check
`clipbroad status` on the machine that showed the ticket: it says which member added each machine,
so you can remove the one you do not know.

## Tailscale and other networks

mDNS only works inside one LAN. For machines that reach each other over Tailscale or another VPN,
write the address of each peer in [`config.toml`](#configuration):

```toml
[[peers]]
name = "laptop"
id = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6"
addrs = ["100.101.102.103:42420"]
```

- `id` is the peer's node id. Run `clipbroad status` on that machine and copy the `node id` line.
- `addrs` are `ip:port` pairs: the Tailscale IP (`100.x.y.z`) or its IPv6 in brackets, and the
  peer's `listen_port` (42420 by default).
- clipbroad reads `[[peers]]` only when the daemon starts. Restart it with `clipbroad service
  install` after editing.

The address you write here wins over the one saved at pairing. A peer in `[[peers]]` joins the mesh
the same as a paired one.

## What syncs and what does not

| Content | Syncs | Limit |
|---|---|---|
| Text | Yes | `inline_max_bytes`, 8 MiB by default |
| Images | Yes, as PNG | `inline_max_bytes`, 8 MiB by default |
| Files | Yes, copied into the cache of the other machines | `file_max_bytes`, 512 MiB per file by default |
| Folders | No, they are skipped | |
| Content marked as sensitive | No, never read | |

A copy larger than its limit is skipped with a warning in the logs.

When a copy holds several kinds of content, one wins: files, then text, then the image. A copied
spreadsheet cell sends its text. The browser's "Copy image" sends the image, except on Firefox,
which also offers the URL as text, so the URL wins there.

Received files live in the cache until it passes `file_cache_max_bytes` (2 GiB) or they are older
than `file_cache_max_days` (7 days). The oldest go first, and the files of the last received clip
always stay.

A password manager that copies without a sensitive marker gets synced like any other app. Check what
yours does with `clipbroad watch`: copy a password and look for the marker in the line it prints.

## Tray icon

`clipbroad run` shows an icon in the notification area on Windows, the menu bar on macOS, and the
tray of your bar on Linux (waybar and quickshell on Hyprland, swaybar on Sway, the panel on KDE). A
blue circle means that at least one machine is connected and sync is on. A grey ring means that none
is connected, or that sync is paused. On Linux, open the menu with a right click.

The menu shows how many machines are connected and the last clip sent or received, and has these
items:

- **Pair a machine** copies a pairing ticket to the clipboard and waits 5 minutes for the other
  machine. Send the ticket to that machine your own way, for example by mail. The ticket works once,
  and clipbroad never sends it to the machines already paired.
- **Pair with the copied ticket** pairs with the ticket on the clipboard. Use it on the other machine
  after copying the ticket there. Both machines start syncing right away. The first line under the
  status says how it went.
- **Remove a machine** lists the members of the mesh. Point at one and choose **Remove ... for
  good** to take it out of the mesh, as `clipbroad remove` does.
- **Reset identity** gives this machine a new identity and restarts the daemon, as `clipbroad reset`
  does. It also deletes the `[[peers]]` in its `config.toml`. Use it on a machine that was removed,
  before pairing it again.
- **Pause sync** stops sending and applying clips, until **Resume sync** or a restart. What you copy
  during a pause is not sent later.
- **Check for updates** looks for a new version, and turns into **Update to 0.3.1 and restart** when
  there is one (see [Updates](#updates)). The first line under the status says how it went.
- **Quit clipbroad** stops the daemon. It starts again at the next login if the service is installed.

Without a tray, as on Hyprland without a bar, the daemon syncs the same and logs a warning. Use
`clipbroad pair`, `clipbroad status` and `clipbroad update` instead.

## Commands

| Command | What it does |
|---|---|
| `clipbroad run` | Runs the daemon in the foreground until Ctrl+C, SIGTERM or **Quit** in the tray. |
| `clipbroad pair` | Prints a ticket and waits 5 minutes for another machine to use it. |
| `clipbroad pair <TICKET>` | Pairs with the machine that printed the ticket. |
| `clipbroad status` | Shows whether the daemon runs, its version and node id, each peer and whether it is connected, and the last clip sent and received. |
| `clipbroad remove <NAME>` | Takes a machine out of the mesh, for good. |
| `clipbroad reset` | Gives this machine a new identity, to pair again after it was removed. |
| `clipbroad watch` | Prints one line per local clipboard change: time, formats and a preview. Sends nothing. |
| `clipbroad service install` | Starts the daemon at every login, and now. Running it again restarts the daemon. |
| `clipbroad service status` | Shows whether the service is installed and running, and where its logs are. |
| `clipbroad service uninstall` | Stops the service and removes it. |
| `clipbroad update` | Installs the latest release, if it is newer, and restarts the daemon. |

`pair`, `run`, `status`, `remove`, `reset`, `update` and `service install` take `--config <FILE>` to
use a config file other than the default one. The identity and the rest of the state then live in
the same folder as that file, so two instances on one machine need a folder each.

`-v` adds log output on stderr, once per level (`-v`, `-vv`, `-vvv`). `run` logs at info by default
and the other commands at warn. `RUST_LOG` (for example `RUST_LOG=info,iroh=debug`) overrides `-v`.

Logs show each clip's size and short hash, never its content.

## Configuration

Every setting is optional, and clipbroad works without a config file. To change one, create
`config.toml` here:

| OS | Path |
|---|---|
| Linux | `~/.config/clipbroad/config.toml` (or `$XDG_CONFIG_HOME/clipbroad/config.toml`) |
| macOS | `~/Library/Application Support/clipbroad/config.toml` |
| Windows | `%LOCALAPPDATA%\clipbroad\config.toml` |

| Setting | Default | What it does |
|---|---|---|
| `listen_port` | `42420` | UDP port the daemon listens on. |
| `mdns` | `true` | Finds peers on the LAN. With `false`, the daemon only dials the `addrs` of `[[peers]]`. |
| `inline_max_bytes` | `8388608` (8 MiB) | Largest text or image that syncs. At most 16 MiB minus 64 KiB. |
| `file_max_bytes` | `536870912` (512 MiB) | Largest file that syncs. |
| `file_cache_max_bytes` | `2147483648` (2 GiB) | Size of the cache of received files, and the limit for all the files of one copy. |
| `file_cache_max_days` | `7` | Days a received file stays in the cache. |
| `update_check` | `true` | Looks for a new version once a day. |
| `[[peers]]` | none | Peers written by hand, with `id`, `name` and `addrs` (see [Tailscale](#tailscale-and-other-networks)). |

The daemon reads the file when it starts, so restart it after a change (`clipbroad service
install`). A misspelled setting is an error, not ignored. [`docs/config.example.toml`](docs/config.example.toml)
has every setting with its full explanation.

Next to the identity, clipbroad keeps its own files. Do not edit them:

| File | What it holds |
|---|---|
| `secret.key` | This machine's identity. Only your user can read it; on Linux and macOS the daemon refuses to start if others can. |
| `members.toml` | The machines in the mesh, shared with the other members. |
| `status.toml` | What the running daemon is doing, for `clipbroad status`. |
| `control.sock` | How `pair`, `remove`, `reset` and `update` talk to the running daemon. |

They are in `~/.local/share/clipbroad` on Linux, `~/Library/Application Support/clipbroad` on macOS
and `%LOCALAPPDATA%\clipbroad` on Windows. Received files go to `~/.cache/clipbroad/files` on Linux,
`~/Library/Caches/clipbroad/files` on macOS and `%LOCALAPPDATA%\clipbroad\files` on Windows.

## Start at login

`clipbroad service install` makes the daemon (`clipbroad run`) start at every login, and starts it
right away. Pass `--config <FILE>` to use a config file other than the default one. Running it again
updates the service and restarts the daemon. `clipbroad service status` shows whether it is installed
and running and where its logs are; `clipbroad service uninstall` stops and removes it.

The service runs the binary from the path it had during `install`: after moving the binary, run
`service install` again.

| OS | Mechanism | Logs |
|---|---|---|
| Linux | systemd user unit, `~/.config/systemd/user/clipbroad.service` | `journalctl --user -u clipbroad` |
| macOS | LaunchAgent, `~/Library/LaunchAgents/io.github.fcamus00.clipbroad.plist` | `~/Library/Logs/clipbroad.log` |
| Windows | `clipbroad` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | not kept |

On Linux the unit starts with `graphical-session.target` and needs `WAYLAND_DISPLAY` in the systemd
user manager. Check both with `systemctl --user is-active graphical-session.target` and
`systemctl --user show-environment`. Starting Hyprland through uwsm sets up both. Otherwise, add
`exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY` to `hyprland.conf` for
the variable. Without a session manager that activates the target, start the daemon with
`exec-once = clipbroad run` in `hyprland.conf` instead of the service. `service install` warns when
either is missing.

On Windows the daemon runs without a console window, so its logs are lost. Windows does not restart
it if it crashes; it starts again at the next login. `install` and `uninstall` stop every other
running `clipbroad.exe`.

## Updates

clipbroad looks for a new version one minute after it starts and then once a day, and shows it in
the tray menu and in `clipbroad status`. Nothing installs until you choose **Update to ... and restart**
or run `clipbroad update`. The daemon then restarts on the new version, by itself.

- With Homebrew, it runs `brew update` and `brew upgrade`, so Homebrew keeps track of the version.
- Any other install downloads the release, checks a signature made outside GitHub and the
  checksum, and replaces the binary.
- A pause does not survive a restart, so the menu asks you to resume sync first.
- Replacing the binary another way, such as `brew upgrade`, the installer of the new version, the
  same `install.sh` command, or copying a new binary, does not restart the daemon. `clipbroad status`
  shows the daemon's version and says when it is older than the `clipbroad` you ran. Restart it with
  `clipbroad service install`.
- If a new version changes the protocol, machines on different versions stop syncing until all of
  them are updated.

Looking for a version is the only connection clipbroad makes outside your machines. It is one
request to `github.com` that sends your public IP and a fixed `clipbroad` user agent, with no version,
machine id or clipboard data. Installing also downloads from GitHub. To stop looking on its own, set
`update_check = false` in the config: the menu item and `clipbroad update` still work.

## Privacy and security

- Clips go only to paired machines, encrypted. There is no relay or server in between.
- With `mdns = true`, anyone on the same LAN can see that this machine runs clipbroad, with its
  node id and addresses. The id grants no access: only paired machines are accepted. Set
  `mdns = false` to announce nothing.
- A pairing ticket is a one-time secret. Anyone who uses it within its 5 minutes, before you,
  joins the whole mesh. Send it over a channel you trust.
- Logs and `clipbroad status` never show clip content.
- On macOS 15.4 or newer, the system can ask for permission before an app reads the clipboard.
  If clips stop being sent from a Mac, open System Settings > Privacy & Security > Paste from Other
  Apps and set clipbroad to Allow. `clipbroad status` shows the current setting.

## Troubleshooting

Start with `clipbroad status` on each machine.

- **"daemon not running".** Start it with `clipbroad service install`, or run `clipbroad run` in a
  terminal to see its logs. `clipbroad service status` says where the service logs are.
- **A peer shows as not connected.** Both machines need the daemon running and the same version.
  On different networks, add the peer's `addrs` (see [Tailscale](#tailscale-and-other-networks)).
  Check that a firewall lets the `listen_port` (UDP 42420) through.
- **The daemon is older than the binary.** You replaced the binary without restarting the daemon.
  Run `clipbroad service install`.
- **A copy does not sync.** Run `clipbroad watch` and copy again. If the line shows a sensitive
  marker, clipbroad skips it on purpose. If no line shows, clipbroad cannot see the clipboard: on
  Linux, check that the compositor supports data-control and that the service has `WAYLAND_DISPLAY`.
  Otherwise, look for a size warning in the daemon's logs.
- **More detail.** Stop the running daemon (**Quit** in the tray, or `clipbroad service uninstall`)
  and run it by hand with `clipbroad run -v` for debug logs.

## Other downloads

### Release archives

Each [GitHub Release](https://github.com/fcamus00/clipbroad/releases) has an archive per platform
(`.tar.gz` for Linux and macOS, `.zip` for Windows) and a `SHA256SUMS` file:

```sh
gh release download --repo fcamus00/clipbroad --pattern '*linux-x86_64*' --pattern SHA256SUMS
grep linux-x86_64 SHA256SUMS | sha256sum -c      # macOS: shasum -a 256 -c
```

The macOS binary is not signed; see the quarantine note below. Maintainers: the release process is in
[docs/RELEASE.md](docs/RELEASE.md).

### CI builds

Every push to `main` and every pull request builds `clipbroad` for each OS. The binaries are attached
to the CI run as artifacts (kept for 90 days by default):

| Artifact | Platform |
|---|---|
| `clipbroad-linux-x86_64` | Linux, x86_64 |
| `clipbroad-macos-aarch64` | macOS, Apple Silicon |
| `clipbroad-windows-x86_64` | Windows, x86_64 |

Download with the [GitHub CLI](https://cli.github.com/) (or from the run's "Artifacts" section on
GitHub):

```sh
gh run list --workflow CI --branch main --limit 1        # find the run id
gh run download <run-id> --name clipbroad-linux-x86_64   # downloads into the current directory
```

On Linux and macOS, if the binary is not executable after the download, run `chmod +x clipbroad`.
Then check it with `./clipbroad --version`.

### macOS quarantine

The macOS binary is not signed. A browser marks downloaded files as quarantined, and Gatekeeper
then refuses to run the unsigned binary. Remove the quarantine flag before running it:

```sh
xattr -d com.apple.quarantine clipbroad
```

`gh run download` usually does not set the flag; it is needed when the file was downloaded with a
browser. If `xattr` says "No such xattr", there is nothing to do. Homebrew installs need none of
this.

## Development

```sh
mise install                 # Rust toolchain + dev tools
cargo run -p clipbroad -- --help
```

See [AGENTS.md](AGENTS.md) for conventions and the full check list, and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
