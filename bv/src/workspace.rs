use crate::{
    linux_fc_platform::bv_root, node_context::REGISTRY_CONFIG_DIR, node_data::NodeImage,
    services::blockchain::BABEL_PLUGIN_NAME, BV_VAR_PATH,
};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::{fs, os::unix, path::Path};
use uuid::Uuid;

const VSCODE_WORKSPACE_FILENAME: &str = ".code-workspace";
const BV_WORKSPACE_FILENAME: &str = ".bv-workspace";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub active_image: Option<NodeImage>,
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
    let plugin_link_path = path.join(BABEL_PLUGIN_NAME);
    let _ = fs::remove_file(&plugin_link_path);
    unix::fs::symlink(
        bv_root()
            .join(BV_VAR_PATH)
            .join(REGISTRY_CONFIG_DIR)
            .join(format!("{id}.rhai")),
        plugin_link_path,
    )?;
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
            let plugin_link_path = path.join(BABEL_PLUGIN_NAME);
            let _ = fs::remove_file(plugin_link_path);
        }
    }
    Ok(())
}

pub fn set_active_image(path: &Path, image: NodeImage) -> Result<()> {
    let ws_path = path.join(BV_WORKSPACE_FILENAME);
    let mut workspace: Workspace = serde_json::from_str(&fs::read_to_string(&ws_path)?)?;
    workspace.active_image = Some(image);
    fs::write(ws_path, serde_json::to_string(&workspace)?)?;
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
        assert!(workspace.active_image.is_none());
        assert!(workspace.active_node.is_none());
        Ok(())
    }

    #[test]
    pub fn test_active() -> Result<()> {
        let tmp_dir = TempDir::new()?.to_path_buf();
        create(&tmp_dir)?;
        set_active_image(
            &tmp_dir,
            NodeImage {
                protocol: "any_protocol".to_string(),
                node_type: "node".to_string(),
                node_version: "7.7.7".to_string(),
            },
        )?;
        let uid = Uuid::new_v4();
        set_active_node(&tmp_dir, uid, "some_crazy_pet_name")?;
        let workspace = read(&tmp_dir)?;
        assert_eq!(
            Some(NodeImage {
                protocol: "any_protocol".to_string(),
                node_type: "node".to_string(),
                node_version: "7.7.7".to_string(),
            }),
            workspace.active_image
        );
        assert_eq!(
            Some(ActiveNode {
                id: uid,
                name: "some_crazy_pet_name".to_string(),
            }),
            workspace.active_node
        );
        assert!(tmp_dir.join(BABEL_PLUGIN_NAME).is_symlink());
        Ok(())
    }
}
