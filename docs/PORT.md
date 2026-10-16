# Implementation and provenance

This Rust implementation follows behavior and algorithms from [Aerial 4.1.7](https://github.com/AerialScreensaver/Aerial/tree/29a27837c09d87d7beb3326da9842557bc33f537), source commit `29a27837c09d87d7beb3326da9842557bc33f537`. The upstream MIT notice is retained.

The release archive was also inspected statically to identify the app and wallpaper extension boundaries. No macOS runtime behavior or decompiled native pseudocode is claimed. The archive SHA-256 was `25da348589b73fbcee73181e87e7a398e2152389c228f48fe364a52cd81eee51`.

| Behavior | Upstream source path |
| --- | --- |
| Apple catalog feeds | Shared/Sources/SourceList.swift, Shared/Sources/Source.swift |
| Cached playback eligibility | Shared/Cache/VideoCache.swift |
| Format preference | Shared/Core/AerialVideo.swift |
| Shuffle and repeat prevention | Shared/Playlists/PlaylistModels.swift |
| Playlist continuity | Aerial/Model/Playback/PlaylistManager.swift |
| Downloads | Shared/Downloads/VideoDownload.swift |
| Transition design reference | WallpaperExtension/Rendering/VideoTransitionCoordinator.swift |
| POI timelines | Shared/Overlays/OverlayState.swift |
| Localized strings | Shared/Cache/PoiStringProvider.swift |

Linux replaces macOS lifecycle and media APIs with native Wayland idle notifications, logind session checks and the existing mpv media stack. The four Rust modules implement catalog/cache handling, Wayland integration, watcher/CLI orchestration and playback/overlays. The application has no Python runtime.

Playback is local, silent and fullscreen. Every player is owned by the watcher and cleaned up on exit. Playlists shuffle cached eligible videos; clock positions advance on file-load events. Transitions dip through black rather than reproducing Aerial's frozen-frame crossfade.

The current implementation does not include Aerial's macOS settings application, wallpaper extension, weather/music integrations or YouTube resolvers. It does not change desktop authentication or power management.
