# clipbroad

Share the clipboard between your own machines (Linux/Wayland, macOS, Windows). Peer-to-peer over your
LAN or Tailscale; no server, no third-party services.

> Status: early development. See [docs/DESIGN.md](docs/DESIGN.md) for the design and roadmap.

## Development

```sh
mise install                 # Rust toolchain + dev tools
cargo run -p clipbroad -- --help
```

See [AGENTS.md](AGENTS.md) for conventions and the full check list.
