This private X11 desktop uses Openbox with one workspace. It lasts until this MCP process exits; switching to another instance preserves its windows.

Use Alt+Tab / Alt+Shift+Tab to switch windows, Super+Left / Super+Right to tile, Super+Up to maximize, Super+Down to restore, and Alt+F4 to close a window. Observe after changing the layout before clicking.

To launch an application from your shell, use the display and XAUTHORITY path above:
`env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DBUS_SESSION_BUS_ADDRESS DISPLAY=<display> XAUTHORITY=<path> XDG_SESSION_TYPE=x11 <application>`.
Application files and accounts belong to the same Linux user. Applications may have their own single-instance behavior. Closing the desktop does not guarantee termination of programs launched independently from your shell.
