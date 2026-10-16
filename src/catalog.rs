//! Apple aerial catalog, local video cache, and timeline captions.
//!
//! Behavior reference: Aerial 4.1.7 commit
//! 29a27837c09d87d7beb3326da9842557bc33f537, MIT licensed.
//! Source.swift:388-446 (asset schema), AerialVideo.swift:100-122
//! (format selection), VideoCache.swift:38-104 (offline availability), and
//! OverlayState.swift:426-497 (POI timelines). This is a Linux implementation;
//! no macOS code, Python process, or optional live-feed resolver is required.

use anyhow::{bail, ensure, Context, Result};
use rand::{seq::IteratorRandom, Rng};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const FEED_URL: &str = "https://sylvan.apple.com/Aerials/resources-16.tar";
const MAX_FEED_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MEMBER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MIN_VIDEO_BYTES: u64 = 500_000;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    #[default]
    Sdr4k,
    Hd1080,
    Hd1080Sdr,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sdr4k => "4k-sdr",
            Self::Hd1080 => "1080p-h264",
            Self::Hd1080Sdr => "1080p-sdr",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poi {
    pub seconds: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    pub id: String,
    pub name: String,
    pub url: String,
    pub resolution: Resolution,
    #[serde(default)]
    pub poi: Vec<Poi>,
}

impl Video {
    /// The caption follows media position, including seeks and loop rewind.
    pub fn caption_at(&self, seconds: f64) -> &str {
        self.poi
            .iter()
            .rev()
            .find(|p| p.seconds <= seconds)
            .map(|p| p.text.as_str())
            .unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub videos: Vec<Video>,
}

#[derive(Deserialize)]
struct Feed {
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    id: String,
    #[serde(rename = "accessibilityLabel")]
    name: String,
    #[serde(default, rename = "pointsOfInterest")]
    poi: HashMap<String, String>,
    #[serde(default, rename = "url-4K-SDR")]
    sdr4k: Option<String>,
    #[serde(default, rename = "url-1080-H264")]
    h264: Option<String>,
    #[serde(default, rename = "url-1080-SDR")]
    hd_sdr: Option<String>,
}

impl Catalog {
    /// Refresh the small Apple feed and persist `root/catalog.json`.
    /// Downloads only metadata; video downloads are explicit and lazy.
    pub fn refresh(root: &Path) -> Result<Self> {
        Self::refresh_with_resolution(root, Resolution::default())
    }

    pub fn refresh_with_resolution(root: &Path, wanted: Resolution) -> Result<Self> {
        let client = apple_client(Duration::from_secs(60))?;
        let mut response = client.get(FEED_URL).send()?.error_for_status()?;
        let bytes = read_bounded(&mut response, MAX_FEED_BYTES)?;
        let catalog = Self::from_archive(&bytes, wanted)?;
        catalog.save(&root.join("catalog.json"))?;
        Ok(catalog)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut file = open_regular(path)?;
        let bytes = read_bounded(&mut file, MAX_FEED_BYTES)?;
        let mut catalog: Self = serde_json::from_slice(&bytes).context("decode cached catalog")?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        atomic_write(path, &bytes)
    }

    /// Avoid an immediate repeat when at least two distinct assets exist.
    /// A singleton remains playable; callers own longer shuffle/history policy.
    pub fn choose(&self, exclude_id: Option<&str>) -> Option<&Video> {
        self.videos
            .iter()
            .filter(|v| Some(v.id.as_str()) != exclude_id)
            .choose(&mut rand::thread_rng())
            .or_else(|| self.videos.first())
    }

    /// Cache paths include actual format so switching 1080p/4K never reuses
    /// the other resolution. IDs and URLs are validated before filesystem use.
    pub fn cache_path(root: &Path, video: &Video) -> Result<PathBuf> {
        validate_id(&video.id)?;
        validate_apple_url(&video.url)?;
        Ok(root
            .join("videos")
            .join(format!("{}-{}.mov", video.id, video.resolution.label())))
    }

    pub fn cached_video(root: &Path, video: &Video) -> Result<Option<PathBuf>> {
        let path = Self::cache_path(root, video)?;
        Ok(valid_movie(&path)?.then_some(path))
    }

    pub fn cache_video(&self, video: &Video, root: &Path) -> Result<PathBuf> {
        let path = Self::cache_path(root, video)?;
        if valid_movie(&path)? {
            return Ok(path);
        }
        fs::create_dir_all(path.parent().context("cache parent")?)?;
        let (mut file, partial) = create_partial(&path)?;
        let client = apple_client(Duration::from_secs(60 * 30))?;
        let mut response = client.get(&video.url).send()?.error_for_status()?;
        if let Some(length) = response.content_length() {
            ensure!(length <= MAX_VIDEO_BYTES, "video exceeds 4 GiB limit");
            ensure!(length > MIN_VIDEO_BYTES, "video response is too small");
        }
        let expected = response.content_length();
        let copied = io::copy(&mut response.by_ref().take(MAX_VIDEO_BYTES + 1), &mut file)?;
        ensure!(copied <= MAX_VIDEO_BYTES, "video exceeds 4 GiB limit");
        if let Some(length) = expected {
            ensure!(copied == length, "incomplete video download");
        }
        file.sync_all()?;
        drop(file);
        ensure!(
            valid_movie(&partial.0)?,
            "download failed the movie container header/size check"
        );
        fs::rename(&partial.0, &path).context("publish cached video")?;
        Ok(path)
    }

    fn from_archive(bytes: &[u8], wanted: Resolution) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_FEED_BYTES,
            "feed exceeds size limit"
        );
        let mut archive = tar::Archive::new(Cursor::new(bytes));
        let mut entries_json = None;
        let mut strings = HashMap::new();
        // Read two exact members in memory. Never unpack archive-controlled
        // paths or accept symlinks/hardlinks, even under a staging directory.
        for entry in archive.entries().context("read Apple feed archive")? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            ensure!(
                path.components().all(|c| matches!(c, Component::Normal(_))),
                "unsafe path in feed archive"
            );
            if !entry.header().entry_type().is_file() {
                continue;
            }
            if path == Path::new("entries.json") {
                ensure!(entries_json.is_none(), "duplicate entries.json in feed");
                entries_json = Some(read_bounded(&mut entry, MAX_MEMBER_BYTES)?);
            } else if path
                == Path::new("TVIdleScreenStrings.bundle/en.lproj/Localizable.nocache.strings")
            {
                let data = read_bounded(&mut entry, MAX_MEMBER_BYTES)?;
                let plist = plist::Value::from_reader(Cursor::new(data))
                    .context("decode Apple English caption strings")?;
                if let Some(dictionary) = plist.as_dictionary() {
                    strings.extend(dictionary.iter().filter_map(|(key, value)| {
                        value
                            .as_string()
                            .map(|text| (key.clone(), text.to_string()))
                    }));
                }
            }
        }
        let feed: Feed =
            serde_json::from_slice(&entries_json.context("feed missing entries.json")?)?;
        let mut seen = HashSet::new();
        let mut videos = Vec::new();
        for asset in feed.assets {
            validate_id(&asset.id)?;
            if !seen.insert(asset.id.clone()) {
                continue;
            }
            let formats = match wanted {
                Resolution::Sdr4k => [
                    (asset.sdr4k.as_deref(), Resolution::Sdr4k),
                    (asset.h264.as_deref(), Resolution::Hd1080),
                    (asset.hd_sdr.as_deref(), Resolution::Hd1080Sdr),
                ],
                Resolution::Hd1080 => [
                    (asset.h264.as_deref(), Resolution::Hd1080),
                    (asset.hd_sdr.as_deref(), Resolution::Hd1080Sdr),
                    (asset.sdr4k.as_deref(), Resolution::Sdr4k),
                ],
                Resolution::Hd1080Sdr => [
                    (asset.hd_sdr.as_deref(), Resolution::Hd1080Sdr),
                    (asset.h264.as_deref(), Resolution::Hd1080),
                    (asset.sdr4k.as_deref(), Resolution::Sdr4k),
                ],
            };
            let Some((url, resolution)) = formats.into_iter().find_map(|(url, resolution)| {
                url.filter(|url| !url.is_empty())
                    .map(|url| (url.to_owned(), resolution))
            }) else {
                continue;
            };
            validate_apple_url(&url)?;
            let mut poi: Vec<Poi> = asset
                .poi
                .into_iter()
                .filter_map(|(time, key)| {
                    let seconds: f64 = time.parse().ok()?;
                    if !seconds.is_finite() || seconds < 0.0 {
                        return None;
                    }
                    // Omit unresolved Apple keys rather than display internal IDs.
                    strings.get(&key).map(|text| Poi {
                        seconds,
                        text: text.clone(),
                    })
                })
                .collect();
            poi.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
            videos.push(Video {
                id: asset.id,
                name: asset.name,
                url,
                resolution,
                poi,
            });
        }
        let mut catalog = Self { videos };
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&mut self) -> Result<()> {
        ensure!(!self.videos.is_empty(), "catalog has no playable aerials");
        let mut ids = HashSet::new();
        for video in &mut self.videos {
            validate_id(&video.id)?;
            validate_apple_url(&video.url)?;
            ensure!(ids.insert(&video.id), "duplicate asset ID in catalog");
            ensure!(!video.name.trim().is_empty(), "asset is missing its name");
            ensure!(
                video
                    .poi
                    .iter()
                    .all(|p| p.seconds.is_finite() && p.seconds >= 0.0),
                "invalid POI timestamp"
            );
            video.poi.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "unsafe asset ID"
    );
    Ok(())
}

fn validate_apple_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid Apple asset URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("sylvan.apple.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none(),
        "asset must use HTTPS on sylvan.apple.com"
    );
    Ok(())
}

fn apple_client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        // sylvan.apple.com chains to Apple's platform root, absent from the
        // Mozilla roots on Linux. This public root is bundled from Apple's PKI
        // over verified HTTPS, not installed in the system trust store. Every
        // request/redirect is separately restricted to the exact Apple host.
        .add_root_certificate(reqwest::Certificate::from_der(include_bytes!(
            "../assets/apple-root-ca.cer"
        ))?)
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .user_agent(concat!("aerial-linux/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if validate_apple_url(attempt.url().as_str()).is_err() {
                attempt.error("redirect leaves trusted Apple HTTPS host")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn read_bounded(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "download/member exceeds size limit"
    );
    Ok(bytes)
}

/// Lightweight container gate, not a codec/decode verification. The renderer
/// must still handle an unplayable movie without a tight retry loop.
fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "cache entry must be a regular file"
    );
    Ok(file)
}

fn valid_movie(path: &Path) -> Result<bool> {
    let mut file = match open_regular(path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|io| io.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() <= MIN_VIDEO_BYTES || metadata.len() > MAX_VIDEO_BYTES
    {
        return Ok(false);
    }
    let mut header = [0_u8; 8];
    file.read_exact(&mut header)?;
    Ok(matches!(
        &header[4..8],
        b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free"
    ))
}

struct PartialFile(PathBuf);
impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_partial(destination: &Path) -> Result<(File, PartialFile)> {
    let parent = destination.parent().context("destination has no parent")?;
    for _ in 0..8 {
        let path = parent.join(format!(
            ".aerial-{:016x}.part",
            rand::thread_rng().gen::<u64>()
        ));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => return Ok((file, PartialFile(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not allocate unique cache partial file")
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(destination.parent().context("destination has no parent")?)?;
    let (mut file, partial) = create_partial(destination)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&partial.0, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str) -> Video {
        Video {
            id: id.into(),
            name: "Coast".into(),
            url: "https://sylvan.apple.com/coast.mov".into(),
            resolution: Resolution::Sdr4k,
            poi: vec![],
        }
    }

    fn feed_archive(json: &str, strings: Option<&[u8]>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, data).unwrap();
        };
        add("entries.json", json.as_bytes());
        if let Some(bytes) = strings {
            add(
                "TVIdleScreenStrings.bundle/en.lproj/Localizable.nocache.strings",
                bytes,
            );
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn hostile_ids_urls_and_oversized_members_are_rejected() {
        for id in ["../escape", "/abs", "x/y", "", ".", "a.b"] {
            assert!(validate_id(id).is_err());
        }
        for url in [
            "http://sylvan.apple.com/a",
            "https://sylvan.apple.com.evil.test/a",
            "https://user@sylvan.apple.com/a",
            "https://sylvan.apple.com:8443/a",
            "file:///tmp/a",
        ] {
            assert!(validate_apple_url(url).is_err(), "{url}");
        }
        assert!(read_bounded(&mut Cursor::new(vec![0; 11]), 10).is_err());
    }

    #[test]
    fn localized_pois_sorted_and_rewind_with_media_position() {
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("early".into(), plist::Value::String("Coastline".into()));
        dictionary.insert("late".into(), plist::Value::String("Harbor".into()));
        let mut strings = Vec::new();
        plist::Value::Dictionary(dictionary)
            .to_writer_binary(&mut strings)
            .unwrap();
        let json = r#"{"assets":[{"id":"A1","accessibilityLabel":"Coast","url-4K-SDR":"https://sylvan.apple.com/a.mov","pointsOfInterest":{"20":"late","0":"early","bad":"early","-1":"early","NaN":"early","30":"missing"}}]}"#;
        let catalog =
            Catalog::from_archive(&feed_archive(json, Some(&strings)), Resolution::Sdr4k).unwrap();
        let v = &catalog.videos[0];
        assert_eq!(v.poi.len(), 2);
        assert_eq!(v.caption_at(20.0), "Harbor");
        assert_eq!(v.caption_at(19.9), "Coastline");
        assert_eq!(v.caption_at(0.0), "Coastline");
    }

    #[test]
    fn missing_format_falls_back_and_cache_paths_differ_by_resolution() {
        let json = r#"{"assets":[{"id":"A1","accessibilityLabel":"Coast","url-1080-H264":"https://sylvan.apple.com/a.mov"}]}"#;
        let catalog = Catalog::from_archive(&feed_archive(json, None), Resolution::Sdr4k).unwrap();
        assert_eq!(catalog.videos[0].resolution, Resolution::Hd1080);
        assert_eq!(catalog.videos[0].caption_at(20.0), "Coast");
        let mut v = video("A1");
        let a = Catalog::cache_path(Path::new("/tmp/cache"), &v).unwrap();
        v.resolution = Resolution::Hd1080;
        assert_ne!(a, Catalog::cache_path(Path::new("/tmp/cache"), &v).unwrap());
    }

    #[test]
    fn immediate_repeat_avoided_but_singleton_stays_playable() {
        let catalog = Catalog {
            videos: vec![video("A1"), video("A2")],
        };
        for _ in 0..64 {
            assert_eq!(catalog.choose(Some("A1")).unwrap().id, "A2");
        }
        assert_eq!(
            Catalog {
                videos: vec![video("A1")]
            }
            .choose(Some("A1"))
            .unwrap()
            .id,
            "A1"
        );
        assert!(Catalog { videos: vec![] }.choose(None).is_none());
    }

    #[test]
    fn catalog_write_roundtrip_and_corrupt_movie_gate() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = Catalog {
            videos: vec![video("A1")],
        };
        let path = temp.path().join("catalog.json");
        catalog.save(&path).unwrap();
        assert_eq!(Catalog::load(&path).unwrap().videos.len(), 1);
        let movie = temp.path().join("movie.mov");
        fs::write(&movie, b"<html>error</html>").unwrap();
        assert!(!valid_movie(&movie).unwrap());
        let mut bytes = vec![0; MIN_VIDEO_BYTES as usize + 1];
        bytes[4..8].copy_from_slice(b"ftyp");
        fs::write(&movie, bytes).unwrap();
        assert!(valid_movie(&movie).unwrap());
    }
    #[test]
    fn hd_formats_have_distinct_cache_keys_and_intro_uses_asset_name() {
        let mut v = video("A1");
        v.resolution = Resolution::Hd1080;
        let h264 = Catalog::cache_path(Path::new("/cache"), &v).unwrap();
        v.resolution = Resolution::Hd1080Sdr;
        assert_ne!(h264, Catalog::cache_path(Path::new("/cache"), &v).unwrap());
        v.poi = vec![Poi {
            seconds: 10.0,
            text: "Later".into(),
        }];
        assert_eq!(v.caption_at(0.0), "Coast");
        assert_eq!(v.caption_at(10.0), "Later");
    }
    #[test]
    fn cache_reads_reject_symlinks_and_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("catalog.json");
        fs::write(&file, "{}").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_regular(&link).is_err());
        assert!(open_regular(temp.path()).is_err());
        assert!(!valid_movie(&temp.path().join("missing.mov")).unwrap());
    }
}
