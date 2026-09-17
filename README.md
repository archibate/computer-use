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

For an agent-owned offscreen desktop, install `Xvfb` and `openbox` on `PATH`,
register `cu mcp` as described below, then call
`computer_connect({"name":"@private"})`. cu starts and owns the Xvfb/Openbox
desktop; no systemd configuration or separately started daemon is needed.

To connect to an existing desktop instead, start a daemon:

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

`computer_connect` returns a profile with metadata from the daemon's active backend:
X11 display and screen (for example, `backend=x11, display=":99", screen=0`), or
the absolute Wayland socket path and selected output. Wayland also includes
`x11_display` when the daemon's `DISPLAY` is set; this is an environment hint,
not a verified association with the same compositor. An inherited `WAYLAND_SOCKET`
connection is identified as `connection=inherited_fd` instead of a socket path.
This metadata is included even without `--profile`; it is returned on each
connection, not in per-frame tool responses.

`--profile` loads a trusted UTF-8 Markdown or text file of at most 16 KiB and
appends it after the connection metadata. This keeps desktop-specific operating
guidance, such as verified window-manager shortcuts, out of the generic tool
schemas. After changing the daemon target or profile, restart the daemon and
call `computer_connect` again to refresh both. A failed connection leaves MCP
disconnected; it never falls back to another desktop.

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

For a persistent, externally managed `:99` Xvfb server, the bundled systemd
units remain an optional alternative after `scripts/install-dev`:

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

Register the local stdio MCP server with one of these commands.

Codex:

```sh
codex mcp add cu -- cu mcp
```

Claude Code (user scope, available across projects):

```sh
claude mcp add --scope user --transport stdio cu -- cu mcp
```

It exposes four tools:

- `computer_connect` accepts one optional string `name`, defaulting to
  `"@default"`, and returns `{profile}`. `"@default"` selects the existing
  default daemon; `"@private"` creates or reuses this MCP process's session-local
  offscreen desktop; other names select existing named daemons.
- `computer_observe` returns a session-local integer `frame`, dimensions,
  settling status, and a PNG image.
- `computer_act` requires that latest `frame`, executes a validated batch, and
  returns execution metadata plus the next numbered observation and PNG.
- `computer_wait` requires that latest `frame`, watches included regions
  minus optional exclusions, and blocks by default without repeated model polling. It
  returns a flat current `frame`; only a changed result includes a PNG and
  `activity_bbox`. Optional `async:true` returns a completion-file path instead.

The published schemas describe every action, coordinate and key convention,
and settling limits. Recovery guidance arrives with the result that needs it:
errors include a diagnostic and applicable next steps, while partial or screenshot-expired
action results include a `message`. Ordinary successful action results omit it.

MCP starts disconnected. Call `computer_connect` before observing, acting, or
waiting; otherwise these tools return `not_connected`. Call with `{}` or
`{"name":"@default"}` for the default daemon, `{"name":"@private"}` for a
private desktop, or `{"name":"work"}` for an existing named daemon. The default
applies only when calling the tool; MCP startup does not connect automatically.
Switching away from a private desktop preserves it until its MCP process exits.
Literal names `default` and `private` are ordinary instance names:
`{"name":"default"}` selects `instances/default`, independently of `@default`.
Instance names use 1-64 ASCII letters, digits, dots, underscores, or hyphens;
`.` and `..` are invalid. Null or empty names, unknown `@` selectors, and extra
fields (including the former `type` and `instance` parameters) are rejected
without changing the current connection or cancelling waits.

Find candidate named instances with:

```sh
ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/computer-use/instances"
```

Directories may remain after a daemon exits; connecting checks availability.
There is no separate instance registry or list command. Select the MCP desktop
through `computer_connect`. CLI commands use their daemon targeting options:
omit `--instance` for the default daemon; an explicit `--instance NAME` always selects
`instances/NAME`, including names `default` and `private`.

Every valid connect attempt cancels this MCP's synchronous and asynchronous
waits, clears its current frame and action retry cache, and requires a fresh
observe, even when reconnecting to the same instance. Connect itself does not
capture a frame or alter the public daemon's current frame. Connection failure,
or cancellation detected during connection, leaves the MCP disconnected. A
completed connection is not undone by a late cancellation.

MCP frame numbers start at 1, increase for each distinct observation, and are
never reset when switching instances within that MCP process. Frames from a
previous connection are invalid. The adapter maps them to the daemon's opaque
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

### Private desktops

Each MCP process owns at most one private desktop: a 1280×800 Xvfb screen with
one Openbox workspace. Xvfb allocates an available display automatically, uses
a private Xauthority cookie, and disables TCP listening. Openbox loads the
built-in configuration without the user's autostart. Its shortcuts and the
display/authentication environment needed to launch applications are returned
in the connection profile. cu does not add a panel or VNC server.

Switching away preserves the desktop and its windows. Reconnecting to `private`
reuses it, or recreates it after a crash. MCP EOF, termination, or death stops
its private worker, Xvfb, and Openbox. Loss of either desktop component stops
the worker; call `computer_connect` again to recreate it. Startup failure,
timeout, and cancellation roll back a newly created desktop.

Private resources live under
`${XDG_RUNTIME_DIR:-/run/user/<uid>}/computer-use/mcp-desktops/<uuid>`, with
owner-only directories and files. Normal shutdown removes that desktop's
resources; forcibly killing its worker can leave runtime files until the next
connect or MCP exit. This is desktop isolation under the same Linux user, not
a container: applications share the user's files and accounts and may have
their own single-instance behavior. cu does not guarantee termination of
applications launched independently from an external shell.

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

The tool immediately returns `{"status":"started","signal_path":"...","message":"..."}`
with instructions to monitor the file and read its result.
This means the MCP process accepted the background task; daemon rejection
(including `busy`), connection failure, or invalid regions can arrive in the
result file. Omitting `async` or setting it to `false` preserves the blocking
behavior. The CLI and daemon protocol remain synchronous.

The MCP process holds the existing daemon wait connection until completion.
There is still one active wait per daemon, and a second pending async wait in
the same MCP process returns `busy`. `computer_observe` cancels a pending async
wait before requesting its screenshot. A new action that commits a new frame
also cancels it; validation failures and cached action replays do not. Other
clients' baseline supersession produces `cancelled`. `computer_connect` also
cancels pending waits, including when the daemon frame has not changed. There
is no cancel tool.

The private final JSON file appears atomically, with one terminal status:

| Status | Additional fields | External watcher |
| --- | --- | --- |
| `changed` | `elapsed_ms`, `activity_bbox`, `settled`, `message` | Wake once |
| `timeout` | `elapsed_ms` | Wake once |
| `error` | `elapsed_ms`, `code`, `message` | Wake once |
| `cancelled` | `elapsed_ms`, `message` | Wake once |

Notify once when the result file appears, including for `cancelled`. Cancellation
ends the wait and must remain visible to a monitor that is still armed. Read the
JSON status to distinguish cancellation, timeout, errors, and detected activity.
Check immediately, then poll once per second with a bounded detector lifetime.
An event-based detector must register a watch on the containing directory
**before** checking for an existing file, so completion during setup is not lost.
If the session directory disappears or the detector's deadline expires without
a result, notify once that monitoring failed. A detector deadline is not proof
that the wait completed. Notify from these explicit branches; stopping the
monitor itself should not trigger an exit-handler notification. cu neither
executes callbacks nor calls Codex; an external watcher such as `monitor-wakeup`
owns any continuation.

After waking, read the result and call `computer_observe` for a fresh screenshot
and frame. Remove the result file after handling it. The file
contains notification metadata only, without a frame number or image path.
Changed results clear the old MCP frame binding; unchanged timeouts preserve it.
If completion wins the race with cancellation, its published result remains;
an already-issued wake-up cannot be withdrawn.

Paths are unique per wait under
`$XDG_RUNTIME_DIR/computer-use/mcp-waits/<session>/<wait>.json`, using the usual
`/run/user/<uid>` fallback. Directories are `0700` and files `0600`. Published
results survive new waits and MCP process exit so late monitors can read them.
Consumers remove handled files; unread files and nonempty session directories
remain until runtime-directory cleanup. There is no automatic result eviction
or retention cap. MCP EOF, SIGTERM, or SIGINT publishes `cancelled` for a pending
wait, stops its worker, and removes an empty session directory. A previously
published terminal result is preserved. Daemon loss while MCP remains alive
produces `error`. SIGKILL or a crash cannot guarantee cleanup or a terminal file.
A publication failure is logged, releases the task, and leaves no partial final
JSON; the monitor's deadline makes a missing result visible.

Repeated changed results with unexpectedly small `elapsed_ms` indicate an
active included region. Narrow the includes or exclude blinking cursors and
unrelated animation before re-arming.

The engine rejects stale frames before input, validates the complete batch
before its first side effect, executes actions in order, and captures state
after success or partial execution. Reusing a CLI `--request-id`, or retrying
the same MCP request within one connection, returns the cached action result
instead of executing twice. This idempotency cache does not survive a daemon
restart. If a cached result outlives its retained PNG, it returns
`image_expired: true` without an image; the action has already executed and the
caller must observe again rather than repeat it.

`settled: false` means the screen kept changing until the settle timeout; the
frame is still usable. The direct Wayland backend intentionally refuses multiple
active outputs because output-specific absolute pointer mapping is not yet
implemented.
