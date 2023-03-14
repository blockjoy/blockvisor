use ctrlc;
use std::future::Future;
use tokio::sync::broadcast;

/// Flag representing global service state to enable graceful shutdown.
/// This cloneable object shall be distributed to each part of service that needs to be aware of shutdown process.
/// Based on tokio::sync::broadcast to notify all clones about shutdown request.
pub struct RunFlag {
    run: bool,
    tx: broadcast::Sender<()>,
    rx: broadcast::Receiver<()>,
}

impl RunFlag {
    /// Create [Self] with CtrlC handler attached.
    pub fn run_until_ctrlc() -> Self {
        let mut run = Self::default();
        let mut ctrlc_run = run.clone();
        ctrlc::set_handler(move || {
            ctrlc_run.stop();
        })
        .expect("Error setting Ctrl-C handler");
        run
    }

    /// Set flag to `false` and send shutdown request to all clones.
    pub fn stop(&mut self) {
        self.run = false;
        let _ = self.tx.send(());
    }

    /// Checks for pending shutdown requests and return flag value.
    pub fn load(&mut self) -> bool {
        if self.run {
            self.run = match self.rx.try_recv() {
                Err(broadcast::error::TryRecvError::Empty) => true,
                Ok(_) | Err(_) => false,
            };
        }
        self.run
    }

    /// Wait for shutdown request.
    /// Return immediately if already stopped.
    pub async fn wait(&mut self) {
        if self.run {
            let _ = self.rx.recv().await;
            self.run = false;
        }
    }

    pub async fn select<T>(&mut self, future: impl Future<Output = T>) {
        tokio::select!(
            _ = future => {},
            _ = self.wait() => {},
        );
    }

    pub fn clone(&mut self) -> Self {
        let rx = self.rx.resubscribe();
        Self {
            run: self.load(),
            tx: self.tx.clone(),
            rx,
        }
    }
}

impl Default for RunFlag {
    fn default() -> Self {
        let (tx, rx) = broadcast::channel(1);
        Self { run: true, tx, rx }
    }
}
