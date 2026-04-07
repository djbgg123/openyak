use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeLocations {
    pub user_home: PathBuf,
    pub codex_home: PathBuf,
    pub openyak_home: PathBuf,
    pub codex_home_from_env: bool,
    pub openyak_home_from_env: bool,
}

#[must_use]
pub fn home_locations() -> HomeLocations {
    let explicit_openyak_home = env_path("OPENYAK_CONFIG_HOME");
    let explicit_codex_home = env_path("CODEX_HOME");
    let user_home = explicit_openyak_home
        .as_deref()
        .map(derive_user_home_from_named_dir)
        .or_else(|| {
            explicit_codex_home
                .as_deref()
                .map(derive_user_home_from_named_dir)
        })
        .unwrap_or_else(platform_user_home_dir);
    let codex_home = explicit_codex_home
        .clone()
        .unwrap_or_else(|| user_home.join(".codex"));
    let openyak_home = explicit_openyak_home
        .clone()
        .unwrap_or_else(|| user_home.join(".openyak"));

    HomeLocations {
        user_home,
        codex_home,
        openyak_home,
        codex_home_from_env: explicit_codex_home.is_some(),
        openyak_home_from_env: explicit_openyak_home.is_some(),
    }
}

#[must_use]
pub fn platform_user_home_dir() -> PathBuf {
    #[cfg(windows)]
    let candidate = env_path("USERPROFILE")
        .or_else(home_drive_home_path)
        .or_else(|| env_path("HOME"));

    #[cfg(not(windows))]
    let candidate = env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .or_else(home_drive_home_path);

    candidate.unwrap_or_else(env::temp_dir)
}

#[must_use]
pub fn default_codex_home() -> PathBuf {
    home_locations().codex_home
}

#[must_use]
pub fn default_openyak_home() -> PathBuf {
    home_locations().openyak_home
}

#[must_use]
pub fn legacy_claw_home() -> PathBuf {
    // Preserve legacy migration support for pre-openyak claw config homes.
    env_path("CLAW_CONFIG_HOME").unwrap_or_else(|| home_locations().user_home.join(".claw"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_drive_home_path() -> Option<PathBuf> {
    let home_drive = env::var_os("HOMEDRIVE")?;
    let home_path = env::var_os("HOMEPATH")?;
    if home_drive.is_empty() || home_path.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(home_drive);
    path.push(home_path);
    Some(path)
}

fn derive_user_home_from_named_dir(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::home_locations;
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn prefers_userprofile_when_home_is_missing() {
        let _guard = crate::test_env_lock();
        let original_home = env::var_os("HOME");
        let original_userprofile = env::var_os("USERPROFILE");
        let original_homedrive = env::var_os("HOMEDRIVE");
        let original_homepath = env::var_os("HOMEPATH");
        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");
        let original_codex_home = env::var_os("CODEX_HOME");

        env::remove_var("HOME");
        env::remove_var("HOMEDRIVE");
        env::remove_var("HOMEPATH");
        env::remove_var("OPENYAK_CONFIG_HOME");
        env::remove_var("CODEX_HOME");
        env::set_var("USERPROFILE", r"C:\Users\tester");

        let locations = home_locations();
        let expected_user_home = PathBuf::from(r"C:\Users\tester");
        assert_eq!(locations.user_home, expected_user_home);
        assert_eq!(locations.codex_home, expected_user_home.join(".codex"));
        assert_eq!(locations.openyak_home, expected_user_home.join(".openyak"));

        restore_env("HOME", original_home);
        restore_env("USERPROFILE", original_userprofile);
        restore_env("HOMEDRIVE", original_homedrive);
        restore_env("HOMEPATH", original_homepath);
        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        restore_env("CODEX_HOME", original_codex_home);
    }

    #[test]
    fn openyak_config_home_takes_priority_for_user_root_derivation() {
        let _guard = crate::test_env_lock();
        let original_home = env::var_os("HOME");
        let original_userprofile = env::var_os("USERPROFILE");
        let original_openyak_config_home = env::var_os("OPENYAK_CONFIG_HOME");
        let original_codex_home = env::var_os("CODEX_HOME");

        env::remove_var("HOME");
        env::remove_var("USERPROFILE");
        env::set_var("OPENYAK_CONFIG_HOME", "/tmp/custom/.openyak");
        env::set_var("CODEX_HOME", "/elsewhere/.codex");

        let locations = home_locations();
        assert_eq!(locations.user_home, PathBuf::from("/tmp/custom"));
        assert_eq!(
            locations.openyak_home,
            PathBuf::from("/tmp/custom/.openyak")
        );
        assert_eq!(locations.codex_home, PathBuf::from("/elsewhere/.codex"));

        restore_env("HOME", original_home);
        restore_env("USERPROFILE", original_userprofile);
        restore_env("OPENYAK_CONFIG_HOME", original_openyak_config_home);
        restore_env("CODEX_HOME", original_codex_home);
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }
}
