# clipbroad

Share the clipboard between your own machines (Linux/Wayland, macOS, Windows). Peer-to-peer over your
LAN or Tailscale; no server, no third-party services.

> Status: early development. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

## Install

| Platform | Command |
|---|---|
| macOS (Apple Silicon) | `brew install fcamus00/tap/clipbroad` |
| Windows | `scoop bucket add fcamus00 https://github.com/fcamus00/scoop-bucket` then `scoop install fcamus00/clipbroad` |
| Linux (x86_64) | `curl -fsSL https://github.com/fcamus00/clipbroad/releases/latest/download/install.sh \| sh` |

On Linux, the script checks the archive against `SHA256SUMS` and installs the binary to
`~/.local/bin`. Run the same command again to upgrade.

Then run `clipbroad service install` to start the daemon at every login (see
[Start at login](#start-at-login)). After an upgrade, run it again to restart the daemon on the new
version. On Windows, Scoop cannot replace the binary while the daemon runs: run
`clipbroad service uninstall` before `scoop update clipbroad`, and `clipbroad service install` after.

## Download a release

Each [GitHub Release](https://github.com/fcamus00/clipbroad/releases) has an archive per platform
(`.tar.gz` for Linux and macOS, `.zip` for Windows) and a `SHA256SUMS` file:

```sh
gh release download --repo fcamus00/clipbroad --pattern '*linux-x86_64*' --pattern SHA256SUMS
grep linux-x86_64 SHA256SUMS | sha256sum -c      # macOS: shasum -a 256 -c
```

The macOS binary is not signed; see the quarantine note below. Maintainers: the release process is in
[docs/RELEASE.md](docs/RELEASE.md).

## Download a CI build

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
Then check it:

```sh
./clipbroad --version
```

On macOS the binary is not signed. A browser marks downloaded files as quarantined, and Gatekeeper
then refuses to run the unsigned binary. Remove the quarantine flag before running it:

```sh
xattr -d com.apple.quarantine clipbroad
```

(`gh run download` usually does not set the flag; it is needed when the file was downloaded with a
browser. If `xattr` says "No such xattr", there is nothing to do.)

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

## Development

```sh
mise install                 # Rust toolchain + dev tools
cargo run -p clipbroad -- --help
```

See [AGENTS.md](AGENTS.md) for conventions and the full check list.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
