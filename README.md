# qgui

`qgui` is a small GUI session manager that starts a full desktop environment *inside a container* and gives operators a stable app-launch contract:

- `Xvfb` (headless X server)
- `xfce4` (desktop session)
- `x11vnc` (VNC server)
- `noVNC` + `websockify` (browser client + WebSocket bridge)

The intended deployment pattern is:

1. Bake `qgui` and the GUI packages into your container image (or rootfs tarball).
2. Start the GUI stack inside the running container: `qgui up`.
3. Launch apps through the managed session: `qgui run -- <command...>`.
3. Expose the noVNC endpoint through your existing control-plane HTTP surface via a reverse proxy
   (recommended), rather than publishing per-container host ports.

## Quick Start (Inside Container)

```bash
qgui up
qgui status
qgui env
qgui run -- xeyes
qgui logs --component websockify
qgui down
```

Defaults:

- Desktop is on `DISPLAY=:1` at `1920x1080x24`
- VNC listens on `127.0.0.1:5901` (loopback by default)
- noVNC listens on `0.0.0.0:6080` (so an internal reverse proxy can reach it)
- `qgui env` prints the exact session variables (`DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`)
- `qgui run -- <command...>` launches an app on the active session without manual low-level wiring

## Rootfs / Image Build

This repo includes a production-oriented script to generate an Alpine-based rootfs tarball that
contains the GUI stack plus a static `qgui` binary:

```bash
./scripts/generate-alpine-gui-rootfs.sh ./qgui-alpine-gui.tar.gz
```

## Security Notes

- Prefer exposing GUI via an authenticated reverse proxy rather than host port publishing.
- noVNC terminates at `websockify` inside the container; add TLS at your ingress/proxy layer.
- The VNC endpoint is bound to loopback by default to avoid accidental direct exposure.
- The supported security model is reverse-proxy auth at the Quilt/control-plane layer, not a separate VNC password contract.

## License

Dual-licensed under MIT or Apache-2.0.
