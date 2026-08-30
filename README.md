# cu

`cu` is a local, agent-facing computer-use service for Linux desktops. Each
daemon instance owns one screen-capture/input target; CLI and MCP clients talk
to it over a private Unix socket. Multiple instances may serve different
desktops concurrently.

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

# Independent daemons for two X11 displays
cu daemon --instance x11-99 --backend x11 --display :99
cu daemon --instance x11-100 --backend x11 --display :100
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
XWayland root is not the complete desktop. The default instance keeps its
socket and retained frames under `$XDG_RUNTIME_DIR/computer-use` for backward
compatibility. Named instances use
`$XDG_RUNTIME_DIR/computer-use/instances/<name>`. Every instance has an
independent socket, frame store, and ownership leases, so only daemons that try
to share the same socket or frame store conflict. Sibling `.lock` files provide
those leases and intentionally remain after shutdown; the kernel lock, not file
existence, represents ownership.

For advanced raw paths, specify both daemon resources together. A lone
`--socket` or `--frame-dir` is rejected, avoiding two sockets silently sharing
one frame store:

```sh
cu daemon --socket "$XDG_RUNTIME_DIR/cu-99.sock" \
  --frame-dir "$XDG_RUNTIME_DIR/cu-99-frames" --backend x11 --display :99
```

Each frame store retains at most `--max-frames` PNGs (32 by default and at least
2). On daemon startup, it removes managed PNGs and incomplete temporary files
left by the previous owner of that store. Files are private, atomically
published, and never cleaned by another live instance. The limit is per
instance, so concurrent instances may retain the sum of their individual
limits. Both CLI observations and MCP-native image responses use these backing
files, although MCP does not expose the local path. An `image_path` remains
usable only while its PNG is retained; its `frame_id` is actionable only until
that daemon instance returns a newer frame.

For an isolated `:99` desktop, install Xvfb and the bundled user units after
`scripts/install-dev`:

```sh
install -Dm644 systemd/cu-display.service \
  ~/.config/systemd/user/cu-display.service
install -Dm644 systemd/cu.service ~/.config/systemd/user/cu.service
systemctl --user daemon-reload
systemctl --user enable --now cu.service
```

The daemon unit is bound to and upheld by `cu-display.service`, so an Xvfb
restart cannot leave `cu` connected to the old X server.

Observe before acting:

```sh
# Bundled :99 service (legacy default instance)
cu observe

# A manually started named instance
cu observe --instance x11-99
```

Then pass the returned `frame_id` as `expected_frame_id`:

```sh
printf '%s\n' '{"expected_frame_id":"<frame_id from observe>","actions":[{"type":"click","x":800,"y":450}]}' \
  | cu act --request-id agent-step-1

# Or target the matching named instance
printf '%s\n' '{"expected_frame_id":"<frame_id from observe>","actions":[{"type":"click","x":800,"y":450}]}' \
  | cu act --instance x11-99 --request-id agent-step-1
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
codex mcp add cu-x11-99 -- cu mcp --instance x11-99
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
restart. If a cached result outlives its retained PNG, it returns
`image_expired: true` without an image; the action has already executed and the
caller must observe again rather than repeat it.

`settled: false` means the screen kept changing until the settle timeout; the
frame is still usable. The direct Wayland backend intentionally refuses multiple
active outputs because output-specific absolute pointer mapping is not yet
implemented.
