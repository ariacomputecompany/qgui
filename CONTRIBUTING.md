# Contributing

This project is intentionally small and pragmatic.

Guidelines:

- Keep defaults secure and fail closed around the browser-facing GUI backend.
- Prefer minimal dependencies and clear failure modes.
- If you add a feature that can weaken isolation (port exposure, auth bypass, etc.), it must be opt-in and documented.
