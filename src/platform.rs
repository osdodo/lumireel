//! Shell conventions shared by agent startup, history commands and pasted paths.

use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

#[derive(Clone, Copy)]
enum Shell {
    Posix,
    PowerShell,
}

fn shell_kind() -> Shell {
    if cfg!(windows) {
        Shell::PowerShell
    } else {
        Shell::Posix
    }
}

pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        "powershell.exe".into()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if cfg!(target_os = "macos") {
                    "/bin/zsh"
                } else {
                    "/bin/sh"
                }
                .into()
            })
    }
}

pub fn shell_arguments(command: &str) -> Vec<String> {
    arguments(command, shell_kind())
}

fn arguments(command: &str, shell: Shell) -> Vec<String> {
    let command = command.trim();
    let interactive = command.is_empty() || command == "$SHELL";
    match (shell, interactive) {
        (Shell::PowerShell, true) => vec![
            "-NoLogo".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
        ],
        (Shell::PowerShell, false) => vec![
            "-NoLogo".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-Command".into(),
            command.into(),
        ],
        (Shell::Posix, true) => vec!["-l".into()],
        // Interactive login loads user PATH setup (nvm, etc.) before starting the agent.
        (Shell::Posix, false) => vec!["-i".into(), "-l".into(), "-c".into(), command.into()],
    }
}

pub fn prepend_path(bin: &Path, inherited: Option<&OsStr>) -> OsString {
    let mut paths = vec![bin.to_path_buf()];
    if let Some(inherited) = inherited.filter(|p| !p.is_empty()) {
        paths.extend(std::env::split_paths(inherited));
    }
    match std::env::join_paths(paths) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("could not add {} to PATH: {error}", bin.display());
            inherited.unwrap_or_default().to_os_string()
        }
    }
}

pub fn quote_shell_word(text: &str) -> String {
    quote(text, shell_kind())
}

fn quote(text: &str, shell: Shell) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "-_./".contains(c))
    {
        return text.to_string();
    }
    let escaped = match shell {
        Shell::Posix => text.replace('\'', "'\\''"),
        Shell::PowerShell => text.replace('\'', "''"),
    };
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shell_startup_preserves_the_complete_agent_command() {
        let command = "codex resume 'a session'";
        assert_eq!(
            arguments(command, Shell::PowerShell),
            ["-NoLogo", "-ExecutionPolicy", "Bypass", "-Command", command]
        );
        assert_eq!(
            arguments(command, Shell::Posix),
            ["-i", "-l", "-c", command]
        );
        assert_eq!(
            arguments("$SHELL", Shell::PowerShell),
            ["-NoLogo", "-ExecutionPolicy", "Bypass"]
        );
        assert_eq!(arguments("", Shell::Posix), ["-l"]);
    }

    #[test]
    fn quotes_preserve_spaces_apostrophes_and_shell_metacharacters() {
        let path = "C:\\Ada's files\\$notes; &draft.txt";
        assert_eq!(
            quote(path, Shell::PowerShell),
            "'C:\\Ada''s files\\$notes; &draft.txt'"
        );
        assert_eq!(quote("/tmp/it's", Shell::Posix), "'/tmp/it'\\''s'");
        for shell in [Shell::Posix, Shell::PowerShell] {
            assert_eq!(quote("", shell), "''");
            assert_eq!(quote("@args", shell), "'@args'");
            assert_eq!(quote("a,b", shell), "'a,b'");
        }
    }

    #[test]
    fn path_round_trips_with_platform_separators_and_spaces() {
        let bin = PathBuf::from(if cfg!(windows) {
            r"C:\Program Files\LumiReel"
        } else {
            "/opt/Lumi Reel/bin"
        });
        let original = [
            PathBuf::from("first folder"),
            PathBuf::from("second-folder"),
        ];
        let inherited = std::env::join_paths(&original).unwrap();
        let result = prepend_path(&bin, Some(&inherited));
        assert_eq!(
            std::env::split_paths(&result).collect::<Vec<_>>(),
            [vec![bin.clone()], original.to_vec()].concat()
        );
        assert_eq!(
            std::env::split_paths(&prepend_path(&bin, None)).collect::<Vec<_>>(),
            [bin]
        );
    }
}
