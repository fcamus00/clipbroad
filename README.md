# clipbroad

Share the clipboard between your own machines (Linux/Wayland, macOS, Windows). Peer-to-peer over your
LAN or Tailscale; no server, no third-party services.

> Status: early development. See [docs/DESIGN.md](docs/DESIGN.md) for the design and roadmap.

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
