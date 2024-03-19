use crate::{node_data::NodeData, pal};
use async_trait::async_trait;
use eyre::Result;
use std::path::Path;

#[derive(Debug)]
pub struct BareMachine {}

pub async fn create(
    _bv_root: &Path,
    _node_data: &NodeData<impl pal::NetInterface>,
) -> Result<BareMachine> {
    Ok(BareMachine {})
}

pub async fn attach(
    _bv_root: &Path,
    _node_data: &NodeData<impl pal::NetInterface>,
) -> Result<BareMachine> {
    Ok(BareMachine {})
}

#[async_trait]
impl pal::VirtualMachine for BareMachine {
    fn state(&self) -> pal::VmState {
        pal::VmState::SHUTOFF
    }

    async fn delete(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }

    async fn force_shutdown(&mut self) -> Result<()> {
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
}
