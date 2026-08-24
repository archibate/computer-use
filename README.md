# cu

`cu` is a local, agent-facing computer-use service for niri. One daemon owns
screen capture and input injection; the CLI and MCP server talk to it over a
private Unix socket.

## Build and run

Requirements: Rust 1.88+, niri, and one active Wayland output exposing wlr
screencopy, virtual pointer, and virtual keyboard protocols.

```sh
cargo build --release
target/release/cu daemon
```

The daemon detects the sole active niri output and captures at its native size.
`--output` can require a specific output; `--max-width` and `--max-height` add
optional downscaling limits. By default, the socket and retained frames live
under `$XDG_RUNTIME_DIR/computer-use` with owner-only permissions.

Observe before acting:

```sh
target/release/cu observe
```

Then pass the returned `frame_id` as `expected_frame_id`:

```sh
printf '%s\n' '{"expected_frame_id":"<frame_id from observe>","actions":[{"type":"click","x":800,"y":450}]}' \
  | target/release/cu act --request-id agent-step-1
```

Coordinates are pixels in the returned, possibly downscaled frame. Supported
actions are `move`, `click`, `double_click`, `drag`, `scroll`, `type`, and
`keypress`. A batch contains at most 16 actions.

## MCP

Start the daemon first, then configure an MCP stdio server with:

```json
{
  "command": "/absolute/path/to/target/release/cu",
  "args": ["mcp"]
}
```

It exposes two tools:

- `computer_observe` returns frame metadata and a PNG image.
- `computer_act` requires the latest `frame_id`, executes a validated batch,
  and returns the resulting metadata and PNG.

The engine rejects stale frames before input, validates the complete batch
before its first side effect, executes actions in order, and captures state
after success or partial execution. Reusing a CLI `--request-id`, or retrying
the same MCP request within one MCP session, returns the cached action result
instead of executing twice. This idempotency cache does not survive a daemon
restart.

`settled: false` means the screen kept changing until the settle timeout; the
frame is still usable. The current niri backend intentionally refuses multiple
active outputs because output-specific absolute pointer mapping is not yet
implemented.
