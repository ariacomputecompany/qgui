# Contributing

This project is intentionally small and pragmatic.

Guidelines:

- Keep defaults secure (no unauthenticated VNC/noVNC by default).
- Prefer minimal dependencies and clear failure modes.
- If you add a feature that can weaken isolation (port exposure, auth bypass, etc.), it must be opt-in and documented.

