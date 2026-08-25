# cu

`cu` is a local, agent-facing computer-use service for Linux desktops. One
daemon owns screen capture and input injection; the CLI and MCP server talk to
it over a private Unix socket.

## Install and run

Requirements: Rust 1.88+ and either a Wayland compositor exposing direct wlr
screencopy, virtual pointer, and virtual keyboard protocols, or an X11 server
exposing XTEST, XInput, and XKB.

For development, link `~/.local/bin/cu` to the release binary. The link remains
stable across rebuilds:

```sh
./scripts/install-dev
# After editing source:
cargo build --release -p cu
```

The examples below assume `~/.local/bin` is on `PATH`. For a copied installation
instead, run `cargo install --locked --path crates/cu --root "$HOME/.local" --force`.

```sh
# Detect the current desktop session
cu daemon

# Explicit backend overrides
cu daemon --backend wayland
cu daemon --backend x11 --display :0
```

Automatic mode uses `XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`, and `DISPLAY`. A
Wayland session selects the direct Wayland backend even when XWayland also sets
`DISPLAY`; an X11 session selects the X11 backend. `--display` selects X11 in
automatic mode, while `--output` selects direct Wayland. `--max-width` and
`--max-height` add optional downscaling limits for either backend.

| Desktop session | Backend | Status |
| --- | --- | --- |
| X11 desktop or window manager | X11 root capture and XTEST input | Supported |
| Wayland compositor exposing the required direct protocols | Direct Wayland | Supported |
| Portal-only Wayland desktop, including typical GNOME and KDE sessions | RemoteDesktop + ScreenCast/PipeWire + EIS | Not implemented yet |

The daemon does not silently fall back from Wayland to XWayland because the
XWayland root is not the complete desktop. By default, the socket and retained
frames live under `$XDG_RUNTIME_DIR/computer-use` with owner-only permissions.

For the local isolated `:99` desktop, install the bundled user unit after
`scripts/install-dev`:

```sh
install -Dm644 systemd/codex-agent-cu.service \
  ~/.config/systemd/user/codex-agent-cu.service
systemctl --user daemon-reload
systemctl --user enable --now codex-agent-cu.service
```

The unit is bound to `codex-agent-display.service` and is upheld by it, so an
Xvfb restart cannot leave `cu` connected to the old X server.

Observe before acting:

```sh
cu observe
```

Then pass the returned `frame_id` as `expected_frame_id`:

```sh
printf '%s\n' '{"expected_frame_id":"<frame_id from observe>","actions":[{"type":"click","x":800,"y":450}]}' \
  | cu act --request-id agent-step-1
```

Coordinates are pixels in the returned, possibly downscaled frame. Supported
actions are `move`, `click`, `double_click`, `drag`, `scroll`, `type`, and
`keypress`. A batch contains at most 16 actions.

## MCP

Start `cu daemon` separately, then register the local stdio MCP server with one
of these commands.

Codex:

```sh
codex mcp add cu -- cu mcp
```

Claude Code (user scope, available across projects):

```sh
claude mcp add --scope user --transport stdio cu -- cu mcp
```

It exposes two tools:

- `computer_observe` returns structured frame metadata and a PNG image.
- `computer_act` requires the latest `frame_id`, executes a validated batch,
  and returns structured execution metadata plus the resulting PNG.

The published schemas describe every action, coordinate and key convention,
settling limits, partial execution, and stale-frame recovery.

The engine rejects stale frames before input, validates the complete batch
before its first side effect, executes actions in order, and captures state
after success or partial execution. Reusing a CLI `--request-id`, or retrying
the same MCP request within one MCP session, returns the cached action result
instead of executing twice. This idempotency cache does not survive a daemon
restart.

`settled: false` means the screen kept changing until the settle timeout; the
frame is still usable. The direct Wayland backend intentionally refuses multiple
active outputs because output-specific absolute pointer mapping is not yet
implemented.
