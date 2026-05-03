# qgui

`qgui` is a small GUI session manager that starts a full desktop environment inside a container and gives operators a stable app-launch contract built around KasmVNC:

- `KasmVNC` (browser-facing X/VNC backend)
- `xfce4` (desktop session)
- `dbus-daemon` (session bus)
- a persisted session contract in `/run/qgui/session.json`

The intended deployment pattern is:

1. Bake `qgui` and the GUI packages into your container image (or rootfs tarball).
2. Start the GUI stack inside the running container: `qgui up`.
3. Launch apps through the managed session: `qgui run -- <command...>`.
3. Expose the KasmVNC endpoint through your existing control-plane HTTP surface via a reverse proxy
   (recommended), rather than publishing per-container host ports.

## Quick Start (Inside Container)

```bash
qgui up
qgui status
qgui env
qgui run -- xeyes
qgui logs --component kasmvnc
qgui down
```

Defaults:

- Desktop is on `DISPLAY=:1` at `1440x900x24`
- KasmVNC listens on `0.0.0.0:6080`
- `qgui up` generates backend auth state and persists it in `/run/qgui/session.json`
- `qgui env` prints the exact session variables (`DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`)
- `qgui run -- <command...>` launches an app on the active session without manual low-level wiring
- `qgui doctor` verifies the required binaries, runtime dirs, file-descriptor limit, and active session state

## Rootfs / Image Build

This repo ships only the `qgui` binary. A host image or rootfs tarball still needs to provide:

- `kasmvncserver`
- `kasmvncpasswd`
- `xfce4-session`
- `dbus-daemon`
- `xrdb`
- `xauth`
- `xsetroot`
- `/usr/share/kasmvnc/www/index.html`

## Security Notes

- Prefer exposing GUI via an authenticated reverse proxy rather than host port publishing.
- Add TLS at your ingress or proxy layer.
- `qgui` generates backend auth material for KasmVNC and writes it into the runtime session contract.
- The supported security model is control-plane or reverse-proxy auth around the browser path, not direct public exposure of the backend port.

## License

Dual-licensed under MIT or Apache-2.0.
