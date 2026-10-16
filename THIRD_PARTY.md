# Third-party material

- **Aerial for macOS:** MIT, copyright Guillaume Louel and contributors. The original notice is retained in LICENSE. See docs/PORT.md for the source map.
- **Apple aerial videos and metadata:** downloaded directly from Apple's CDN for local use. They are not included as video files and are not covered by the application's MIT license. The README screenshot shows Apple video imagery.
- **Apple Root CA:** public certificate from [Apple's certificate authority page](https://www.apple.com/certificateauthority/), included to verify the aerial CDN. Provenance and digest are in assets/README.md. It is not application code.
- **mpv Flatpak:** separately installed and separately licensed. See [mpv](https://mpv.io/) and [its Flatpak packaging](https://github.com/flathub/io.mpv.Mpv).
- **Rust crates:** exact dependency versions are pinned in Cargo.lock. Their licenses and source notices remain with their upstream packages. This repository does not bundle their source or claim to relicense them.
