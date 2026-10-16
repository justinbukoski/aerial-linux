//! Compositor-native idle notifications. This never locks the session or changes
//! display power; the caller decides when to show and dismiss its video window.

use std::{
    io,
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use wayland_client::{
    backend::WaylandError,
    delegate_noop,
    protocol::{wl_callback, wl_output, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    Idle,
    Resumed,
    /// Native idle monitoring failed. Do not guess idle state in response.
    Unavailable(String),
}

/// Start an independent Wayland connection for idle monitoring. Startup or
/// compositor failures are reported through `tx`; thread-creation failures are
/// returned directly. Setting `shutdown` lets the worker exit within one poll
/// interval (250 ms), including while waiting for initial registry replies.
pub fn spawn_idle(
    timeout: Duration,
    tx: Sender<IdleEvent>,
    shutdown: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let timeout_ms = timeout_millis(timeout)?;
    thread::Builder::new()
        .name("aerial-idle".into())
        .spawn(move || {
            if let Err(error) = run_idle(timeout_ms, &tx, &shutdown) {
                if !shutdown.load(Ordering::Relaxed) {
                    let _ = tx.send(IdleEvent::Unavailable(format!("{error:#}")));
                }
            }
        })
        .context("start native Wayland idle monitor")
}

fn timeout_millis(timeout: Duration) -> Result<u32> {
    if timeout.is_zero() {
        bail!("idle timeout must be greater than zero");
    }
    // Round up so a fractional millisecond never becomes an immediate timeout.
    let millis =
        timeout.as_millis() + u128::from(!timeout.subsec_nanos().is_multiple_of(1_000_000));
    u32::try_from(millis).context("idle timeout exceeds Wayland's 32-bit millisecond limit")
}

/// Stop the worker without letting a stuck connection trap application exit.
pub fn stop_worker(worker: JoinHandle<()>, shutdown: &AtomicBool) -> Result<()> {
    shutdown.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            bail!("Wayland idle worker did not stop within two seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
    worker
        .join()
        .map_err(|_| anyhow!("Wayland idle worker panicked"))
}

struct Seat {
    global: u32,
    proxy: wl_seat::WlSeat,
    name: Option<String>,
}

struct State {
    tx: Sender<IdleEvent>,
    seats: Vec<Seat>,
    notifier: Option<(u32, ExtIdleNotifierV1)>,
    selected_seat: Option<u32>,
    sync_done: bool,
    failure: Option<String>,
    receiver_gone: bool,
    outputs: Vec<(u32, wl_output::WlOutput, Option<String>)>,
}

fn discover(
    connection: &Connection,
    tx: &Sender<IdleEvent>,
    shutdown: &AtomicBool,
) -> Result<Option<(EventQueue<State>, State)>> {
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = State {
        tx: tx.clone(),
        seats: Vec::new(),
        notifier: None,
        selected_seat: None,
        sync_done: false,
        failure: None,
        receiver_gone: false,
        outputs: Vec::new(),
    };

    // The second sync receives names from the seats bound during the first.
    // Both use bounded polling, so an unresponsive compositor cannot trap join.
    for _ in 0..2 {
        state.sync_done = false;
        connection.display().sync(&qh, ());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.sync_done {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                bail!("Wayland registry discovery timed out");
            }
            pump(connection, &mut queue, &mut state)?;
            if let Some(reason) = state.failure.take() {
                bail!(reason);
            }
        }
    }
    Ok(Some((queue, state)))
}

/// Discover compositor connector names (for example `DP-2`) without GTK. These
/// are a snapshot; call again before each activation to handle display changes.
/// A compositor without wl_output v4 names reports an error instead of guessing
/// connector names from output order.
pub fn output_names() -> Result<Vec<String>> {
    let connection = Connection::connect_to_env().context("connect to the Wayland compositor")?;
    let (tx, _rx) = std::sync::mpsc::channel();
    let shutdown = AtomicBool::new(false);
    let (_, state) = discover(&connection, &tx, &shutdown)?
        .ok_or_else(|| anyhow!("Wayland output discovery cancelled"))?;
    if state.outputs.is_empty() {
        bail!("compositor does not expose a wl_output");
    }
    let mut names = state
        .outputs
        .iter()
        .map(|(_, _, name)| {
            name.clone()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("compositor did not provide a wl_output v4 connector name"))
        })
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    names.dedup();
    Ok(names)
}

fn run_idle(timeout_ms: u32, tx: &Sender<IdleEvent>, shutdown: &AtomicBool) -> Result<()> {
    if shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }
    let connection = Connection::connect_to_env().context("connect to the Wayland compositor")?;
    let Some((mut queue, mut state)) = discover(&connection, tx, shutdown)? else {
        return Ok(());
    };
    let qh = queue.handle();

    let manager = state
        .notifier
        .as_ref()
        .map(|(_, proxy)| proxy.clone())
        .ok_or_else(|| anyhow!("compositor does not expose ext_idle_notifier_v1"))?;
    let seat = state
        .seats
        .iter()
        .find(|seat| seat.name.as_deref() == Some("seat0"))
        .or_else(|| state.seats.first())
        .ok_or_else(|| anyhow!("compositor does not expose a wl_seat"))?;
    state.selected_seat = Some(seat.global);
    // get_idle_notification respects visible-surface idle inhibitors. Do not
    // use get_input_idle_notification, which deliberately ignores inhibitors.
    let notification = manager.get_idle_notification(timeout_ms, &seat.proxy, &qh, ());

    let result = (|| {
        while !shutdown.load(Ordering::Relaxed) && !state.receiver_gone {
            pump(&connection, &mut queue, &mut state)?;
            if let Some(reason) = state.failure.take() {
                bail!(reason);
            }
        }
        Ok(())
    })();
    notification.destroy();
    manager.destroy();
    for seat in &state.seats {
        if seat.proxy.version() >= 5 {
            seat.proxy.release();
        }
    }
    // Closing our own connection also releases resources if flushing fails.
    let _ = connection.flush();
    result
}

fn pump(connection: &Connection, queue: &mut EventQueue<State>, state: &mut State) -> Result<()> {
    queue
        .dispatch_pending(state)
        .context("dispatch Wayland idle events")?;
    let needs_write = match connection.flush() {
        Ok(()) => false,
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(error) => return Err(error).context("flush Wayland idle requests"),
    };
    let Some(reader) = queue.prepare_read() else {
        return Ok(());
    };
    let mut fd = libc::pollfd {
        fd: reader.connection_fd().as_raw_fd(),
        events: libc::POLLIN | if needs_write { libc::POLLOUT } else { 0 },
        revents: 0,
    };
    // SAFETY: fd points to one initialized pollfd for the duration of poll.
    // The read guard keeps this connection alive until readiness is handled.
    let ready = unsafe { libc::poll(&mut fd, 1, 250) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(error).context("poll Wayland idle connection");
    }
    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        bail!(
            "Wayland idle connection closed (poll events {:#x})",
            fd.revents
        );
    }
    if fd.revents & libc::POLLIN != 0 {
        match reader.read() {
            Ok(_) => {}
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("read Wayland idle events"),
        }
    }
    // If no input was ready, dropping reader cancels the prepared read. This
    // also happens on all early returns, avoiding a Wayland read deadlock.
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "ext_idle_notifier_v1" if state.notifier.is_none() => {
                    state.notifier = Some((name, registry.bind(name, version.min(1), qh, ())));
                }
                "wl_seat" => state.seats.push(Seat {
                    global: name,
                    proxy: registry.bind(name, version.min(5), qh, ()),
                    name: None,
                }),
                "wl_output" => {
                    state
                        .outputs
                        .push((name, registry.bind(name, version.min(4), qh, ()), None));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if state.selected_seat == Some(name)
                    || state
                        .notifier
                        .as_ref()
                        .is_some_and(|(global, _)| *global == name)
                {
                    state.failure =
                        Some("Wayland idle notifier or selected seat was removed".into());
                }
                state.seats.retain(|seat| seat.global != name);
                state.outputs.retain(|(global, _, _)| *global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some((_, _, output_name)) = state
                .outputs
                .iter_mut()
                .find(|(_, output, _)| output.id() == proxy.id())
            {
                *output_name = Some(name);
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            if let Some(seat) = state
                .seats
                .iter_mut()
                .find(|seat| seat.proxy.id() == proxy.id())
            {
                seat.name = Some(name);
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = true;
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            ext_idle_notification_v1::Event::Idled => IdleEvent::Idle,
            ext_idle_notification_v1::Event::Resumed => IdleEvent::Resumed,
            _ => return,
        };
        state.receiver_gone = state.tx.send(event).is_err();
    }
}

delegate_noop!(State: ignore ExtIdleNotifierV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_timeout_is_positive_rounded_up_and_checked() {
        assert!(timeout_millis(Duration::ZERO).is_err());
        assert_eq!(timeout_millis(Duration::from_nanos(1)).unwrap(), 1);
        assert_eq!(timeout_millis(Duration::from_secs(300)).unwrap(), 300_000);
        assert_eq!(
            timeout_millis(Duration::from_millis(u32::MAX.into())).unwrap(),
            u32::MAX
        );
        assert!(timeout_millis(Duration::from_millis(u64::from(u32::MAX) + 1)).is_err());
    }
    #[test]
    fn worker_panics_are_reported() {
        let shutdown = AtomicBool::new(false);
        assert!(stop_worker(thread::spawn(|| panic!("test worker")), &shutdown).is_err());
        assert!(shutdown.load(Ordering::Relaxed));
    }
}
