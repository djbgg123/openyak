use std::path::{Path, PathBuf};

pub fn openyak_binary() -> PathBuf {
    let mut candidates = Vec::new();

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(profile_dir) = current_exe.parent().and_then(Path::parent) {
            candidates.push(profile_dir.join(openyak_binary_name()));
        }
    }

    candidates.push(PathBuf::from(env!("CARGO_BIN_EXE_openyak")));

    if let Some(repo_root) = current_exe_repo_root() {
        candidates.push(
            repo_root
                .join("rust")
                .join("target")
                .join("debug")
                .join(openyak_binary_name()),
        );
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "openyak binary should resolve from candidates rooted at {}",
                env!("CARGO_BIN_EXE_openyak")
            )
        })
}

#[allow(dead_code)]
pub fn repo_root() -> PathBuf {
    current_exe_repo_root()
        .or_else(compile_time_repo_root)
        .expect("repo root should resolve")
}

fn openyak_binary_name() -> &'static str {
    if cfg!(windows) {
        "openyak.exe"
    } else {
        "openyak"
    }
}

fn current_exe_repo_root() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|current_exe| {
        current_exe
            .ancestors()
            .skip(1)
            .find(|ancestor| ancestor.join("rust").join("Cargo.toml").is_file())
            .map(Path::to_path_buf)
    })
}

#[allow(dead_code)]
fn compile_time_repo_root() -> Option<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
}
