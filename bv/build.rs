use clap::CommandFactory;
use eyre::{Context, Result};
use std::path::{Path, PathBuf};
use std::{env, fs};

const PROTO_DIRS: &[&str] = &["./proto"];
const EXCLUDE_DIRS: &[&str] = &[".direnv"];

mod bv {
    include!("src/bv_cli.rs");
}

mod bib {
    include!("src/bib_cli.rs");
}
fn main() -> Result<()> {
    let mut bv = bv::App::command();
    let mut bib = bib::App::command();

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut iter = out_dir.ancestors().into_iter().step_by(3);
    iter.next();
    let complete_out = iter.next().unwrap().to_path_buf().join("sh_complete");
    fs::create_dir_all(&complete_out).unwrap();
    for shell in [clap_complete::Shell::Bash, clap_complete::Shell::Zsh] {
        clap_complete::generate_to(shell, &mut bv, "bv", &complete_out)
            .unwrap_or_else(|_| panic!("failed to generate {shell} complete for bv"));
        clap_complete::generate_to(shell, &mut bib, "bib", &complete_out)
            .unwrap_or_else(|_| panic!("failed to generate {shell} complete for bib"));
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&proto_files()?, PROTO_DIRS)
        .context("Failed to compile protos")
}

fn proto_files() -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in PROTO_DIRS {
        find_recursive(Path::new(dir), &mut files)?;
    }
    Ok(files)
}

fn find_recursive(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let is_excluded = || {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| EXCLUDE_DIRS.contains(&name))
            .unwrap_or_default()
    };

    if !path.is_dir() || is_excluded() {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            find_recursive(&path, files)?;
        } else if path.extension().map_or(false, |ext| ext == "proto") {
            files.push(path.to_path_buf());
        }
    }

    Ok(())
}
