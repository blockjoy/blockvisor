use eyre::bail;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct CpuRegistry(Arc<Mutex<Vec<usize>>>);

impl CpuRegistry {
    pub fn new(available_cpus: usize) -> Self {
        Self(Arc::new(Mutex::new(
            (0..available_cpus).collect::<Vec<_>>(),
        )))
    }

    pub async fn acquire(&self, count: usize) -> eyre::Result<Vec<usize>> {
        let mut registry = self.0.lock().await;
        let len = registry.len();
        if count > len {
            bail!("not enough cpu cores")
        }
        Ok(registry.drain(len - count..).collect())
    }

    pub async fn mark_acquired(&self, cpus: &[usize]) {
        self.0.lock().await.retain(|cpu| !cpus.contains(cpu));
    }

    pub async fn release(&self, cpus: &mut Vec<usize>) {
        self.0.lock().await.append(cpus);
    }

    pub async fn len(&self) -> usize {
        self.0.lock().await.len()
    }
}
