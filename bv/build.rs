use std::fs;
use std::path::{Path, PathBuf};

const PROTO_DIRS: &[&str] = &["./proto"];

fn main() {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&proto_files(), PROTO_DIRS)
        .expect("Failed to compile protos")
}

fn proto_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in PROTO_DIRS {
        find_recursive(Path::new(dir), &mut files);
    }
    files
}

fn find_recursive(path: &Path, files: &mut Vec<PathBuf>) {
    if !path.is_dir() {
        return;
    }

    for entry in fs::read_dir(path).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            find_recursive(&path, files);
        } else if path.extension().map_or(false, |ext| ext == "proto") {
            files.push(path.to_path_buf());
        }
    }
}
