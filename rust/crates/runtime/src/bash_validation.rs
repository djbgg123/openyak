use std::sync::OnceLock;

use regex::Regex;

use crate::permissions::PermissionMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BashCommandValidation {
    Allow,
    Deny(BashCommandDenial),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BashCommandDenial {
    pub(crate) required_mode: PermissionMode,
    pub(crate) reason: String,
}

const READ_ONLY_COMMANDS: &[&str] = &[
    "awk",
    "basename",
    "b3sum",
    "cal",
    "cat",
    "cut",
    "date",
    "df",
    "diff",
    "dirname",
    "du",
    "echo",
    "env",
    "false",
    "file",
    "find",
    "free",
    "grep",
    "head",
    "hexdump",
    "jq",
    "less",
    "ls",
    "md5sum",
    "more",
    "od",
    "paste",
    "printenv",
    "printf",
    "pwd",
    "readlink",
    "realpath",
    "rg",
    "sed",
    "sha256sum",
    "sort",
    "stat",
    "strings",
    "tail",
    "test",
    "tree",
    "tr",
    "true",
    "type",
    "uname",
    "uniq",
    "uptime",
    "wc",
    "where",
    "which",
    "whoami",
    "xxd",
    "yq",
];

const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "blame",
    "cat-file",
    "describe",
    "diff",
    "log",
    "ls-files",
    "ls-tree",
    "reflog",
    "rev-parse",
    "shortlog",
    "show",
    "status",
];

const WRITE_COMMANDS: &[&str] = &[
    "cp", "dd", "install", "ln", "mkdir", "mkfifo", "mknod", "mv", "rm", "rmdir", "shred", "tee",
    "touch", "truncate",
];

const STATE_CHANGING_COMMANDS: &[&str] = &[
    "apt",
    "apt-get",
    "brew",
    "bun",
    "cargo",
    "chgrp",
    "chmod",
    "choco",
    "chown",
    "crontab",
    "dnf",
    "docker",
    "gem",
    "go",
    "groupadd",
    "groupdel",
    "halt",
    "kill",
    "killall",
    "launchctl",
    "mount",
    "netsh",
    "npm",
    "pacman",
    "pip",
    "pip3",
    "pkill",
    "pnpm",
    "poweroff",
    "reboot",
    "reg",
    "rustup",
    "sc",
    "service",
    "shutdown",
    "scoop",
    "systemctl",
    "takeown",
    "umount",
    "useradd",
    "userdel",
    "usermod",
    "winget",
    "yarn",
    "yum",
];

const WORKSPACE_WRITE_MACHINE_COMMANDS: &[&str] = &[
    "apt",
    "apt-get",
    "brew",
    "chgrp",
    "choco",
    "chown",
    "crontab",
    "diskutil",
    "dnf",
    "fdisk",
    "format",
    "groupadd",
    "groupdel",
    "halt",
    "icacls",
    "launchctl",
    "mkfs",
    "mount",
    "netsh",
    "pacman",
    "parted",
    "poweroff",
    "reboot",
    "reg",
    "sc",
    "service",
    "shutdown",
    "scoop",
    "systemctl",
    "takeown",
    "umount",
    "useradd",
    "userdel",
    "usermod",
    "winget",
    "wipefs",
    "yum",
];

#[must_use]
pub(crate) fn validate_bash_command(command: &str, mode: PermissionMode) -> BashCommandValidation {
    if command.trim().is_empty() {
        return deny(mode, "shell command is empty or whitespace-only");
    }

    match mode {
        PermissionMode::ReadOnly => validate_read_only(command),
        PermissionMode::WorkspaceWrite => validate_workspace_write(command),
        PermissionMode::Prompt => deny(
            PermissionMode::DangerFullAccess,
            "bash requires confirmation in prompt mode",
        ),
        PermissionMode::DangerFullAccess | PermissionMode::Allow => BashCommandValidation::Allow,
    }
}

fn validate_read_only(command: &str) -> BashCommandValidation {
    if has_write_redirection(command) {
        return deny(
            PermissionMode::WorkspaceWrite,
            "read-only mode blocks shell write redirection",
        );
    }

    for segment in command_segments(command) {
        let tokens = normalized_tokens(segment);
        if tokens.is_empty() {
            continue;
        }
        if command_name(&tokens[0]) == "sudo" {
            return deny(
                PermissionMode::WorkspaceWrite,
                "read-only mode blocks privileged shell command 'sudo'",
            );
        }

        let effective_tokens = effective_tokens(&tokens);
        let Some(first_command) = effective_tokens.first().map(|token| command_name(token)) else {
            continue;
        };

        if first_command == "sed" && has_in_place_flag(effective_tokens) {
            return deny(
                PermissionMode::WorkspaceWrite,
                "read-only mode blocks in-place editing via 'sed -i'",
            );
        }

        if first_command == "git" {
            if let Some(subcommand) = git_subcommand(effective_tokens) {
                if !READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
                    return deny(
                        PermissionMode::WorkspaceWrite,
                        format!(
                            "read-only mode blocks git subcommand '{subcommand}' because it mutates repository state"
                        ),
                    );
                }
            }
            continue;
        }

        if READ_ONLY_COMMANDS.contains(&first_command) {
            continue;
        }

        if WRITE_COMMANDS.contains(&first_command) {
            return deny(
                PermissionMode::WorkspaceWrite,
                format!("read-only mode blocks shell write command '{first_command}'"),
            );
        }

        if STATE_CHANGING_COMMANDS.contains(&first_command) {
            return deny(
                PermissionMode::WorkspaceWrite,
                format!("read-only mode blocks state-changing shell command '{first_command}'"),
            );
        }

        return deny(
            PermissionMode::WorkspaceWrite,
            format!(
                "read-only mode only allows known read-only shell commands; '{first_command}' is not classified as read-only"
            ),
        );
    }

    BashCommandValidation::Allow
}

fn validate_workspace_write(command: &str) -> BashCommandValidation {
    if let Some(reason) = destructive_reason(command) {
        return deny(
            PermissionMode::DangerFullAccess,
            format!(
                "workspace-write mode blocks destructive shell pattern ({reason}); use danger-full-access"
            ),
        );
    }

    if raw_device_write_detected(command) {
        return deny(
            PermissionMode::DangerFullAccess,
            "workspace-write mode blocks raw device writes; use danger-full-access",
        );
    }

    for segment in command_segments(command) {
        let tokens = normalized_tokens(segment);
        if tokens.is_empty() {
            continue;
        }
        if command_name(&tokens[0]) == "sudo" {
            return deny(
                PermissionMode::DangerFullAccess,
                "workspace-write mode blocks privileged shell command 'sudo'; use danger-full-access",
            );
        }

        let effective_tokens = effective_tokens(&tokens);
        let Some(first_command) = effective_tokens.first().map(|token| command_name(token)) else {
            continue;
        };

        if WORKSPACE_WRITE_MACHINE_COMMANDS.contains(&first_command) {
            return deny(
                PermissionMode::DangerFullAccess,
                format!(
                    "workspace-write mode blocks system administration command '{first_command}'; use danger-full-access"
                ),
            );
        }
    }

    BashCommandValidation::Allow
}

fn deny(required_mode: PermissionMode, reason: impl Into<String>) -> BashCommandValidation {
    BashCommandValidation::Deny(BashCommandDenial {
        required_mode,
        reason: reason.into(),
    })
}

fn command_segments(command: &str) -> impl Iterator<Item = &str> + '_ {
    segment_separator_regex()
        .split(command)
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
}

fn segment_separator_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\|\||&&|;|\|").expect("valid command separator regex"))
}

fn normalized_tokens(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect()
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|char| matches!(char, '"' | '\'' | '(' | ')' | '{' | '}'))
        .to_ascii_lowercase()
}

fn command_name(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

fn effective_tokens(tokens: &[String]) -> &[String] {
    if tokens.first().map(|token| command_name(token)) != Some("sudo") {
        return tokens;
    }

    let mut index = 1;
    while index < tokens.len() && tokens[index].starts_with('-') {
        index += 1;
    }
    &tokens[index..]
}

fn git_subcommand(tokens: &[String]) -> Option<String> {
    let mut index = 1;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token == "--" {
            return tokens.get(index + 1).cloned();
        }
        if token == "-c" || token == "-C" || token == "--git-dir" || token == "--work-tree" {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(tokens[index].clone());
    }
    None
}

fn has_in_place_flag(tokens: &[String]) -> bool {
    tokens.iter().skip(1).any(|token| {
        token == "-i"
            || token.starts_with("-i")
            || token == "--in-place"
            || token.starts_with("--in-place=")
    })
}

fn has_write_redirection(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    for character in command.chars() {
        match character {
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '>' if !in_single_quote && !in_double_quote => return true,
            _ => {}
        }
    }

    false
}

fn destructive_reason(command: &str) -> Option<&'static str> {
    for segment in command_segments(command) {
        let tokens = normalized_tokens(segment);
        let effective_tokens = effective_tokens(&tokens);
        if let Some(reason) = recursive_rm_reason(effective_tokens) {
            return Some(reason);
        }
    }

    let lowered = command.to_ascii_lowercase();
    if lowered.contains(":(){ :|:& };:") {
        return Some("fork bomb");
    }
    if lowered.contains("chmod -r 777") || lowered.contains("chmod -r 000") {
        return Some("destructive recursive permission reset");
    }

    None
}

fn recursive_rm_reason(tokens: &[String]) -> Option<&'static str> {
    if tokens.first().map(|token| command_name(token)) != Some("rm") {
        return None;
    }

    let recursive = tokens
        .iter()
        .skip(1)
        .any(|token| token.starts_with('-') && token.contains('r'));
    let force = tokens
        .iter()
        .skip(1)
        .any(|token| token.starts_with('-') && token.contains('f'));
    if !recursive || !force {
        return None;
    }

    let targets: Vec<&str> = tokens
        .iter()
        .skip(1)
        .filter(|token| !token.starts_with('-'))
        .map(String::as_str)
        .collect();

    if targets.iter().any(|target| matches!(*target, "/" | "/*")) {
        return Some("root recursive deletion");
    }
    if targets
        .iter()
        .any(|target| matches!(*target, "~" | "~/" | "~/*"))
    {
        return Some("home-directory recursive deletion");
    }
    if targets
        .iter()
        .any(|target| matches!(*target, "." | "./" | "*" | "./*"))
    {
        return Some("current-directory recursive deletion");
    }

    None
}

fn raw_device_write_detected(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    lowered.contains("of=/dev/")
        || lowered.contains(">/dev/")
        || lowered.contains("> /dev/")
        || lowered.contains("\\\\.\\physicaldrive")
}

#[cfg(test)]
mod tests {
    use super::{validate_bash_command, BashCommandValidation};
    use crate::permissions::PermissionMode;

    #[test]
    fn read_only_allows_known_read_only_commands() {
        assert_eq!(
            validate_bash_command("cat src/main.rs", PermissionMode::ReadOnly),
            BashCommandValidation::Allow
        );
        assert_eq!(
            validate_bash_command("git status", PermissionMode::ReadOnly),
            BashCommandValidation::Allow
        );
        assert_eq!(
            validate_bash_command("grep -r needle .", PermissionMode::ReadOnly),
            BashCommandValidation::Allow
        );
    }

    #[test]
    fn read_only_denies_write_like_commands() {
        assert!(matches!(
            validate_bash_command("touch notes.txt", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("write command 'touch'")
                    && denial.required_mode == PermissionMode::WorkspaceWrite
        ));
        assert!(matches!(
            validate_bash_command("echo hello > notes.txt", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("write redirection")
        ));
        assert!(matches!(
            validate_bash_command("sed -i 's/a/b/' file.txt", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("sed -i")
        ));
        assert!(matches!(
            validate_bash_command("git commit -m test", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("git subcommand 'commit'")
        ));
    }

    #[test]
    fn read_only_denies_chained_write_commands() {
        assert!(matches!(
            validate_bash_command("cat Cargo.toml | tee copy.txt", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("write command 'tee'")
        ));
        assert!(matches!(
            validate_bash_command("echo ok && rm notes.txt", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("write command 'rm'")
        ));
    }

    #[test]
    fn workspace_write_denies_destructive_or_machine_level_commands() {
        assert!(matches!(
            validate_bash_command("rm -rf /", PermissionMode::WorkspaceWrite),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("destructive shell pattern")
                    && denial.reason.contains("danger-full-access")
                    && denial.required_mode == PermissionMode::DangerFullAccess
        ));
        assert!(matches!(
            validate_bash_command(
                "dd if=image.iso of=/dev/sda bs=4m",
                PermissionMode::WorkspaceWrite
            ),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("raw device writes")
        ));
        assert!(matches!(
            validate_bash_command("systemctl restart sshd", PermissionMode::WorkspaceWrite),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("system administration command 'systemctl'")
        ));
        assert!(matches!(
            validate_bash_command("sudo rm -rf build", PermissionMode::WorkspaceWrite),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("privileged shell command 'sudo'")
        ));
    }

    #[test]
    fn workspace_write_allows_ordinary_workspace_commands() {
        assert_eq!(
            validate_bash_command(
                "mkdir -p build && touch build/log.txt",
                PermissionMode::WorkspaceWrite
            ),
            BashCommandValidation::Allow
        );
        assert_eq!(
            validate_bash_command("rm -rf target/debug", PermissionMode::WorkspaceWrite),
            BashCommandValidation::Allow
        );
    }

    #[test]
    fn malformed_input_is_rejected_in_every_mode() {
        assert!(matches!(
            validate_bash_command("   ", PermissionMode::ReadOnly),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("empty or whitespace-only")
                    && denial.required_mode == PermissionMode::ReadOnly
        ));
        assert!(matches!(
            validate_bash_command("   ", PermissionMode::DangerFullAccess),
            BashCommandValidation::Deny(denial)
                if denial.reason.contains("empty or whitespace-only")
                    && denial.required_mode == PermissionMode::DangerFullAccess
        ));
    }

    #[test]
    fn prompt_mode_still_requires_confirmation_for_non_malformed_commands() {
        assert!(matches!(
            validate_bash_command("git status", PermissionMode::Prompt),
            BashCommandValidation::Deny(denial)
                if denial.reason == "bash requires confirmation in prompt mode"
                    && denial.required_mode == PermissionMode::DangerFullAccess
        ));
    }
}
