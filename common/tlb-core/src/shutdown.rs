//! Graceful shutdown on SIGTERM / SIGINT.

use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

#[derive(Clone)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
}

impl Shutdown {
    /// Installs the signal handlers. Must be called inside a Tokio runtime.
    pub fn install() -> Self {
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            eprintln!("shutdown requested");
            let _ = tx.send(true);
        });
        Self { rx }
    }

    pub fn is_requested(&self) -> bool {
        *self.rx.borrow()
    }

    /// Sleeps for `duration` or until shutdown is requested.
    /// Returns true when shutdown was requested.
    pub async fn sleep(&mut self, duration: Duration) -> bool {
        if self.is_requested() {
            return true;
        }
        tokio::select! {
            _ = tokio::time::sleep(duration) => self.is_requested(),
            _ = self.rx.changed() => true,
        }
    }
}
