//! Bounded, passive native idle/inhibitor diagnostic. Never launches a player.
#[allow(dead_code)]
#[path = "../src/idle.rs"]
mod idle;
use std::{
    sync::{atomic::AtomicBool, mpsc, Arc},
    time::{Duration, Instant},
};

fn main() -> anyhow::Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker = idle::spawn_idle(Duration::from_secs(2), tx, shutdown.clone())?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(14) {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) {
            println!("{:.3}s {event:?}", start.elapsed().as_secs_f64());
        }
    }
    idle::stop_worker(worker, &shutdown)
}
