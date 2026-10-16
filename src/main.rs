mod catalog;
mod clock;
mod idle;
mod playback;

use anyhow::{bail, Context, Result};
use catalog::{Catalog, Resolution, Video};
use clap::{Parser, Subcommand};
use idle::IdleEvent;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::OwnedObjectPath,
};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(Parser)]
#[command(
    version,
    about = "Aerial screensaver for Linux — native Rust, offline aerial videos"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download Apple's current small catalog (no movies).
    Refresh {
        #[arg(long)]
        hd: bool,
    },
    /// List catalog videos and cached status; optionally match a location.
    List { search: Option<String> },
    /// Cache a bounded selection of movies for offline playback.
    Download {
        #[arg(long, default_value_t = 6)]
        count: usize,
        #[arg(long)]
        search: Option<String>,
    },
    /// Watch native Wayland idle events, showing videos after five minutes.
    Watch {
        #[arg(long)]
        idle_seconds: Option<u64>,
    },
    /// Show a short preview, without changing the idle or locking settings.
    Preview {
        #[arg(long, default_value_t = 20)]
        seconds: u64,
    },
    /// Dismiss the current video.
    Hide,
    /// Print daemon and video-decoder status.
    Status,
    /// Stop the watcher and only the players it owns.
    Quit,
    /// Set up desktop login startup and a preview launcher.
    InstallAutostart,
    /// Remove login startup and stop this screensaver.
    Disable,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    idle_seconds: u64,
    clock: bool,
    captions: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            idle_seconds: 300,
            clock: true,
            captions: false,
        }
    }
}

struct Paths {
    cache: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    state: PathBuf,
}
impl Paths {
    fn new() -> Result<Self> {
        let cache = dirs::cache_dir()
            .context("No user cache directory")?
            .join("aerial-linux");
        let runtime = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").context("No desktop runtime directory")?,
        )
        .join("aerial-linux");
        let config = dirs::config_dir()
            .context("No user config directory")?
            .join("aerial-linux/config.json");
        let state = dirs::state_dir()
            .context("No user state directory")?
            .join("aerial-linux");
        Ok(Self {
            cache,
            runtime,
            config,
            state,
        })
    }
    fn catalog(&self) -> Result<Catalog> {
        Catalog::load(&self.cache.join("catalog.json"))
    }
    fn config(&self) -> Result<Config> {
        if self.config.exists() {
            Ok(serde_json::from_slice(&fs::read(&self.config)?)?)
        } else {
            Ok(Config::default())
        }
    }
    fn socket(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }
}

struct Session {
    bus: Connection,
    path: OwnedObjectPath,
}
impl Session {
    fn display() -> Result<Self> {
        let bus = Connection::system()?;
        let manager = Proxy::new(
            &bus,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )?;
        // SAFETY: getuid takes no pointers and reads this process's UID.
        let user_path: OwnedObjectPath = manager.call("GetUser", &(unsafe { libc::getuid() },))?;
        let user = Proxy::new(
            &bus,
            "org.freedesktop.login1",
            user_path.as_str(),
            "org.freedesktop.login1.User",
        )?;
        let (name, path): (String, OwnedObjectPath) = user.get_property("Display")?;
        if name.is_empty() {
            bail!("No display session")
        }
        drop(user);
        drop(manager);
        Ok(Self { bus, path })
    }
    fn allowed(&self) -> Result<bool> {
        let proxy = Proxy::new(
            &self.bus,
            "org.freedesktop.login1",
            self.path.as_str(),
            "org.freedesktop.login1.Session",
        )?;
        let active: bool = proxy.get_property("Active")?;
        let locked: bool = proxy.get_property("LockedHint")?;
        Ok(active && !locked)
    }
}

fn control(paths: &Paths, command: &str) -> Result<Value> {
    let mut stream =
        UnixStream::connect(paths.socket()).context("Aerial watcher is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    writeln!(stream, "{command}")?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(128 * 1024)
        .read_line(&mut response)?;
    let response: Value = serde_json::from_str(&response)?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        bail!("Watcher rejected command: {error}")
    }
    Ok(response)
}

fn watcher_absent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    })
}

fn validated_command(command: &str) -> Result<String> {
    let words: Vec<_> = command.split_whitespace().collect();
    match words.as_slice() {
        ["show" | "hide" | "quit" | "status"] => Ok(words[0].to_owned()),
        ["preview", seconds] => Ok(format!("preview {}", seconds.parse::<u64>()?.clamp(1, 600))),
        _ => bail!("Unknown or empty control command"),
    }
}

fn cached(paths: &Paths, catalog: &Catalog) -> Vec<(Video, PathBuf)> {
    catalog
        .videos
        .iter()
        .filter_map(|video| {
            let path = Catalog::cached_video(&paths.cache, video).ok()??;
            Some((video.clone(), path))
        })
        .collect()
}

fn log(file: &mut File, message: impl std::fmt::Display) {
    let _ = writeln!(file, "{} {message}", chrono::Local::now().to_rfc3339());
}

fn watch(paths: &Paths, idle_seconds: u64, preview: Option<u64>) -> Result<()> {
    if idle_seconds == 0 {
        bail!("Idle timeout must be positive")
    }
    fs::create_dir_all(&paths.runtime)?;
    fs::set_permissions(&paths.runtime, fs::Permissions::from_mode(0o700))?;
    fs::create_dir_all(&paths.state)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.runtime.join("watcher.lock"))?;
    // SAFETY: lock owns a live descriptor; flock borrows it without taking ownership.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Aerial watcher is already running")
    }
    let mut logger = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.state.join("watcher.log"))?;
    let catalog = paths.catalog()?;
    let mut videos = cached(paths, &catalog);
    if videos.is_empty() {
        bail!("No cached aerials. Run aerial-linux refresh, then aerial-linux download")
    }
    let session = Session::display()?;
    let config = paths.config()?;
    let socket_path = paths.socket();
    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let idle_thread = idle::spawn_idle(Duration::from_secs(idle_seconds), tx, shutdown.clone())?;
    // SAFETY: the C ABI handlers only store to a lock-free atomic flag. Their
    // lifetime is static and the cast matches Linux's signal-handler ABI.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, stop_signal as *const () as libc::sighandler_t);
    }
    let mut players: Option<playback::PlayerGroup> = None;
    let mut deadline: Option<Instant> = None;
    let mut last_session_check = Instant::now();
    let mut pending = preview.map(|seconds| format!("preview {}", seconds.clamp(1, 600)));
    let mut last_first: Option<String> = None;
    log(
        &mut logger,
        format!(
            "watching idle_seconds={idle_seconds} cached={}",
            videos.len()
        ),
    );
    let outcome = (|| -> Result<()> {
        loop {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            while let Ok(event) = rx.try_recv() {
                match event {
                    IdleEvent::Idle if preview.is_none() => pending = Some("show".into()),
                    IdleEvent::Resumed => {
                        pending = None;
                        players.take();
                        deadline = None;
                        log(&mut logger, "dismissed activity");
                    }
                    IdleEvent::Unavailable(error) => {
                        bail!("Native idle monitor unavailable: {error}")
                    }
                    _ => {}
                }
            }
            let mut client = None;
            let mut client_error = None;
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
                    let mut command = String::new();
                    match BufReader::new(stream.try_clone()?)
                        .take(256)
                        .read_line(&mut command)
                    {
                        Ok(_) => match validated_command(&command) {
                            Ok(command) if command != "status" => pending = Some(command),
                            Ok(_) => {} // Status must not cancel a queued idle activation.
                            Err(error) => client_error = Some(error.to_string()),
                        },
                        Err(error) => client_error = Some(error.to_string()),
                    }
                    client = Some(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            let mut quitting = false;
            if let Some(command) = pending.take() {
                if command == "hide" {
                    players.take();
                    deadline = None;
                } else if command == "quit" {
                    quitting = true;
                } else if command == "show" || command.starts_with("preview ") {
                    if let Some(seconds) = command.strip_prefix("preview ") {
                        deadline = Some(
                            Instant::now()
                                + Duration::from_secs(seconds.parse::<u64>()?.clamp(1, 600)),
                        );
                    }
                    if players.is_none() && session.allowed()? {
                        // Shuffle only cached candidates, avoid a repeat between runs.
                        videos.shuffle(&mut rand::thread_rng());
                        avoid_repeat(&mut videos, last_first.as_deref());
                        let outputs = idle::output_names()?;
                        let mut resumed = false;
                        for event in rx.try_iter() {
                            match event {
                                IdleEvent::Resumed => resumed = true,
                                IdleEvent::Unavailable(error) => {
                                    bail!("Native idle monitor unavailable: {error}")
                                }
                                _ => {}
                            }
                        }
                        if (!resumed || command.starts_with("preview "))
                            && !STOP.load(Ordering::Relaxed)
                            && session.allowed()?
                        {
                            let group = playback::PlayerGroup::start(
                                &videos,
                                &outputs,
                                &paths.cache,
                                &paths.runtime,
                                config.clock,
                                config.captions,
                            )?;
                            last_first = videos.first().map(|(v, _)| v.id.clone());
                            players = Some(group);
                            log(
                                &mut logger,
                                format!("playing outputs={outputs:?} first={}", videos[0].0.name),
                            );
                        }
                    }
                }
            }
            if let Some(stream) = client.as_mut() {
                let state = players
                    .as_ref()
                    .map(|p| p.status())
                    .unwrap_or(json!({"playing":false}));
                let _ = writeln!(
                    stream,
                    "{}",
                    json!({"running":true,"idle_seconds":idle_seconds,"playback":state,"error":client_error})
                );
            }
            if quitting {
                break;
            }
            if let Some(group) = players.as_mut() {
                match group.tick() {
                    Ok(true) => {}
                    Ok(false) => {
                        players.take();
                        deadline = None;
                        log(&mut logger, "dismissed player-exit");
                    }
                    Err(error) => {
                        // IPC may close just before a quitting child is reaped.
                        // Stop this activation, keep idle monitoring available,
                        // and wait for a fresh idle/preview request before retry.
                        players.take();
                        deadline = None;
                        log(&mut logger, format!("playback stopped: {error:#}"));
                    }
                }
            }
            if players.is_some() && last_session_check.elapsed() >= Duration::from_millis(500) {
                last_session_check = Instant::now();
                if !session.allowed()? {
                    players.take();
                    deadline = None;
                    log(&mut logger, "dismissed locked-or-inactive");
                }
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                players.take();
                deadline = None;
                if preview.is_some() {
                    break;
                }
            }
            if preview.is_some() && players.is_none() && pending.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(80));
        }
        Ok(())
    })();
    players.take();
    let worker_outcome = idle::stop_worker(idle_thread, &shutdown);
    let _ = fs::remove_file(socket_path);
    log(&mut logger, "watcher stopped; owned players reaped");
    outcome.and(worker_outcome)
}

fn avoid_repeat(videos: &mut [(Video, PathBuf)], last: Option<&str>) {
    let candidates = Catalog {
        videos: videos.iter().map(|(v, _)| v.clone()).collect(),
    };
    let first = candidates
        .choose(last)
        .and_then(|first| videos.iter().position(|(v, _)| v.id == first.id));
    if let Some(index) = first {
        videos.swap(0, index);
    }
}

fn install_autostart(paths: &Paths) -> Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let value = executable.to_string_lossy();
    if value.contains(['\n', '\r', '%', '"', '`', '$', '\\']) {
        bail!("Executable path cannot be safely represented in a desktop entry")
    }
    let config_dir = dirs::config_dir().context("No config directory")?;
    let autostart = config_dir.join("autostart");
    let applications = dirs::data_dir()
        .context("No app directory")?
        .join("applications");
    fs::create_dir_all(&autostart)?;
    fs::create_dir_all(&applications)?;
    fs::create_dir_all(paths.config.parent().context("No config parent")?)?;
    if !paths.config.exists() {
        fs::write(
            &paths.config,
            serde_json::to_vec_pretty(&Config::default())?,
        )?;
    }
    fs::write(autostart.join("aerial-linux.desktop"),format!("[Desktop Entry]\nType=Application\nName=Aerial Screensaver\nExec=\"{value}\" watch\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"))?;
    fs::write(applications.join("aerial-linux.desktop"),format!("[Desktop Entry]\nType=Application\nName=Aerial Screensaver Preview\nComment=Preview aerial videos; automatic start follows idle time\nExec=\"{value}\" preview\nIcon=video-display\nTerminal=false\nCategories=Utility;\n"))?;
    println!("Installed login startup and preview launcher.");
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let paths = Paths::new()?;
    match args.command {
        Command::Refresh { hd } => {
            let catalog = if hd {
                Catalog::refresh_with_resolution(&paths.cache, Resolution::Hd1080)?
            } else {
                Catalog::refresh(&paths.cache)?
            };
            println!("{} aerials in Apple's catalog", catalog.videos.len());
        }
        Command::List { search } => {
            let catalog = paths.catalog()?;
            for video in catalog.videos.iter().filter(|v| {
                search
                    .as_ref()
                    .is_none_or(|s| v.name.to_lowercase().contains(&s.to_lowercase()))
            }) {
                let path = Catalog::cache_path(&paths.cache, video)?;
                println!(
                    "{}\t{}\t{}",
                    if path.exists() { "cached" } else { "available" },
                    video.id,
                    video.name
                );
            }
        }
        Command::Download { count, search } => {
            if count == 0 || count > 100 {
                bail!("Choose between 1 and 100 videos")
            }
            let catalog = paths.catalog()?;
            let mut candidates: Vec<_> = catalog
                .videos
                .iter()
                .filter(|v| {
                    search
                        .as_ref()
                        .is_none_or(|s| v.name.to_lowercase().contains(&s.to_lowercase()))
                })
                .collect();
            candidates.shuffle(&mut rand::thread_rng());
            if candidates.is_empty() {
                bail!("No aerials match this location")
            }
            for video in candidates.into_iter().take(count) {
                println!("Caching {} ({})", video.name, video.resolution.label());
                let path = catalog.cache_video(video, &paths.cache)?;
                println!("{}", path.display());
            }
        }
        Command::Watch { idle_seconds } => watch(
            &paths,
            idle_seconds.unwrap_or(paths.config()?.idle_seconds),
            None,
        )?,
        Command::Preview { seconds } => {
            if paths.socket().exists() {
                println!(
                    "{}",
                    control(&paths, &format!("preview {}", seconds.clamp(1, 600)))?
                );
            } else {
                watch(&paths, paths.config()?.idle_seconds, Some(seconds))?;
            }
        }
        Command::Hide => println!("{}", control(&paths, "hide")?),
        Command::Status => match control(&paths, "status") {
            Ok(state) => println!("{}", serde_json::to_string_pretty(&state)?),
            Err(error) if watcher_absent(&error) => println!("{}", json!({"running":false})),
            Err(error) => return Err(error),
        },
        Command::Quit => {
            println!("{}", control(&paths, "quit")?);
        }
        Command::InstallAutostart => install_autostart(&paths)?,
        Command::Disable => {
            let stopped = control(&paths, "quit");
            let path = dirs::config_dir()
                .context("No config directory")?
                .join("autostart/aerial-linux.desktop");
            if path.exists() {
                fs::remove_file(path)?;
            }
            if let Err(error) = stopped {
                if !watcher_absent(&error) {
                    return Err(error)
                        .context("Startup removed, but could not confirm watcher stopped");
                }
            }
            println!("Automatic startup disabled.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_idle_is_five_minutes() {
        assert_eq!(Config::default().idle_seconds, 300);
    }
    #[test]
    fn control_commands_are_validated_without_losing_status_semantics() {
        for invalid in ["", "preview nope", "preview", "preview 1 extra", "unknown"] {
            assert!(validated_command(invalid).is_err(), "{invalid}");
        }
        assert_eq!(validated_command(" status \n").unwrap(), "status");
        assert_eq!(validated_command("preview 9999").unwrap(), "preview 600");
        assert_eq!(validated_command("preview 0").unwrap(), "preview 1");
        assert!(!watcher_absent(&anyhow::anyhow!("malformed response")));
        assert!(watcher_absent(
            &std::io::Error::from(std::io::ErrorKind::NotFound).into()
        ));
    }
    #[test]
    fn empty_repeat_avoidance_is_safe() {
        avoid_repeat(&mut [], Some("id"));
    }
    #[test]
    fn repeated_runs_never_immediately_repeat_with_two_distinct_videos() {
        let video = |id: &str| {
            (
                Video {
                    id: id.into(),
                    name: id.into(),
                    url: "https://sylvan.apple.com/video.mov".into(),
                    resolution: Resolution::Sdr4k,
                    poi: vec![],
                },
                PathBuf::from(id),
            )
        };
        let mut videos = vec![video("a"), video("b")];
        for _ in 0..100 {
            let last = videos[0].0.id.clone();
            avoid_repeat(&mut videos, Some(&last));
            assert_ne!(videos[0].0.id, last);
        }
    }
}
