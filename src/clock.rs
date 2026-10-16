//! Follow COSMIC's clock preference without running a settings helper process.
use chrono::{Local, NaiveTime};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct DesktopClock {
    preference: Option<PathBuf>,
    checked_at: Option<Instant>,
    use_24h: bool,
}

impl DesktopClock {
    pub fn new() -> Self {
        let cosmic = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .any(|desktop| desktop.eq_ignore_ascii_case("COSMIC"));
        let preference = cosmic
            .then(dirs::config_dir)
            .flatten()
            .map(|root| root.join("cosmic/com.system76.CosmicAppletTime/v1/military_time"));
        Self {
            preference,
            checked_at: None,
            use_24h: false,
        }
    }

    pub fn text(&mut self) -> String {
        // Re-read while playing so a desktop setting change takes effect live.
        if self
            .checked_at
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            self.use_24h = self
                .preference
                .as_deref()
                .and_then(read_preference)
                .unwrap_or(false);
            self.checked_at = Some(Instant::now());
        }
        format_time(Local::now().time(), self.use_24h)
    }

    pub fn format_name(&self) -> &'static str {
        if self.use_24h {
            "24h"
        } else {
            "12h"
        }
    }
}

fn read_preference(path: &Path) -> Option<bool> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut value = String::new();
    file.by_ref().take(65).read_to_string(&mut value).ok()?;
    if value.len() > 64 {
        return None;
    }
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn format_time(time: NaiveTime, use_24h: bool) -> String {
    time.format(if use_24h { "%H:%M" } else { "%-I:%M %p" })
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_midnight_noon_and_evening() {
        for (hour, twelve, twenty_four) in [
            (0, "12:05 AM", "00:05"),
            (12, "12:05 PM", "12:05"),
            (21, "9:05 PM", "21:05"),
        ] {
            let time = NaiveTime::from_hms_opt(hour, 5, 0).unwrap();
            assert_eq!(format_time(time, false), twelve);
            assert_eq!(format_time(time, true), twenty_four);
        }
    }

    #[test]
    fn follows_preference_changes_and_rejects_invalid_files() {
        let temp = tempfile::tempdir().unwrap();
        let preference = temp.path().join("military_time");
        let mut clock = DesktopClock {
            preference: Some(preference.clone()),
            checked_at: None,
            use_24h: false,
        };
        for (contents, expected) in [("true\n", "24h"), ("false", "12h"), ("bad", "12h")] {
            std::fs::write(&preference, contents).unwrap();
            clock.checked_at = None;
            clock.text();
            assert_eq!(clock.format_name(), expected);
        }
        std::fs::write(&preference, " ".repeat(65)).unwrap();
        assert_eq!(read_preference(&preference), None);
        assert_eq!(read_preference(temp.path()), None);
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&preference, &link).unwrap();
        assert_eq!(read_preference(&link), None);
    }
}
