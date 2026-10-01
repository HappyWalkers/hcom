//! Codex launch preprocessing — state access and bootstrap injection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::paths;

/// Add the hcom dir to Codex's workspace-write roots. A `-c` override is
/// ignored outside workspace-write, whereas `--add-dir` is fatal at startup
/// when the effective sandbox is read-only (e.g. workspace-write on Windows
/// without the Windows sandbox). The override replaces the whole list, so it
/// carries the user's roots from the CLI or `$CODEX_HOME/config.toml`; roots
/// set only in project or system config are dropped. `[permissions]` profiles
/// ignore these legacy roots, so there the hcom dir is not made writable.
pub fn ensure_hcom_writable(tokens: &[String], codex_home: Option<&Path>) -> Vec<String> {
    const ROOTS_KEY: &str = "sandbox_workspace_write.writable_roots";
    let end = tokens
        .iter()
        .position(|token| token == "--")
        .unwrap_or(tokens.len());
    let options = &tokens[..end];
    let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();

    let mut cli_roots = None;
    let mut i = 0;
    while i < options.len() {
        let token = options[i].as_str();
        // Respect an explicit --add-dir for the hcom dir.
        if (token == "--add-dir" && options.get(i + 1) == Some(&hcom_dir))
            || token.strip_prefix("--add-dir=") == Some(hcom_dir.as_str())
        {
            return tokens.to_vec();
        }
        let raw = if matches!(token, "-c" | "--config") {
            i += 1;
            options.get(i).map(String::as_str)
        } else {
            token
                .strip_prefix("--config=")
                .or_else(|| token.strip_prefix("-c="))
                .or_else(|| token.strip_prefix("-c"))
        };
        let override_kv = raw.and_then(|raw| raw.split_once('='));
        // A whole-table override would be replaced by the dotted one below.
        if override_kv.is_some_and(|(key, _)| key.trim() == "sandbox_workspace_write") {
            return tokens.to_vec();
        }
        if let Some((key, value)) = override_kv
            && key.trim() == ROOTS_KEY
        {
            match parse_roots(value) {
                Some(roots) => cli_roots = Some(roots),
                // Codex rejects a malformed override itself.
                None => return tokens.to_vec(),
            }
        }
        i += 1;
    }

    let mut roots = cli_roots
        .or_else(|| codex_home.and_then(config_writable_roots))
        .unwrap_or_default();
    if roots.contains(&hcom_dir) {
        return tokens.to_vec();
    }
    roots.push(hcom_dir);
    let value = toml::Value::Array(roots.into_iter().map(toml::Value::String).collect());
    let mut result = tokens.to_vec();
    // Later overrides win, so this replaces any user override above.
    crate::hooks::runtime::insert_before_separator(
        &mut result,
        ["-c".to_string(), format!("{ROOTS_KEY}={value}")],
    );
    result
}

fn parse_roots(value: &str) -> Option<Vec<String>> {
    let table: toml::Table = toml::from_str(&format!("x = {value}")).ok()?;
    string_array(table.get("x")?)
}

fn string_array(value: &toml::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// Top-level `sandbox_workspace_write.writable_roots` from `config.toml`.
fn config_writable_roots(codex_home: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(codex_home.join("config.toml")).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let roots = string_array(
        table
            .get("sandbox_workspace_write")?
            .get("writable_roots")?,
    )?;
    // Codex resolves these against the config file; a CLI override would not.
    Some(
        roots
            .into_iter()
            .map(|root| codex_home.join(root).to_string_lossy().into_owned())
            .collect(),
    )
}

/// Resolve the Codex state directory from the effective child launch
/// environment, including values supplied through `~/.hcom/env` or `--env`.
pub(crate) fn resolve_codex_home_from_env(
    env: &HashMap<String, String>,
    launch_dir: &Path,
) -> Option<(PathBuf, bool)> {
    // `dirs::home_dir()` reads HOME on Unix but uses the platform profile API
    // on Windows. Reproduce that distinction from the child's effective env.
    #[cfg(windows)]
    let default_home = dirs::home_dir();
    #[cfg(not(windows))]
    let default_home = env
        .get("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir);
    resolve_codex_home_from_env_with(
        env,
        launch_dir,
        default_home.or_else(|| Some(crate::runtime_env::tool_config_root())),
        cfg!(windows),
    )
}

fn resolve_codex_home_from_env_with(
    env: &HashMap<String, String>,
    launch_dir: &Path,
    default_home: Option<PathBuf>,
    case_insensitive: bool,
) -> Option<(PathBuf, bool)> {
    let configured = if case_insensitive {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("CODEX_HOME"))
            .map(|(_, value)| value.as_str())
    } else {
        env.get("CODEX_HOME").map(String::as_str)
    };
    if let Some(value) = configured.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return Some((
            if path.is_absolute() {
                path
            } else {
                launch_dir.join(path)
            },
            true,
        ));
    }
    default_home.map(|home| (home.join(".codex"), false))
}

/// Probe whether `CODEX_HOME` is writable before launching codex.
///
/// When hcom is invoked from inside a sandboxed parent codex (e.g.
/// `--sandbox workspace-write`), seatbelt/landlock is inherited by the entire
/// process chain. The child codex then fails to init its state DB
/// (SQLITE_READONLY) and hangs on an interactive "Repair Codex local data
/// now? [y/N]:" prompt with no human to answer.
///
/// Catching this synchronously and exiting non-zero with a permission-denied
/// message lets the parent codex's existing sandbox-escalation flow ("approve
/// to run unsandboxed?") trigger naturally on the failed shell command,
/// instead of leaving a brick agent behind.
pub(crate) fn ensure_codex_home_writable_at(codex_home: &Path, explicit_env: bool) -> Result<()> {
    let probe_dir = if codex_home.exists() {
        codex_home
    } else if explicit_env {
        return Ok(());
    } else {
        let Some(parent) = codex_home.ancestors().find(|p| p.exists()) else {
            return Ok(());
        };
        parent
    };
    let probe = probe_dir.join(".hcom_writable_probe");
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => {
            use std::io::ErrorKind;
            let denied = matches!(
                e.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            );
            if !denied {
                return Ok(());
            }
            bail!(
                "Operation not permitted: cannot write to CODEX_HOME ({}): {}\n\
                 The current process is running inside a sandbox that denies writes \
                 to the codex state directory. If this hcom command was invoked by \
                 a sandboxed agent (e.g. codex --sandbox workspace-write), approve \
                 it to run unsandboxed and retry.",
                codex_home.display(),
                e
            );
        }
    }
}

/// Add hcom bootstrap to codex developer_instructions.
///
/// Builds full bootstrap and adds via `-c developer_instructions=...` flag.
/// If user also provided developer_instructions, bootstrap comes first,
/// then separator, then user content.
///
pub fn add_codex_developer_instructions(
    codex_args: &[String],
    bootstrap_text: &str,
) -> Vec<String> {
    let mut existing_dev_instructions: Option<String> = None;
    let mut remaining = Vec::with_capacity(codex_args.len() + 2);
    let mut i = 0;
    while i < codex_args.len() {
        let token = &codex_args[i];
        if token == "--" {
            remaining.extend_from_slice(&codex_args[i..]);
            break;
        }
        if let Some(value) = token
            .strip_prefix("-c=developer_instructions=")
            .or_else(|| token.strip_prefix("--config=developer_instructions="))
        {
            existing_dev_instructions = Some(value.to_string());
            i += 1;
            continue;
        }
        if (token == "-c" || token == "--config")
            && i + 1 < codex_args.len()
            && let Some(value) = codex_args[i + 1].strip_prefix("developer_instructions=")
        {
            existing_dev_instructions = Some(value.to_string());
            i += 2;
            continue;
        }
        remaining.push(token.clone());
        i += 1;
    }

    let combined = if let Some(existing) = existing_dev_instructions {
        // Codex accepts a TOML string, falling back to raw text on parse errors.
        let existing = existing
            .parse::<toml::Value>()
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or(existing);
        format!("{}\n---\n{}", bootstrap_text, existing)
    } else {
        bootstrap_text.to_string()
    };

    // `-c` values are TOML expressions. A raw multiline string happened to be
    // accepted by older Codex builds but is ignored by current builds,
    // silently dropping the hcom identity bootstrap. Serialize a real TOML
    // string so quotes, backslashes, and newlines survive on every platform.
    let encoded = toml::Value::String(combined).to_string();
    crate::hooks::runtime::insert_before_separator(
        &mut remaining,
        [
            "-c".to_string(),
            format!("developer_instructions={encoded}"),
        ],
    );
    remaining
}

/// Allow terminal Unix sockets in workspace-write without selecting Codex's
/// sandbox or approval policy. Explicit CLI network settings take precedence.
fn ensure_terminal_socket_access(args: &[String]) -> Vec<String> {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut i = 0;
    while i < end {
        let raw = if matches!(args[i].as_str(), "-c" | "--config") {
            i += 1;
            args.get(i).filter(|_| i < end).map(String::as_str)
        } else {
            args[i]
                .strip_prefix("--config=")
                .or_else(|| args[i].strip_prefix("-c="))
                .or_else(|| args[i].strip_prefix("-c"))
        };
        if let Some(raw) = raw
            && let Some((key, _)) = raw.split_once('=')
            && matches!(
                key.trim(),
                "sandbox_workspace_write" | "sandbox_workspace_write.network_access"
            )
        {
            return args.to_vec();
        }
        i += 1;
    }
    let mut result = args.to_vec();
    // Seatbelt otherwise denies terminal Unix sockets (kitty/tmux).
    crate::hooks::runtime::insert_before_separator(
        &mut result,
        [
            "-c".to_string(),
            "sandbox_workspace_write.network_access=true".to_string(),
        ],
    );
    result
}

/// Add the state directory, terminal socket access and identity bootstrap.
/// Codex's own config and CLI flags select sandbox and approval policy.
pub fn preprocess_codex_args(
    codex_args: &[String],
    bootstrap_text: &str,
    codex_home: Option<&Path>,
) -> Vec<String> {
    let args = ensure_hcom_writable(codex_args, codex_home);
    let args = ensure_terminal_socket_access(&args);
    add_codex_developer_instructions(&args, bootstrap_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    /// Roots from the last `-c sandbox_workspace_write.writable_roots=...`.
    fn injected_roots(result: &[String]) -> Vec<String> {
        result
            .iter()
            .rev()
            .find_map(|arg| arg.strip_prefix("sandbox_workspace_write.writable_roots="))
            .and_then(parse_roots)
            .unwrap_or_default()
    }

    fn has_hcom_writable_dir(result: &[String]) -> bool {
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        injected_roots(result).contains(&hcom_dir)
    }

    fn init_config() {
        // Config::init is idempotent-ish but needs to be called before paths::hcom_dir()
        crate::config::Config::init();
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_adds_writable_root() {
        init_config();
        let tokens = s(&["--model", "gpt-6-luna"]);
        let result = ensure_hcom_writable(&tokens, None);
        assert_eq!(&result[..tokens.len()], &tokens);
        assert!(
            has_hcom_writable_dir(&result),
            "missing hcom directory: {result:?}"
        );
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_respects_explicit_add_dir() {
        init_config();
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        let tokens = vec!["--add-dir".to_string(), hcom_dir];
        let result = ensure_hcom_writable(&tokens, None);
        assert_eq!(result, tokens, "explicit --add-dir must suppress injection");
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_respects_user_writable_roots() {
        init_config();
        let tokens = s(&[
            "--sandbox",
            "workspace-write",
            "-c",
            r#"sandbox_workspace_write.writable_roots=["/my/dir"]"#,
        ]);
        let result = ensure_hcom_writable(&tokens, None);
        assert_eq!(&result[..tokens.len()], &tokens);
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        assert_eq!(
            injected_roots(&result),
            vec!["/my/dir".to_string(), hcom_dir]
        );
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_keeps_config_roots() {
        init_config();
        let codex_home = tempfile::tempdir().unwrap();
        let root = codex_home
            .path()
            .join("work")
            .to_string_lossy()
            .into_owned();
        let roots = toml::Value::Array(vec![toml::Value::String(root.clone())]);
        std::fs::write(
            codex_home.path().join("config.toml"),
            format!("[sandbox_workspace_write]\nwritable_roots = {roots}\n"),
        )
        .unwrap();
        let result = ensure_hcom_writable(&[], Some(codex_home.path()));
        let hcom_dir = paths::hcom_dir().to_string_lossy().to_string();
        assert_eq!(injected_roots(&result), vec![root, hcom_dir]);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_resolves_relative_config_roots() {
        init_config();
        let codex_home = tempfile::tempdir().unwrap();
        std::fs::write(
            codex_home.path().join("config.toml"),
            "[sandbox_workspace_write]\nwritable_roots = [\"rel\"]\n",
        )
        .unwrap();
        let result = ensure_hcom_writable(&[], Some(codex_home.path()));
        let expected = codex_home.path().join("rel").to_string_lossy().into_owned();
        assert_eq!(injected_roots(&result)[0], expected);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_leaves_table_override() {
        init_config();
        let tokens = s(&["-c", r#"sandbox_workspace_write={writable_roots=["/x"]}"#]);
        assert_eq!(ensure_hcom_writable(&tokens, None), tokens);
    }

    #[test]
    #[serial]
    fn test_ensure_hcom_writable_never_adds_add_dir() {
        // `--add-dir` is fatal when Codex's effective sandbox is read-only.
        init_config();
        let result = ensure_hcom_writable(&s(&["-s", "read-only"]), None);
        assert!(!result.iter().any(|arg| arg.starts_with("--add-dir")));
        assert!(has_hcom_writable_dir(&result));
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_probes_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        ensure_codex_home_writable_at(dir.path(), true).unwrap();

        assert!(!dir.path().join(".hcom_writable_probe").exists());
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_skips_missing_explicit_home() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("missing-codex-home");
        ensure_codex_home_writable_at(&codex_home, true).unwrap();

        assert!(!codex_home.exists());
        assert!(!dir.path().join(".hcom_writable_probe").exists());
    }

    #[test]
    #[serial]
    fn test_ensure_codex_home_writable_probes_parent_when_default_home_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        ensure_codex_home_writable_at(&home.join(".codex"), false).unwrap();

        assert!(!home.join(".codex").exists());
        assert!(!home.join(".hcom_writable_probe").exists());
    }

    #[test]
    fn test_resolve_codex_home_uses_effective_child_env_override() {
        let env = HashMap::from([
            ("HOME".to_string(), "/readonly-parent-home".to_string()),
            (
                "CODEX_HOME".to_string(),
                "/writable-child-codex-home".to_string(),
            ),
        ]);

        let resolved = resolve_codex_home_from_env(&env, Path::new("/workspace")).unwrap();

        assert_eq!(resolved.0, PathBuf::from("/writable-child-codex-home"));
        assert!(resolved.1);
    }

    #[test]
    fn test_resolve_codex_home_uses_platform_home_not_child_home_env() {
        let env = HashMap::from([
            ("HOME".to_string(), "/different-child-home".to_string()),
            (
                "USERPROFILE".to_string(),
                r"C:\different-child-home".to_string(),
            ),
        ]);

        let resolved = resolve_codex_home_from_env_with(
            &env,
            Path::new("/workspace"),
            Some(PathBuf::from("/platform-home")),
            true,
        )
        .unwrap();

        assert_eq!(resolved, (PathBuf::from("/platform-home/.codex"), false));
    }

    #[test]
    fn test_resolve_codex_home_handles_windows_key_casing_and_child_cwd() {
        let env = HashMap::from([("Codex_Home".to_string(), "relative-home".to_string())]);

        let resolved = resolve_codex_home_from_env_with(
            &env,
            Path::new("/child-workspace"),
            Some(PathBuf::from("/platform-home")),
            true,
        )
        .unwrap();

        assert_eq!(
            resolved,
            (PathBuf::from("/child-workspace/relative-home"), true)
        );
    }

    #[test]
    fn test_add_developer_instructions_basic() {
        let args = s(&["-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(
            result,
            s(&["-m", "o3", "-c", "developer_instructions=\"BOOTSTRAP\""])
        );
    }

    #[test]
    fn test_add_developer_instructions_keeps_resume() {
        let args = s(&["resume"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "-c");
        assert_eq!(result[2], "developer_instructions=\"BOOTSTRAP\"");
    }

    #[test]
    fn test_add_developer_instructions_keeps_resume_session_first() {
        let args = s(&["resume", "thread-1", "--model", "gpt-5"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "thread-1");
        assert_eq!(result[2], "--model");
        assert_eq!(result[3], "gpt-5");
        assert_eq!(result[4], "-c");
        assert_eq!(result[5], "developer_instructions=\"BOOTSTRAP\"");
    }

    #[test]
    fn test_add_developer_instructions_keeps_fork_session_first_with_existing_config() {
        let args = s(&[
            "fork",
            "thread-1",
            "-c",
            "developer_instructions=OLD",
            "--model",
            "gpt-5",
        ]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "fork");
        assert_eq!(result[1], "thread-1");
        assert_eq!(result[2], "--model");
        assert_eq!(result[3], "gpt-5");
        assert_eq!(result[4], "-c");
        assert!(result[5].contains("BOOTSTRAP"));
        assert!(result[5].contains("OLD"));
    }

    #[test]
    fn test_add_developer_instructions_merge_existing() {
        let args = s(&["-c", "developer_instructions=USER_NOTES", "-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        let injected = result.last().unwrap();
        assert!(injected.contains("BOOTSTRAP"));
        assert!(injected.contains("USER_NOTES"));
        assert!(injected.contains("---"));
        let di_count = result
            .iter()
            .filter(|t| t.starts_with("developer_instructions="))
            .count();
        assert_eq!(di_count, 1);
    }

    #[test]
    fn test_add_developer_instructions_preserves_fork_subcommand() {
        let args = s(&["fork", "-m", "o3"]);
        let result = add_codex_developer_instructions(&args, "BOOTSTRAP");
        assert_eq!(result[0], "fork");
        assert_eq!(result[result.len() - 2], "-c");
    }

    #[test]
    #[serial]
    fn test_preprocess_resume_keeps_session_first() {
        init_config();
        let args = s(&["resume", "thread-1", "--model", "gpt-5"]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", None);
        assert_eq!(result[0], "resume");
        assert_eq!(result[1], "thread-1");
        assert!(result.iter().any(|t| t.contains("developer_instructions=")));
    }

    #[test]
    #[serial]
    fn preprocessing_preserves_native_permission_flags() {
        init_config();
        for args in [
            s(&[]),
            s(&["--sandbox", "read-only", "-a", "never"]),
            s(&["--sandbox=workspace-write", "--ask-for-approval=on-request"]),
            s(&["--yolo"]),
            s(&[
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "-c",
                "approval_policy=\"never\"",
            ]),
        ] {
            let result = preprocess_codex_args(&args, "BOOTSTRAP", None);
            assert_eq!(&result[..args.len()], &args);
            assert!(has_hcom_writable_dir(&result));
            assert!(result.contains(&"sandbox_workspace_write.network_access=true".to_string()));
            for flag in [
                "--sandbox",
                "--ask-for-approval",
                "-s",
                "-a",
                "--yolo",
                "--dangerously-bypass-approvals-and-sandbox",
            ] {
                assert_eq!(
                    result.iter().filter(|arg| arg.as_str() == flag).count(),
                    args.iter().filter(|arg| arg.as_str() == flag).count()
                );
            }
        }
    }

    #[test]
    #[serial]
    fn explicit_network_overrides_survive_launch_resume_and_fork() {
        init_config();
        for prefix in [
            s(&[]),
            s(&["resume", "session-id"]),
            s(&["fork", "session-id"]),
        ] {
            for override_args in [
                s(&["-c", "sandbox_workspace_write.network_access=false"]),
                s(&["--config", "sandbox_workspace_write.network_access=false"]),
                s(&["-c=sandbox_workspace_write.network_access=false"]),
                s(&["-csandbox_workspace_write.network_access=false"]),
                s(&["--config=sandbox_workspace_write.network_access=false"]),
                s(&["-c", "sandbox_workspace_write={network_access=false}"]),
            ] {
                let args = [prefix.clone(), override_args].concat();
                let result = preprocess_codex_args(&args, "BOOTSTRAP", None);
                assert_eq!(&result[..args.len()], &args);
                assert!(
                    !result.contains(&"sandbox_workspace_write.network_access=true".to_string())
                );
                // A whole-table override owns writable_roots too.
                let table = args.iter().any(|arg| arg.contains("={"));
                assert_eq!(has_hcom_writable_dir(&result), !table);
            }
        }
    }

    #[test]
    #[serial]
    fn preprocessing_preserves_positional_prompt() {
        init_config();
        let args = s(&[
            "--",
            "--sandbox=read-only",
            "-c=developer_instructions=literal prompt",
        ]);
        let result = preprocess_codex_args(&args, "BOOTSTRAP", None);
        let separator = result.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&result[separator..], &args);
        assert!(
            result[..separator]
                .iter()
                .any(|arg| arg.starts_with("developer_instructions="))
        );
    }

    #[test]
    #[serial]
    fn writable_directory_keeps_other_additional_directories() {
        init_config();
        let args = s(&["--sandbox", "workspace-write", "--add-dir", "/user/root"]);
        let result = ensure_hcom_writable(&args, None);
        assert_eq!(&result[..args.len()], &args);
        assert!(has_hcom_writable_dir(&result));
    }

    #[test]
    #[serial]
    fn resume_and_fork_preserve_user_developer_instructions() {
        init_config();
        for subcommand in ["resume", "fork"] {
            let args = s(&[
                subcommand,
                "session-id",
                "-c",
                r#"developer_instructions="User notes\nwith quotes \"here\"""#,
            ]);
            let result = preprocess_codex_args(&args, "BOOTSTRAP", None);
            assert_eq!(&result[..2], &args[..2]);
            let encoded = result
                .last()
                .unwrap()
                .strip_prefix("developer_instructions=")
                .unwrap();
            let decoded = encoded.parse::<toml::Value>().unwrap();
            assert_eq!(
                decoded.as_str(),
                Some("BOOTSTRAP\n---\nUser notes\nwith quotes \"here\"")
            );
        }
    }
}
