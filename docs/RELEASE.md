# Releases

A `v*` tag on GitHub triggers `.github/workflows/release.yml`, which builds the 3 binaries and publishes a
GitHub Release with them.

## How to cut a release

1. Bump the version in `[workspace.package]` of the root `Cargo.toml` (for example `0.2.0`) and run
   `cargo check` to update `Cargo.lock`. The build uses `--locked` and fails if the lock is stale.
2. Open a PR with that change and merge it with green CI.
3. Create the tag on `main` and push it:

   ```sh
   git fetch origin
   git tag v0.2.0 origin/main
   git push origin v0.2.0
   ```

4. Follow the workflow with `gh run watch` (or in the Actions tab). When it finishes, the release is at
   `gh release view v0.2.0`.

A tag with a suffix (`v0.2.0-rc.1`) creates a **pre-release**. The binary version does not carry the suffix:
`v0.2.0-rc.1` expects `clipbroad --version` to say `clipbroad 0.2.0`.

## What the workflow does

- **build** (Linux x86_64, macOS Apple Silicon, Windows x86_64): `cargo build --release --locked`, and
  checks that `clipbroad --version` matches the tag. If it does not match, it fails and publishes nothing.
- Packages the binary with `README.md`, `LICENSE-APACHE` and `LICENSE-MIT` in a
  `clipbroad-<tag>-<platform>/` folder, as `.tar.gz` (Linux, macOS) or `.zip` (Windows).
- **release**: collects the 3 archives, writes `SHA256SUMS` and creates the release with `gh release create`
  (notes generated from the merged PRs).

## If something fails

The release is created in the last step; if something fails earlier, nothing is published. To retry with the same
number: fix it on `main`, delete the tag and create it again.

```sh
git push origin :refs/tags/v0.2.0 && git tag -d v0.2.0
gh release delete v0.2.0 --yes   # only if the release got created
```

## Install from a release

```sh
gh release download v0.2.0 --repo fcamus00/clipbroad --pattern '*linux-x86_64*' --pattern SHA256SUMS
grep linux-x86_64 SHA256SUMS | sha256sum -c      # on macOS: shasum -a 256 -c
tar -xzf clipbroad-v0.2.0-linux-x86_64.tar.gz
./clipbroad-v0.2.0-linux-x86_64/clipbroad --version
```

On macOS the binary is not signed or notarized (that requires an Apple Developer account; it is in the
backlog). If the file was downloaded with a browser, macOS quarantines it and Gatekeeper does not let it
run. Remove the flag after extracting:

```sh
xattr -d com.apple.quarantine clipbroad
```

`gh release download` usually does not set the flag. If `xattr` says "No such xattr", there is nothing to
do.

## Why a custom workflow and not cargo-dist

We evaluated [`cargo-dist`](https://github.com/axodotdev/cargo-dist) (0.32). It does the same and more
(shell/PowerShell installers, Homebrew, MSI), but it generates a workflow of several hundred lines that
is not edited by hand, adds its own configuration (`dist-workspace.toml`) and has to be regenerated when
the tool is updated. Today we only need 3 archives and checksums: the custom workflow is about 90
lines that you can read in one go, and it uses the same actions as CI. If installers or Homebrew are needed
later, it is worth looking at `cargo-dist` again.
