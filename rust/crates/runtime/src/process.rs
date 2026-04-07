use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[must_use]
pub fn command_exists(command: &str) -> bool {
    resolve_command_path(command).is_some()
}

#[must_use]
pub fn resolve_command_path(command: &str) -> Option<PathBuf> {
    resolve_command_path_with_env(command, env::var_os("PATH"), platform_pathext())
}

fn platform_pathext() -> Option<OsString> {
    #[cfg(windows)]
    {
        env::var_os("PATHEXT").or_else(|| Some(OsString::from(".COM;.EXE;.BAT;.CMD")))
    }

    #[cfg(not(windows))]
    {
        None
    }
}

fn resolve_command_path_with_env(
    command: &str,
    path_var: Option<OsString>,
    pathext_var: Option<OsString>,
) -> Option<PathBuf> {
    let command_path = Path::new(command);
    let pathexts = parse_pathexts(pathext_var);

    if has_path_component(command_path) {
        return candidate_paths(command_path, &pathexts)
            .into_iter()
            .find(|path| is_executable_file(path));
    }

    let path_var = path_var?;

    env::split_paths(&path_var).find_map(|dir| {
        candidate_paths(&dir.join(command_path), &pathexts)
            .into_iter()
            .find(|path| is_executable_file(path))
    })
}

fn has_path_component(path: &Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}

fn candidate_paths(path: &Path, pathexts: &[String]) -> Vec<PathBuf> {
    let mut candidates = vec![path.to_path_buf()];
    if cfg!(windows) && path.extension().is_none() {
        candidates.extend(pathexts.iter().map(|ext| {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(ext);
            PathBuf::from(candidate)
        }));
    }
    candidates
}

fn parse_pathexts(pathext_var: Option<OsString>) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }

    pathext_var
        .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
        .to_string_lossy()
        .split(';')
        .filter(|ext| !ext.trim().is_empty())
        .map(|ext| ext.trim().to_string())
        .collect()
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        path.metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_command_path_with_env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("openyak-runtime-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("temp test dir should create");
        dir
    }

    #[cfg(windows)]
    #[test]
    fn detects_windows_commands_via_pathext() {
        use std::ffi::OsString;

        let dir = temp_test_dir("command-exists");
        let exe = dir.join("gh.EXE");
        fs::write(&exe, "").expect("test exe should write");

        let exists = resolve_command_path_with_env(
            "gh",
            Some(dir.as_os_str().to_os_string()),
            Some(OsString::from(".COM;.EXE;.BAT;.CMD")),
        )
        .is_some();
        fs::remove_dir_all(&dir).expect("temp test dir should clean up");

        assert!(exists);
    }

    #[cfg(not(windows))]
    #[test]
    fn detects_commands_from_path_entries() {
        let dir = temp_test_dir("command-exists");
        let tool = dir.join("gh");
        fs::write(&tool, "").expect("test tool should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&tool)
                .expect("test tool metadata should load")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&tool, permissions).expect("test tool permissions should update");
        }

        let exists =
            resolve_command_path_with_env("gh", Some(dir.as_os_str().to_os_string()), None)
                .is_some();
        fs::remove_dir_all(&dir).expect("temp test dir should clean up");

        assert!(exists);
    }

    #[test]
    fn returns_false_when_path_is_missing() {
        assert!(resolve_command_path_with_env("gh", None, None).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn resolves_first_matching_windows_command_in_path_order() {
        use std::ffi::OsString;

        let first_dir = temp_test_dir("command-resolve-first");
        let second_dir = temp_test_dir("command-resolve-second");
        let first = first_dir.join("gh.CMD");
        let second = second_dir.join("gh.EXE");
        fs::write(&first, "").expect("first command should write");
        fs::write(&second, "").expect("second command should write");

        let path_var = std::env::join_paths([first_dir.as_path(), second_dir.as_path()])
            .expect("path should join");
        let resolved = resolve_command_path_with_env(
            "gh",
            Some(path_var),
            Some(OsString::from(".COM;.EXE;.BAT;.CMD")),
        )
        .expect("command should resolve");

        fs::remove_dir_all(&first_dir).expect("first temp dir should clean up");
        fs::remove_dir_all(&second_dir).expect("second temp dir should clean up");

        assert_eq!(resolved, first);
    }
}
