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
cu daemon --backend x11 --display :99 --profile ~/.config/codex-agent-desktop/profile.md

# Independent daemons for two X11 displays
cu daemon --instance x11-99 --backend x11 --display :99
cu daemon --instance x11-100 --backend x11 --display :100
```

Automatic mode uses `XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`, and `DISPLAY`. A
Wayland session selects the direct Wayland backend even when XWayland also sets
`DISPLAY`; an X11 session selects the X11 backend. `--display` selects X11 in
automatic mode, while `--output` selects direct Wayland. `--max-width` and
`--max-height` add optional downscaling limits for either backend.

`--profile` loads a trusted UTF-8 Markdown or text file of at most 16 KiB. The
daemon serves that text to MCP adapters, which append it once to their
initialization instructions. This keeps desktop-specific operating guidance,
such as verified window-manager shortcuts, out of the generic tool schemas and
per-frame responses. After changing the file, restart the daemon and then the
MCP connection so both reload it. If the daemon or profile is unavailable when
MCP starts, that connection keeps the generic instructions until it reconnects.

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

To wait for screen activity without repeatedly capturing in an agent loop,
first observe and retain its `frame_id`, then start a bounded wait:

```sh
cu wait --last-frame-id "<frame_id from cu observe>" \
  --include 780,120,800,720 \
  --include 40,120,700,720 \
  --exclude 1490,120,80,30 \
  --timeout-ms 3600000 \
  --quiet-ms 30000
```

Repeat `--include` for 1-8 regions and `--exclude` for up to 8 carets,
animations, clocks, or other known noise. Comparison is exact RGB over the
union of included rectangles minus the union of exclusions. Rectangles use
baseline-frame `x,y,width,height` coordinates.

`timeout_ms` is the total budget. After the first change, another monitored
pixel change restarts `quiet_ms`. If the deadline arrives before a full quiet
period, the latest changed frame is returned with `settled:false`. Defaults are
3600000 ms (1 hour) total and 2000 ms quiet. The daemon samples every 500 ms
internally.

An unchanged timeout returns `status:"timeout"` and `elapsed_ms`, without an
image or frame-store publication; re-arm it with the same baseline. Activity
returns `status:"changed"`, the accumulated `activity_bbox`, and a fresh
observation. A viewport-size change returns
`viewport_changed` with expected and actual dimensions and requires another
observe. Any concurrent observe, act, or changed wait supersedes the baseline
and makes the wait stale. One wait may run at a time; another returns `busy`.
Dropping the CLI process cancels its daemon wait.

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

It exposes three tools:

- `computer_observe` returns a session-local integer `frame`, dimensions,
  settling status, and a PNG image.
- `computer_act` requires that latest `frame`, executes a validated batch, and
  returns execution metadata plus the next numbered observation and PNG.
- `computer_wait` requires that latest `frame`, watches included regions
  minus optional exclusions, and blocks by default without repeated model polling. It
  returns a flat current `frame`; only a changed result includes a PNG and
  `activity_bbox`. Optional `async:true` returns a completion-file path instead.

The published schemas describe every action, coordinate and key convention,
settling limits, partial execution, and stale-frame recovery.

MCP frame numbers start at 1, increase for each distinct observation, and are
valid only within that MCP process. The adapter maps them to the daemon's opaque
frame IDs, which remain part of the CLI and daemon protocols but are not exposed
to the agent. Re-arm an unchanged timeout with the same frame; use the fresh
frame after a changed result. Call `computer_observe` after `stale_frame`,
cancellation, or `viewport_changed`.

`computer_wait` emits rate-limited progress notifications when the MCP client
supplies a progress token and propagates cancellation by dropping the daemon
connection. Its duration is `timeout_ms` plus terminal capture and PNG encoding.
Configure the MCP host deadline above that bound. This Codex configuration
allows a full one-hour wait, including capture and encoding:

```toml
[mcp_servers.cu]
command = "cu"
args = ["mcp"]
tool_timeout_sec = 100000
```

### Asynchronous MCP waits

For longer waits, prefer async mode with a monitor tool when one is available.
The completed JSON at `signal_path` is a Bash-visible condition: use Claude
Code's Monitor tool or the curated `monitor-wakeup` skill in Codex to watch its
terminal status and notify the agent. Once the monitor is armed, continue
unrelated work or end the turn and stay idle until notified. The wait keeps
running while its MCP process stays alive.

Pass `"async":true` with the same frame, regions, and timing parameters:

```json
{"frame":1,"include_rects":[{"x":780,"y":120,"width":800,"height":720}],"async":true}
```

The tool immediately returns `{"status":"started","signal_path":"..."}`.
This means the MCP process accepted the background task; daemon rejection
(including `busy`), connection failure, or invalid regions can arrive in the
result file. Omitting `async` or setting it to `false` preserves the blocking
behavior. The CLI and daemon protocol remain synchronous.

The MCP process holds the existing daemon wait connection until completion.
There is still one active wait per daemon, and a second pending async wait in
the same MCP process returns `busy`. `computer_observe` cancels a pending async
wait before requesting its screenshot. A new action that commits a new frame
also cancels it; validation failures and cached action replays do not. Other
clients' baseline supersession produces `cancelled`. There is no cancel tool.

The private final JSON file appears atomically, with one terminal status:

| Status | Additional fields | External watcher |
| --- | --- | --- |
| `changed` | `elapsed_ms`, `activity_bbox`, `settled` | Wake once |
| `timeout` | `elapsed_ms` | Wake once |
| `error` | `elapsed_ms`, `code`, `message` | Wake once |
| `cancelled` | `elapsed_ms` | Exit silently |

Read the status before waking: file existence alone also detects cancellation.
Check immediately, then poll once per second with a bounded detector lifetime.
An event-based detector must register a watch on the containing directory
**before** checking for an existing file, so completion during setup is not lost.
Exit silently if the session directory disappears or the detector's own deadline
expires without a result. Never notify from an exit or cleanup handler. A wait
timeout is a published `timeout` result; a detector deadline is not proof that
the wait completed. cu neither executes callbacks nor calls Codex; an external
watcher such as `monitor-wakeup` owns any continuation.

After waking, call `computer_observe` for a fresh screenshot and frame. The file
contains notification metadata only, without a frame number or image path.
Changed results clear the old MCP frame binding; unchanged timeouts preserve it.
If completion wins the race with cancellation, its published result remains;
an already-issued wake-up cannot be withdrawn.

Paths are unique per wait under
`$XDG_RUNTIME_DIR/computer-use/mcp-waits/<session>/<wait>.json`, using the usual
`/run/user/<uid>` fallback. Directories are `0700` and files `0600`. Each MCP
process retains at most its latest result, deleting it on the next accepted
async start. MCP EOF, SIGTERM, or SIGINT cancels the worker and removes its private
session directory. Daemon loss while MCP remains alive produces `error`.
SIGKILL or a crash cannot guarantee cleanup or a terminal file; remaining files
last until runtime-directory cleanup. A publication failure is logged, releases
the task, and leaves no partial final JSON; the detector's bound covers it.

Repeated changed results with unexpectedly small `elapsed_ms` indicate an
active included region. Narrow the includes or exclude blinking cursors and
unrelated animation before re-arming.

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
