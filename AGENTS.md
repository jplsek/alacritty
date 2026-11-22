# Agent UI Testing

Use the opt-in UI debug IPC interface to test real windows, tabs, terminal
output, and redraws without `xdotool`.

If a smoke test would require `xdotool` or a similar external UI automation tool,
add the required functionality to the `ui-debug` feature instead, and use that
interface in the test.

## Temporary Smoke Tests

Write task-specific smoke test scripts in a temporary directory, not in the
repository. Do not commit these scripts. Build the opt-in UI debug feature from
the repository root:

```sh
cargo build -p alacritty --features ui-debug
```

Use the debug IPC commands below to drive real windows and inspect state.
Keep test windows visible and floating so the window manager permits requested
resizes. Poll state for asynchronous operations and use bounded timeouts.

Use a temporary configuration and private IPC socket, disable session restore,
and avoid running the user's shell configuration. Clean up test windows,
processes, and temporary files on success or failure.

## Debug IPC Commands

The debug build exposes JSON state and actions through the normal IPC CLI:

```sh
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug state
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug create-tab
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug select-tab 0
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug resize 800 600
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug output 'test output'
target/debug/alacritty msg --socket /path/to/ipc.sock ui-debug redraw
```

When multiple windows exist, place `--window-id ID` immediately after `ui-debug`.
These commands are excluded from normal builds and from release builds, even
when the feature is enabled. Output injection accepts complete UTF-8/ANSI
sequences per request; parser state is not retained between requests.
