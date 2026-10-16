# Security

Aerial is a screensaver, not an authentication boundary. Use your desktop's screen locker for access control.

The downloader restricts requests and redirects to `https://sylvan.apple.com:443`, validates TLS, bounds metadata and download sizes, and writes completed cache entries through atomic renames. The bundled public Apple Root CA applies only to this downloader; it does not modify system trust.

Playback uses the user-installed mpv Flatpak. Each invocation removes network, audio, X11 and broad filesystem access, then grants GPU access, read-only access to the video cache and a private IPC directory. User mpv config, scripts and URL resolvers are disabled. Decoder vulnerabilities and compositor behavior remain part of the runtime trust boundary. Keep your system and Flatpak runtimes updated.

To report a vulnerability, use the repository's **Security → Report a vulnerability** feature. Do not include private information in a public issue. Only the current source version is maintained.
