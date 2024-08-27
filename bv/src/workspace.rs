use crate::{linux_platform::bv_root, node_context};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::{fs, os::unix, path::Path};
use uuid::Uuid;

const VSCODE_WORKSPACE_FILENAME: &str = ".code-workspace";
const BV_WORKSPACE_FILENAME: &str = ".bv-workspace";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub active_node: Option<ActiveNode>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ActiveNode {
    pub id: Uuid,
    pub name: String,
}

pub fn create(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::write(
        path.join(VSCODE_WORKSPACE_FILENAME),
        include_str!("../data/.code-workspace"),
    )?;
    fs::write(path.join(BV_WORKSPACE_FILENAME), "{}")?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Workspace> {
    Ok(serde_json::from_str(&fs::read_to_string(
        path.join(BV_WORKSPACE_FILENAME),
    )?)?)
}

pub fn set_active_node(path: &Path, id: Uuid, name: &str) -> Result<()> {
    let ws_path = path.join(BV_WORKSPACE_FILENAME);
    let mut workspace: Workspace = serde_json::from_str(&fs::read_to_string(&ws_path)?)?;
    workspace.active_node = Some(ActiveNode {
        id,
        name: name.to_owned(),
    });
    fs::write(ws_path, serde_json::to_string(&workspace)?)?;
    let node_link_path = path.join("node");
    let _ = fs::remove_file(&node_link_path);
    unix::fs::symlink(node_context::build_node_dir(&bv_root(), id), node_link_path)?;
    Ok(())
}

/// Unset active node in workspace if it was active previously
pub fn unset_active_node(path: &Path, id: Uuid) -> Result<()> {
    let ws_path = path.join(BV_WORKSPACE_FILENAME);
    let mut workspace: Workspace = serde_json::from_str(&fs::read_to_string(&ws_path)?)?;
    if let Some(active_node) = workspace.active_node {
        if active_node.id == id {
            workspace.active_node = None;
            fs::write(ws_path, serde_json::to_string(&workspace)?)?;
            let plugin_link_path = path.join("node");
            let _ = fs::remove_file(plugin_link_path);
        }
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use assert_fs::TempDir;

    #[test]
    pub fn test_create() -> Result<()> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        create(&tmp_dir)?;
        assert!(tmp_dir.join(VSCODE_WORKSPACE_FILENAME).exists());
        assert!(tmp_dir.join(BV_WORKSPACE_FILENAME).exists());
        let workspace = read(&tmp_dir)?;
        assert!(workspace.active_node.is_none());
        Ok(())
    }

    #[test]
    pub fn test_active() -> Result<()> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        create(&tmp_dir)?;
        let uid = Uuid::new_v4();
        set_active_node(&tmp_dir, uid, "some_crazy_pet_name")?;
        let workspace = read(&tmp_dir)?;
        assert_eq!(
            Some(ActiveNode {
                id: uid,
                name: "some_crazy_pet_name".to_string(),
            }),
            workspace.active_node
        );
        assert!(tmp_dir.join("node").is_symlink());
        Ok(())
    }
}
