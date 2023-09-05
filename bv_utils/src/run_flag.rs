use ctrlc;
use std::sync::Mutex;
use std::{future::Future, sync::Arc};
use tokio::sync::broadcast;

/// Flag representing global service state to enable graceful shutdown.
/// This cloneable object shall be distributed to each part of service that needs to be aware of shutdown process.
/// Based on tokio::sync::broadcast to notify all clones about shutdown request.
pub struct RunFlag {
    run: bool,
    tx: Arc<Mutex<Vec<broadcast::Sender<()>>>>,
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
        for tx in self.tx.lock().unwrap().iter() {
            let _ = tx.send(());
        }
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

    pub async fn select<T>(&mut self, future: impl Future<Output = T>) -> Option<T> {
        tokio::select!(
            output = future => { Some(output)},
            _ = self.wait() => { None},
        )
    }

    pub fn clone(&mut self) -> Self {
        let rx = self.rx.resubscribe();
        Self {
            run: self.load(),
            tx: self.tx.clone(),
            rx,
        }
    }

    /// Creates new `RunFlag` that will be stopped if parent is stopped,
    /// but stopping child flag won't stop parent flag.
    pub fn child_flag(&mut self) -> Self {
        let (tx, rx) = broadcast::channel(1);
        self.tx.lock().unwrap().push(tx.clone());
        Self {
            run: self.load(),
            tx: Arc::new(Mutex::new(vec![tx])),
            rx,
        }
    }
}

impl Default for RunFlag {
    fn default() -> Self {
        let (tx, rx) = broadcast::channel(1);
        Self {
            run: true,
            tx: Arc::new(Mutex::new(vec![tx])),
            rx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_child_flag() {
        let mut orig_parent = RunFlag::default();
        let mut parent = orig_parent.clone();

        let mut child_i = parent.child_flag();
        let mut sibling_i_a = child_i.clone();
        let mut sibling_i_b = child_i.clone();

        let mut child_ii = parent.child_flag();
        let mut sibling_ii_a = child_ii.clone();
        let mut sibling_ii_b = child_ii.clone();

        assert!(parent.load());
        assert!(child_i.load());
        assert!(sibling_i_a.load());
        assert!(sibling_i_b.load());
        assert!(child_ii.load());
        assert!(sibling_ii_a.load());
        assert!(sibling_ii_b.load());

        sibling_ii_b.stop();

        assert!(parent.load());
        assert!(child_i.load());
        assert!(sibling_i_a.load());
        assert!(sibling_i_b.load());
        assert!(!child_ii.load());
        assert!(!sibling_ii_a.load());
        assert!(!sibling_ii_b.load());

        orig_parent.stop();

        assert!(!parent.load());
        assert!(!child_i.load());
        assert!(!sibling_i_a.load());
        assert!(!sibling_i_b.load());
        assert!(!child_ii.load());
        assert!(!sibling_ii_a.load());
        assert!(!sibling_ii_b.load());
    }
}
