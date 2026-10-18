use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use overseer_core::Permissions;
use serde::Deserialize;
use serde_json::{Value, json};

pub const MAX_READ_LINES: usize = 400;
pub const MAX_READ_BYTES: usize = 20 * 1024;
pub const MAX_COMMAND_BYTES: usize = 8 * 1024;
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Descriptions of tools offered to the model, filtered by permissions.
pub fn tool_definitions(permissions: Permissions) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files and directories in a directory relative to the repository root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path relative to repository root (defaults to '.' for root)."
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents with line numbers. Output is capped to 400 lines and 20 KB per call.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to repository root."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "1-based line number to start reading from (defaults to 1)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read (capped to 400)."
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
    ];

    if permissions >= Permissions::WriteWorkdir {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file relative to the repository root. Creates parent directories if needed and overwrites existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to repository root."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full file content to write."
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "replace_in_file",
                "description": "Replace a unique string occurrence in a file with new content. Fails if the old content does not occur exactly once.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to repository root."
                        },
                        "old": {
                            "type": "string",
                            "description": "Exact text to find and replace (must appear exactly once)."
                        },
                        "new": {
                            "type": "string",
                            "description": "New replacement text."
                        }
                    },
                    "required": ["path", "old", "new"]
                }
            }
        }));
    }

    if permissions >= Permissions::WriteShell {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the repository root. Output is combined stdout and stderr, capped to 8 KB.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to run with `sh -c`."
                        }
                    },
                    "required": ["command"]
                }
            }
        }));
    }

    tools
}

/// Resolves `user_path` safely within `workdir`.
///
/// Ensures the path:
/// - Does not touch `.git`
/// - Does not escape `workdir` via `..`
/// - Is not an absolute path outside `workdir`
/// - Does not escape `workdir` through symlinks
pub fn resolve_path(workdir: &Path, user_path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(user_path);

    for comp in raw.components() {
        if comp.as_os_str() == ".git" {
            return Err("access to .git is forbidden".to_owned());
        }
    }

    let canonical_workdir = workdir
        .canonicalize()
        .map_err(|e| format!("failed to resolve workdir: {e}"))?;

    let rel_candidate = if raw.is_absolute() {
        if let Ok(stripped) = raw.strip_prefix(workdir) {
            stripped
        } else if let Ok(stripped) = raw.strip_prefix(&canonical_workdir) {
            stripped
        } else {
            return Err("path is outside workdir".to_owned());
        }
    } else {
        raw
    };

    let mut stack = Vec::new();
    for comp in rel_candidate.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err("path escapes workdir".to_owned());
                }
            }
            Component::Normal(c) => {
                if c == ".git" {
                    return Err("access to .git is forbidden".to_owned());
                }
                stack.push(c);
            }
        }
    }

    let mut target = canonical_workdir.clone();
    for comp in stack {
        target.push(comp);
    }

    // Check existing target or existing ancestors for symlink escapes.
    let mut check_node = target.as_path();
    while fs::symlink_metadata(check_node).is_err() && check_node != canonical_workdir {
        if let Some(parent) = check_node.parent() {
            check_node = parent;
        } else {
            break;
        }
    }

    if fs::symlink_metadata(check_node).is_ok() {
        match check_node.canonicalize() {
            Ok(canonical_node) => {
                if !canonical_node.starts_with(&canonical_workdir) {
                    return Err("path escapes workdir via symlink".to_owned());
                }
                for comp in canonical_node.components() {
                    if comp.as_os_str() == ".git" {
                        return Err("access to .git is forbidden".to_owned());
                    }
                }
            }
            Err(_) => {
                // If canonicalize failed, check if it's a symlink that points outside
                if let Ok(link_target) = fs::read_link(check_node) {
                    let resolved = if link_target.is_absolute() {
                        link_target
                    } else if let Some(parent) = check_node.parent() {
                        parent.join(link_target)
                    } else {
                        link_target
                    };
                    if !resolved.starts_with(&canonical_workdir) {
                        return Err("path escapes workdir via symlink".to_owned());
                    }
                }
            }
        }
    }

    Ok(target)
}

#[derive(Deserialize)]
struct ListDirArgs {
    #[serde(default = "default_dot")]
    path: String,
}

fn default_dot() -> String {
    ".".to_owned()
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct ReplaceInFileArgs {
    path: String,
    old: String,
    new: String,
}

#[derive(Deserialize)]
struct RunCommandArgs {
    command: String,
}

fn list_dir(workdir: &Path, raw_path: &str) -> String {
    let target = match resolve_path(workdir, raw_path) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if !target.is_dir() {
        return format!("error: '{raw_path}' is not a directory");
    }
    let entries = match fs::read_dir(&target) {
        Ok(entries) => entries,
        Err(e) => return format!("error: failed to read directory: {e}"),
    };
    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            names.push(format!("{file_name}/"));
        } else {
            names.push(file_name);
        }
    }
    names.sort();
    if names.is_empty() {
        "(empty directory)".to_owned()
    } else {
        names.join("\n")
    }
}

fn read_file(
    workdir: &Path,
    raw_path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> String {
    let target = match resolve_path(workdir, raw_path) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if !target.is_file() {
        return format!("error: '{raw_path}' is not a file");
    }
    let content = match fs::read_to_string(&target) {
        Ok(c) => c,
        Err(e) => return format!("error: failed to read file: {e}"),
    };

    let start_line = offset.unwrap_or(1);
    if start_line == 0 {
        return "error: offset must be >= 1".to_owned();
    }
    let max_lines = limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return String::new();
    }
    if start_line > total_lines {
        return format!("error: offset {start_line} exceeds file length ({total_lines} lines)");
    }

    let zero_idx = start_line.saturating_sub(1);
    let mut output = String::new();
    let mut bytes_written = 0usize;
    let mut lines_written = 0usize;
    let mut truncated = false;
    let mut next_offset = start_line;

    for (i, line) in lines.iter().enumerate().skip(zero_idx) {
        let current_line_num = i.saturating_add(1);
        if lines_written >= max_lines {
            truncated = true;
            next_offset = current_line_num;
            break;
        }
        let formatted = format!("{current_line_num}\t{line}\n");
        let line_bytes = formatted.len();
        if bytes_written.saturating_add(line_bytes) > MAX_READ_BYTES {
            if output.is_empty() && line_bytes > MAX_READ_BYTES {
                let cutoff = formatted.floor_char_boundary(MAX_READ_BYTES.saturating_sub(200));
                if let Some(chunk) = formatted.get(..cutoff) {
                    output.push_str(chunk);
                }
            }
            truncated = true;
            next_offset = current_line_num;
            break;
        }
        output.push_str(&formatted);
        bytes_written = bytes_written.saturating_add(line_bytes);
        lines_written = lines_written.saturating_add(1);
    }

    if truncated {
        output.push_str(&format!(
            "[truncated: use offset={next_offset} to read further]"
        ));
    }

    output
}

fn write_file(workdir: &Path, permissions: Permissions, raw_path: &str, content: &str) -> String {
    if permissions < Permissions::WriteWorkdir {
        return "error: permission denied: write_file requires at least write-workdir permission"
            .to_owned();
    }
    let target = match resolve_path(workdir, raw_path) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if let Some(parent) = target.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return format!("error: failed to create directories: {e}");
    }
    match fs::write(&target, content) {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("error: failed to write file: {e}"),
    }
}

fn replace_in_file(
    workdir: &Path,
    permissions: Permissions,
    raw_path: &str,
    old: &str,
    new: &str,
) -> String {
    if permissions < Permissions::WriteWorkdir {
        return "error: permission denied: replace_in_file requires at least write-workdir permission"
            .to_owned();
    }
    if old.is_empty() {
        return "error: 'old' pattern cannot be empty".to_owned();
    }
    let target = match resolve_path(workdir, raw_path) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    let content = match fs::read_to_string(&target) {
        Ok(c) => c,
        Err(e) => return format!("error: failed to read file: {e}"),
    };
    let count = content.match_indices(old).count();
    if count == 0 {
        return format!("error: 'old' text not found in '{raw_path}'");
    }
    if count > 1 {
        return format!(
            "error: 'old' text occurred {count} times in '{raw_path}'; expected exactly once"
        );
    }
    let replaced = content.replacen(old, new, 1);
    match fs::write(&target, replaced) {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("error: failed to write file: {e}"),
    }
}

async fn run_command(
    workdir: &Path,
    permissions: Permissions,
    command: &str,
    timeout_duration: Duration,
) -> String {
    if permissions < Permissions::WriteShell {
        return "error: permission denied: run_command requires write-shell permission".to_owned();
    }
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workdir)
        .env_clear()
        .env("HOME", workdir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("error: failed to spawn command: {e}"),
    };

    let output = match tokio::time::timeout(timeout_duration, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return format!("error: failed to read command output: {e}"),
        Err(_) => {
            return format!(
                "error: command timed out after {} seconds",
                timeout_duration.as_secs()
            );
        }
    };

    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&combined);

    if text.len() > MAX_COMMAND_BYTES {
        let cutoff = text.floor_char_boundary(MAX_COMMAND_BYTES);
        let truncated_slice = text.get(..cutoff).unwrap_or("");
        format!("{truncated_slice}\n[truncated: output exceeded 8 KB]")
    } else {
        text.into_owned()
    }
}

pub async fn execute_tool(
    workdir: &Path,
    permissions: Permissions,
    name: &str,
    args_json: &str,
) -> String {
    execute_tool_with_timeout(workdir, permissions, name, args_json, COMMAND_TIMEOUT).await
}

pub async fn execute_tool_with_timeout(
    workdir: &Path,
    permissions: Permissions,
    name: &str,
    args_json: &str,
    command_timeout: Duration,
) -> String {
    match name {
        "list_dir" => {
            let args: ListDirArgs = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return format!("error: invalid arguments for list_dir: {e}"),
            };
            tracing::debug!(tool = %name, path = %args.path, "calling tool");
            list_dir(workdir, &args.path)
        }
        "read_file" => {
            let args: ReadFileArgs = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return format!("error: invalid arguments for read_file: {e}"),
            };
            tracing::debug!(tool = %name, path = %args.path, "calling tool");
            read_file(workdir, &args.path, args.offset, args.limit)
        }
        "write_file" => {
            let args: WriteFileArgs = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return format!("error: invalid arguments for write_file: {e}"),
            };
            tracing::debug!(tool = %name, path = %args.path, "calling tool");
            write_file(workdir, permissions, &args.path, &args.content)
        }
        "replace_in_file" => {
            let args: ReplaceInFileArgs = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return format!("error: invalid arguments for replace_in_file: {e}"),
            };
            tracing::debug!(tool = %name, path = %args.path, "calling tool");
            replace_in_file(workdir, permissions, &args.path, &args.old, &args.new)
        }
        "run_command" => {
            let args: RunCommandArgs = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return format!("error: invalid arguments for run_command: {e}"),
            };
            tracing::debug!(tool = %name, "calling tool");
            run_command(workdir, permissions, &args.command, command_timeout).await
        }
        other => format!("error: unknown tool '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn path_escape_parent_dir_rejected() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let err1 = resolve_path(workdir, "../outside").unwrap_err();
        assert!(err1.contains("escapes workdir"), "{err1}");

        let err2 = resolve_path(workdir, "sub/../../outside").unwrap_err();
        assert!(err2.contains("escapes workdir"), "{err2}");
    }

    #[test]
    fn path_escape_absolute_rejected() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let err = resolve_path(workdir, "/etc/passwd").unwrap_err();
        assert!(err.contains("outside workdir"), "{err}");
    }

    #[test]
    fn symlink_escape_rejected() {
        let outside_dir = tempdir().unwrap();
        let inside_dir = tempdir().unwrap();
        let workdir = inside_dir.path();

        fs::write(outside_dir.path().join("secret.txt"), "secret").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside_dir.path(), workdir.join("link_to_outside"))
                .unwrap();
            let err = resolve_path(workdir, "link_to_outside/secret.txt").unwrap_err();
            assert!(err.contains("symlink") || err.contains("escapes"), "{err}");

            let err2 = resolve_path(workdir, "link_to_outside").unwrap_err();
            assert!(
                err2.contains("symlink") || err2.contains("escapes"),
                "{err2}"
            );
        }
    }

    #[test]
    fn git_component_rejected() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let err1 = resolve_path(workdir, ".git").unwrap_err();
        assert!(err1.contains(".git"), "{err1}");

        let err2 = resolve_path(workdir, ".git/config").unwrap_err();
        assert!(err2.contains(".git"), "{err2}");

        let err3 = resolve_path(workdir, "foo/.git/bar").unwrap_err();
        assert!(err3.contains(".git"), "{err3}");
    }

    #[tokio::test]
    async fn read_file_truncates_at_400_lines() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let mut lines = String::new();
        for i in 1..=450 {
            lines.push_str(&format!("line {i}\n"));
        }
        fs::write(workdir.join("big.txt"), lines).unwrap();

        let out = execute_tool(
            workdir,
            Permissions::Read,
            "read_file",
            r#"{"path": "big.txt"}"#,
        )
        .await;

        assert!(out.contains("1\tline 1\n"), "{out}");
        assert!(out.contains("400\tline 400\n"), "{out}");
        assert!(!out.contains("401\tline 401\n"), "{out}");
        assert!(out.contains("truncated"), "{out}");
    }

    #[tokio::test]
    async fn read_file_truncates_at_20kb() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        // 30 lines of 1 KB each = 30 KB > 20 KB
        let mut content = String::new();
        for i in 1..=30 {
            content.push_str(&format!("{i}: {}\n", "a".repeat(1024)));
        }
        fs::write(workdir.join("long_lines.txt"), content).unwrap();

        let out = execute_tool(
            workdir,
            Permissions::Read,
            "read_file",
            r#"{"path": "long_lines.txt"}"#,
        )
        .await;

        assert!(out.contains("truncated"), "{out}");
        assert!(out.len() <= MAX_READ_BYTES + 500, "len is {}", out.len());
    }

    #[tokio::test]
    async fn replace_in_file_requires_uniqueness() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        fs::write(workdir.join("test.txt"), "hello world\nhello universe\n").unwrap();

        let not_found = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "replace_in_file",
            r#"{"path": "test.txt", "old": "goodbye", "new": "hi"}"#,
        )
        .await;
        assert!(not_found.starts_with("error:"), "{not_found}");
        assert!(not_found.contains("not found"), "{not_found}");

        let multiple = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "replace_in_file",
            r#"{"path": "test.txt", "old": "hello", "new": "hi"}"#,
        )
        .await;
        assert!(multiple.starts_with("error:"), "{multiple}");
        assert!(multiple.contains("2 times"), "{multiple}");

        let unique = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "replace_in_file",
            r#"{"path": "test.txt", "old": "hello world", "new": "greetings earth"}"#,
        )
        .await;
        assert_eq!(unique, "ok");
        let content = fs::read_to_string(workdir.join("test.txt")).unwrap();
        assert_eq!(content, "greetings earth\nhello universe\n");
    }

    #[tokio::test]
    async fn write_file_creates_parents_and_overwrites() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let res = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "write_file",
            r#"{"path": "nested/dir/file.txt", "content": "first"}"#,
        )
        .await;
        assert_eq!(res, "ok");
        assert_eq!(
            fs::read_to_string(workdir.join("nested/dir/file.txt")).unwrap(),
            "first"
        );

        let res2 = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "write_file",
            r#"{"path": "nested/dir/file.txt", "content": "second"}"#,
        )
        .await;
        assert_eq!(res2, "ok");
        assert_eq!(
            fs::read_to_string(workdir.join("nested/dir/file.txt")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn run_command_timeout() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let res = execute_tool_with_timeout(
            workdir,
            Permissions::WriteShell,
            "run_command",
            r#"{"command": "sleep 1"}"#,
            Duration::from_millis(50),
        )
        .await;
        assert!(res.starts_with("error:"), "{res}");
        assert!(res.contains("timed out"), "{res}");
    }

    #[tokio::test]
    async fn run_command_truncation() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let res = execute_tool(
            workdir,
            Permissions::WriteShell,
            "run_command",
            r#"{"command": "head -c 10000 /dev/zero | tr '\\0' 'x'"}"#,
        )
        .await;
        assert!(res.contains("truncated"), "{res}");
    }

    #[tokio::test]
    async fn permissions_restrict_offered_and_executed_tools() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        // Read permissions: only list_dir and read_file offered
        let read_tools = tool_definitions(Permissions::Read);
        let names: Vec<&str> = read_tools
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["list_dir", "read_file"]);

        // Calling write_file under Read permission is rejected
        let res = execute_tool(
            workdir,
            Permissions::Read,
            "write_file",
            r#"{"path": "foo.txt", "content": "data"}"#,
        )
        .await;
        assert!(res.starts_with("error:"), "{res}");
        assert!(res.contains("permission denied"), "{res}");

        // WriteWorkdir permissions: list_dir, read_file, write_file, replace_in_file
        let workdir_tools = tool_definitions(Permissions::WriteWorkdir);
        let names: Vec<&str> = workdir_tools
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            ["list_dir", "read_file", "write_file", "replace_in_file"]
        );

        // Calling run_command under WriteWorkdir permission is rejected
        let res2 = execute_tool(
            workdir,
            Permissions::WriteWorkdir,
            "run_command",
            r#"{"command": "ls"}"#,
        )
        .await;
        assert!(res2.starts_with("error:"), "{res2}");
        assert!(res2.contains("permission denied"), "{res2}");

        // WriteShell permissions: all 5 tools offered
        let shell_tools = tool_definitions(Permissions::WriteShell);
        let names: Vec<&str> = shell_tools
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            [
                "list_dir",
                "read_file",
                "write_file",
                "replace_in_file",
                "run_command"
            ]
        );
    }

    #[tokio::test]
    async fn malformed_json_and_unknown_tool_return_errors() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        let bad_json = execute_tool(workdir, Permissions::Read, "read_file", "{bad json").await;
        assert!(bad_json.starts_with("error:"), "{bad_json}");
        assert!(bad_json.contains("invalid arguments"), "{bad_json}");

        let unknown = execute_tool(workdir, Permissions::Read, "magic_tool", "{}").await;
        assert!(unknown.starts_with("error:"), "{unknown}");
        assert!(unknown.contains("unknown tool"), "{unknown}");
    }

    #[tokio::test]
    async fn list_dir_lists_entries_sorted_and_ignores_git() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();

        fs::create_dir(workdir.join(".git")).unwrap();
        fs::write(workdir.join(".git/config"), "secret").unwrap();
        fs::create_dir(workdir.join("src")).unwrap();
        fs::write(workdir.join("src/lib.rs"), "code").unwrap();
        fs::write(workdir.join("Cargo.toml"), "cargo").unwrap();

        let out = execute_tool(workdir, Permissions::Read, "list_dir", "{}").await;
        assert_eq!(out, "Cargo.toml\nsrc/");
    }
}
