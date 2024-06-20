include!("src/cli.rs");
use clap::CommandFactory;
use std::path::Path;
use std::{env, fs};

const PROTO_DIRS: &[&str] = &["./proto"];

fn main() {
    let mut app = App::command();

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut iter = out_dir.ancestors().into_iter().step_by(3);
    iter.next();
    let complete_out = iter.next().unwrap().to_path_buf().join("sh_complete");
    fs::create_dir_all(&complete_out).unwrap();
    for shell in [clap_complete::Shell::Bash, clap_complete::Shell::Zsh] {
        clap_complete::generate_to(shell, &mut app, "bv", &complete_out)
            .unwrap_or_else(|_| panic!("failed to generate {shell} complete"));
    }

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
