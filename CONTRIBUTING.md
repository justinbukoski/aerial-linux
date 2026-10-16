# Contributing

Issues and pull requests are welcome. Include your compositor, Linux distribution, mpv Flatpak version and the relevant error when reporting a problem. Redact usernames, private paths and any unrelated desktop content from logs and screenshots.

Build and run the checks in the README before opening a pull request. Keep changes focused and explain the observable behavior they fix. Tests should exercise failure cases and lifecycle guarantees where practical. Live Wayland tests need an actual desktop session; unit tests alone do not verify input dismissal or compositor integration.

Maintain the playback sandbox, verified HTTPS downloads, bounded I/O and owned-process cleanup. Do not add Apple video files, credentials or machine-specific configuration to the repository. Contributions are licensed under the project's MIT license.
