//! Offline, owned mpv playback. Only children started by this module are stopped.
use crate::catalog::Video;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

const APP: &str = "io.mpv.Mpv";
// Continue around the screen between preview/idle runs in this watcher too.
static NEXT_CLOCK_SLOT: AtomicUsize = AtomicUsize::new(0);
const CLOCK_POSITIONS: [(u8, u16, u16); 8] = [
    (7, 72, 72),
    (8, 960, 72),
    (9, 1848, 72),
    (6, 1848, 540),
    (3, 1848, 1010),
    (2, 960, 1010),
    (1, 72, 1010),
    (4, 72, 540),
];
const INPUT: &str = "ANY_UNICODE quit\nESC quit\nSPACE quit\nENTER quit\nLEFT quit\nRIGHT quit\nUP quit\nDOWN quit\nMBTN_LEFT quit\nMBTN_RIGHT quit\nMBTN_MID quit\nWHEEL_UP quit\nWHEEL_DOWN quit\n";

pub struct PlayerGroup {
    players: Vec<Player>,
    run_dir: PathBuf,
    videos: Vec<(Video, PathBuf)>,
    clock: bool,
    captions: bool,
    desktop_clock: crate::clock::DesktopClock,
    started: Instant,
}

struct Player {
    child: Child,
    output: String,
    socket: PathBuf,
    ipc: Option<Ipc>,
    path: Option<String>,
    position: Option<f64>,
    duration: Option<f64>,
    hwdec: Option<String>,
    dropped_frames: Option<u64>,
    brightness: Option<i64>,
    overlay: String,
    clock_slot: usize,
    exited: bool,
}

struct Ipc {
    stream: UnixStream,
    incoming: Vec<u8>,
    outgoing: VecDeque<Vec<u8>>,
    written: usize,
}

impl PlayerGroup {
    pub fn start(
        videos: &[(Video, PathBuf)],
        outputs: &[String],
        cache_root: &Path,
        runtime_root: &Path,
        clock: bool,
        captions: bool,
    ) -> Result<Self> {
        ensure!(!videos.is_empty(), "no cached videos to play");
        ensure!(!outputs.is_empty(), "no Wayland outputs found");
        let cache_root = fs::canonicalize(cache_root).context("resolve video cache")?;
        ensure!(cache_root.is_absolute(), "cache must be absolute");
        private_directory(runtime_root)?;
        let runtime_root = fs::canonicalize(runtime_root)?;
        // Flatpak gets exactly this XDG runtime directory, never /tmp or home.
        let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is required")?;
        ensure!(
            runtime_root == fs::canonicalize(xdg_runtime)?.join("aerial-linux"),
            "runtime root must be $XDG_RUNTIME_DIR/aerial-linux"
        );
        let mut checked = Vec::new();
        for (video, path) in videos {
            let path = fs::canonicalize(path).context("resolve cached video")?;
            ensure!(
                path.starts_with(&cache_root) && path.is_file(),
                "video must be a file in the cache"
            );
            checked.push((video.clone(), path));
        }
        let input_path = cache_root.join("input.conf");
        let mut input = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&input_path)?;
        input.write_all(INPUT.as_bytes())?;
        let run_dir = runtime_root.join(format!(
            "play-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&run_dir)?;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
        // Construct the owner before spawning so a later spawn failure also
        // stops earlier children and removes the private runtime directory.
        let mut group = Self {
            players: Vec::new(),
            run_dir,
            videos: checked,
            clock,
            captions,
            desktop_clock: crate::clock::DesktopClock::new(),
            started: Instant::now(),
        };
        for (index, output) in outputs.iter().enumerate() {
            ensure!(
                !output.is_empty() && !output.contains('\0'),
                "invalid output name"
            );
            let socket = group.run_dir.join(format!("player-{index}.sock"));
            let paths: Vec<_> = group.videos.iter().map(|(_, path)| path.clone()).collect();
            let args = player_args(&cache_root, &socket, output, &paths);
            let mut command = Command::new("flatpak");
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            // A fresh process group lets the bounded fallback terminate this
            // invocation's sandbox and player, never another user's mpv.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().context("start offline mpv Flatpak")?;
            group.players.push(Player {
                child,
                output: output.clone(),
                socket,
                ipc: None,
                path: None,
                position: None,
                duration: None,
                hwdec: None,
                dropped_frames: None,
                brightness: None,
                overlay: String::new(),
                clock_slot: NEXT_CLOCK_SLOT.fetch_add(1, Ordering::Relaxed) % CLOCK_POSITIONS.len(),
                exited: false,
            });
        }
        Ok(group)
    }

    /// Returns false as soon as any owned player exits (including user input).
    pub fn tick(&mut self) -> Result<bool> {
        let clock_text = self.clock.then(|| self.desktop_clock.text());
        for player in &mut self.players {
            if player.exited || player.child.try_wait()?.is_some() {
                player.exited = true;
                return Ok(false);
            }
            if player.ipc.is_none() {
                match UnixStream::connect(&player.socket) {
                    Ok(stream) => {
                        stream.set_nonblocking(true)?;
                        let mut ipc = Ipc {
                            stream,
                            incoming: Vec::new(),
                            outgoing: VecDeque::new(),
                            written: 0,
                        };
                        for (id, property) in [
                            (1, "time-pos"),
                            (2, "path"),
                            (3, "duration"),
                            (4, "hwdec-current"),
                            (5, "frame-drop-count"),
                        ] {
                            ipc.queue(json!({"command": ["observe_property", id, property]}))?;
                        }
                        player.ipc = Some(ipc);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::NotFound | ErrorKind::ConnectionRefused
                        ) =>
                    {
                        ensure!(
                            self.started.elapsed() < Duration::from_secs(15),
                            "mpv did not create IPC socket for {}",
                            player.output
                        );
                        continue;
                    }
                    Err(error) => return Err(error).context("connect owned mpv IPC"),
                }
            }
            let ipc = player.ipc.as_mut().expect("connected above");
            for event in ipc.poll()? {
                if event.get("event").and_then(Value::as_str) == Some("file-loaded") {
                    // File-load events include playlist wraps and repeat-one;
                    // metadata/path notifications alone do not define rotation.
                    player.clock_slot = next_clock_slot(player.clock_slot);
                    NEXT_CLOCK_SLOT.store(next_clock_slot(player.clock_slot), Ordering::Relaxed);
                }
                if event.get("event").and_then(Value::as_str) == Some("end-file")
                    && event.get("reason").and_then(Value::as_str) == Some("error")
                {
                    bail!("mpv playback failed on {}: {}", player.output, event);
                }
                if event.get("event").and_then(Value::as_str) != Some("property-change") {
                    continue;
                }
                let data = &event["data"];
                match event["name"].as_str() {
                    Some("time-pos") => player.position = finite_number(data),
                    Some("duration") => player.duration = finite_number(data),
                    Some("hwdec-current") => player.hwdec = data.as_str().map(str::to_owned),
                    Some("frame-drop-count") => player.dropped_frames = data.as_u64(),
                    Some("path") => {
                        player.path = data.as_str().map(str::to_owned);
                        player.position = None;
                        player.duration = None;
                    }
                    _ => {}
                }
            }
            let position = player.position.unwrap_or(0.0);
            let brightness = fade_brightness(position, player.duration);
            if player.brightness != Some(brightness) {
                ipc.queue(json!({"command": ["set_property", "brightness", brightness]}))?;
                player.brightness = Some(brightness);
            }
            let video = player
                .path
                .as_ref()
                .and_then(|path| {
                    self.videos
                        .iter()
                        .find(|(_, local)| local.as_os_str() == std::ffi::OsStr::new(path))
                })
                .map(|(video, _)| video);
            let caption = if self.captions {
                video.map(|video| video.caption_at(position)).unwrap_or("")
            } else {
                ""
            };
            let overlay = overlay_ass(caption, clock_text.as_deref(), player.clock_slot);
            if overlay != player.overlay {
                ipc.queue(
                    json!({"command": ["osd-overlay", 1, "ass-events", overlay, 1920, 1080]}),
                )?;
                player.overlay = overlay;
            }
            ipc.flush()?;
        }
        Ok(true)
    }

    /// Local status only: useful for confirming position advances per output.
    pub fn status(&self) -> Value {
        json!({"backend": APP, "clock": self.clock, "clock_format": self.clock.then(|| self.desktop_clock.format_name()), "captions": self.captions, "players": self.players.iter().map(|player| json!({
            "pid": player.child.id(), "output": player.output, "socket": player.socket,
            "connected": player.ipc.is_some(), "path": player.path,
            "position": player.position, "duration": player.duration, "hwdec": player.hwdec,
            "dropped_frames": player.dropped_frames, "exited": player.exited,
            "clock_slot": player.clock_slot, "clock_position": CLOCK_POSITIONS[player.clock_slot]
        })).collect::<Vec<_>>()})
    }

    pub fn stop(&mut self) {
        for player in &mut self.players {
            if let Some(ipc) = player.ipc.as_mut() {
                let _ = ipc.queue(json!({"command": ["quit"]}));
                let _ = ipc.flush();
            } else if let Ok(mut stream) = UnixStream::connect(&player.socket) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                let _ = stream.write_all(b"{\"command\":[\"quit\"]}\n");
            }
        }
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            let mut remaining = false;
            for player in &mut self.players {
                if !player.exited {
                    if matches!(player.child.try_wait(), Ok(Some(_))) {
                        player.exited = true;
                    } else {
                        remaining = true;
                    }
                }
            }
            if !remaining || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for player in &mut self.players {
            if !player.exited && matches!(player.child.try_wait(), Ok(None)) {
                // The direct child has not been reaped; its PID cannot have
                // been reused. Its private process group belongs to this run.
                unsafe {
                    libc::kill(-(player.child.id() as i32), libc::SIGTERM);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            let mut remaining = false;
            for player in &mut self.players {
                if !player.exited {
                    if matches!(player.child.try_wait(), Ok(Some(_))) {
                        player.exited = true;
                    } else {
                        remaining = true;
                    }
                }
            }
            if !remaining || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for player in &mut self.players {
            if !player.exited && matches!(player.child.try_wait(), Ok(None)) {
                unsafe {
                    libc::kill(-(player.child.id() as i32), libc::SIGKILL);
                }
                let _ = player.child.kill();
                let _ = player.child.wait();
                player.exited = true;
            }
            player.ipc = None;
            let _ = fs::remove_file(&player.socket);
        }
        let _ = fs::remove_dir(&self.run_dir);
    }
}

impl Drop for PlayerGroup {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Ipc {
    fn queue(&mut self, value: Value) -> Result<()> {
        ensure!(
            self.outgoing.len() < 64,
            "mpv IPC is not accepting commands"
        );
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.outgoing.push_back(bytes);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        while let Some(bytes) = self.outgoing.front() {
            match self.stream.write(&bytes[self.written..]) {
                Ok(0) => bail!("mpv IPC closed while writing"),
                Ok(count) => {
                    self.written += count;
                    if self.written == bytes.len() {
                        self.outgoing.pop_front();
                        self.written = 0;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("write mpv IPC"),
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<Vec<Value>> {
        self.flush()?;
        let mut chunk = [0u8; 8192];
        // Bound work even if a malfunctioning child emits unlimited events.
        for _ in 0..16 {
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("mpv IPC closed while reading"),
                Ok(count) => self.incoming.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("read mpv IPC"),
            }
            ensure!(
                self.incoming.len() <= 256 * 1024,
                "mpv IPC exceeded event buffer limit"
            );
        }
        let mut events = Vec::new();
        let mut consumed = 0;
        while let Some(end) = self.incoming[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + end;
            let value = serde_json::from_slice(&self.incoming[consumed..end])
                .context("malformed mpv IPC event")?;
            events.push(value);
            consumed = end + 1;
        }
        self.incoming.drain(..consumed);
        Ok(events)
    }
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "runtime path must be a real directory"
    );
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "runtime directory belongs to another user"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn player_args(cache: &Path, socket: &Path, output: &str, paths: &[PathBuf]) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "run",
        "--unshare=network",
        "--nodevice=all",
        "--device=dri",
        "--nosocket=x11",
        "--nosocket=pulseaudio",
        "--nofilesystem=host",
        "--nofilesystem=home",
        "--nofilesystem=xdg-pictures",
        "--nofilesystem=xdg-videos",
        "--nofilesystem=/tmp",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut cache_permission = OsString::from("--filesystem=");
    cache_permission.push(cache);
    cache_permission.push(":ro");
    args.push(cache_permission);
    args.push("--filesystem=xdg-run/aerial-linux:rw".into());
    args.push(APP.into());
    args.extend(
        [
            "--no-config",
            "--load-scripts=no",
            "--no-terminal",
            "--vo=gpu-next",
            "--gpu-context=wayland",
            "--hwdec=auto-safe",
            "--fs",
            "--no-border",
            "--no-audio",
            "--panscan=1",
            "--ytdl=no",
            "--no-osc",
            "--osd-level=0",
            "--no-input-default-bindings",
            "--cursor-autohide=always",
            "--stop-screensaver=no",
            "--loop-playlist=inf",
            "--title=Aerial Screensaver",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(format!("--fs-screen-name={output}").into());
    let mut input = OsString::from("--input-conf=");
    input.push(cache.join("input.conf"));
    args.push(input);
    let mut ipc = OsString::from("--input-ipc-server=");
    ipc.push(socket);
    args.push(ipc);
    args.push("--".into());
    args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    args
}

fn finite_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn fade_brightness(position: f64, duration: Option<f64>) -> i64 {
    let from_start = (position / 2.0).clamp(0.0, 1.0);
    let to_end = duration
        .map(|end| ((end - position) / 2.0).clamp(0.0, 1.0))
        .unwrap_or(1.0);
    (-100.0 * (1.0 - from_start.min(to_end))).round() as i64
}

fn ass_text(text: &str) -> String {
    text.chars()
        .take(600)
        .map(|character| match character {
            '{' => '｛',
            '}' => '｝',
            '\\' => '／',
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

fn next_clock_slot(slot: usize) -> usize {
    (slot + 1) % CLOCK_POSITIONS.len()
}

fn overlay_ass(caption: &str, clock: Option<&str>, clock_slot: usize) -> String {
    let caption = ass_text(caption);
    let mut text = if caption.is_empty() {
        String::new()
    } else {
        format!("{{\\an1\\pos(72,1010)\\fs30\\bord1.2\\shad1\\1c&HFFFFFF&\\3c&H222222&}}{caption}")
    };
    if let Some(clock) = clock {
        let (alignment, x, y) = CLOCK_POSITIONS[clock_slot];
        text.push_str(&format!(
            "\n{{\\an{alignment}\\pos({x},{y})\\fs42\\bord1.2\\shad1\\1c&HFFFFFF&\\3c&H222222&}}{}",
            ass_text(clock)
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_text_emits_no_overlay_content() {
        assert!(overlay_ass("", None, 0).is_empty());
    }
    #[test]
    fn clock_moves_through_every_position_without_adjacent_repeats() {
        let mut slot = 0;
        let mut visited = std::collections::HashSet::new();
        for _ in 0..CLOCK_POSITIONS.len() {
            assert!(visited.insert(CLOCK_POSITIONS[slot]));
            let next = next_clock_slot(slot);
            assert_ne!(CLOCK_POSITIONS[slot], CLOCK_POSITIONS[next]);
            assert_ne!(
                overlay_ass("", Some("9:42 PM"), slot),
                overlay_ass("", Some("9:42 PM"), next)
            );
            slot = next;
        }
        assert_eq!(slot, 0);
    }
    #[test]
    fn captions_cannot_inject_ass_tags_or_line_commands() {
        assert_eq!(ass_text("A{\\pos(0,0)}\\N\nB"), "A｛／pos(0,0)｝／N B");
        assert_eq!(ass_text(&"x".repeat(900)).len(), 600);
    }
    #[test]
    fn fades_both_ends_and_handles_short_videos() {
        assert_eq!(fade_brightness(0.0, Some(60.0)), -100);
        assert_eq!(fade_brightness(1.0, Some(60.0)), -50);
        assert_eq!(fade_brightness(3.0, Some(60.0)), 0);
        assert_eq!(fade_brightness(59.0, Some(60.0)), -50);
        assert_eq!(fade_brightness(60.0, Some(60.0)), -100);
        assert_eq!(fade_brightness(0.5, Some(1.0)), -75);
    }
    #[test]
    fn sandbox_is_offline_and_paths_are_separate_arguments() {
        let args = player_args(
            Path::new("/cache with spaces"),
            Path::new("/run/user/1000/aerial-linux/player.sock"),
            "eDP-1",
            &[PathBuf::from("/cache with spaces/a.mov")],
        );
        let strings: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        for flag in [
            "--unshare=network",
            "--nodevice=all",
            "--device=dri",
            "--nosocket=x11",
            "--nosocket=pulseaudio",
            "--nofilesystem=host",
            "--nofilesystem=home",
            "--nofilesystem=/tmp",
            "--filesystem=/cache with spaces:ro",
            "--filesystem=xdg-run/aerial-linux:rw",
            "--load-scripts=no",
        ] {
            assert!(strings.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert_eq!(args.last().unwrap(), "/cache with spaces/a.mov");
    }
    #[test]
    fn ipc_preserves_fragmented_events_and_queues_commands() {
        let (a, mut b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut ipc = Ipc {
            stream: a,
            incoming: vec![],
            outgoing: VecDeque::new(),
            written: 0,
        };
        b.write_all(b"{\"event\":\"property-change\",").unwrap();
        assert!(ipc.poll().unwrap().is_empty());
        b.write_all(b"\"name\":\"time-pos\",\"data\":12.5}\n")
            .unwrap();
        assert_eq!(ipc.poll().unwrap()[0]["data"], 12.5);
        ipc.queue(json!({"command": ["quit"]})).unwrap();
        ipc.flush().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut data = [0u8; 128];
        let count = b.read(&mut data).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&data[..count]).unwrap()["command"][0],
            "quit"
        );
    }
    #[test]
    fn closed_ipc_is_a_failure_instead_of_a_frozen_overlay() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut ipc = Ipc {
            stream: a,
            incoming: vec![],
            outgoing: VecDeque::new(),
            written: 0,
        };
        drop(b);
        assert!(ipc.poll().is_err());
    }
}
