use tauri::Emitter;

use crate::util::{background_command, expand_path};

// ── diff 解析 ─────────────────────────────────────────────────────

/// 解析 diff 文本中的 hunk 列表
pub fn parse_diff_hunks(diff: &str) -> Vec<serde_json::Value> {
    let mut hunks = vec![];
    let mut current_hunk: Option<(String, Vec<serde_json::Value>)> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some((header, lines)) = current_hunk.take() {
                hunks.push(serde_json::json!({ "header": header, "lines": lines }));
            }
            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() >= 3 {
                let old_part = parts[1].trim_start_matches('-');
                let new_part = parts[2].trim_start_matches('+');
                old_line = old_part
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                new_line = new_part
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
            }
            current_hunk = Some((line.to_string(), vec![]));
        } else if let Some((_, ref mut lines)) = current_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                lines.push(serde_json::json!({
                    "type": "added",
                    "content": &line[1..],
                    "newLineNo": new_line,
                }));
                new_line += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                lines.push(serde_json::json!({
                    "type": "deleted",
                    "content": &line[1..],
                    "oldLineNo": old_line,
                }));
                old_line += 1;
            } else if !line.starts_with('\\') {
                lines.push(serde_json::json!({
                    "type": "context",
                    "content": if line.is_empty() { "" } else { &line[1..] },
                    "oldLineNo": old_line,
                    "newLineNo": new_line,
                }));
                old_line += 1;
                new_line += 1;
            }
        }
    }
    if let Some((header, lines)) = current_hunk {
        hunks.push(serde_json::json!({ "header": header, "lines": lines }));
    }
    hunks
}

/// 从无 hunk 的 diff 输出中提取说明性文本（权限变更、submodule、空文件等）
pub fn parse_diff_note(diff: &str) -> Option<String> {
    let mut old_mode: Option<&str> = None;
    let mut new_mode: Option<&str> = None;
    let mut submodule_lines: Vec<&str> = vec![];

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("old mode ") {
            old_mode = Some(rest);
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            new_mode = Some(rest);
        } else if line.starts_with("Subproject commit") || line.starts_with("-Subproject commit") {
            submodule_lines.push(line);
        }
    }

    if let (Some(old), Some(new)) = (old_mode, new_mode) {
        return Some(format!("文件权限变更：{old} → {new}"));
    }
    if !submodule_lines.is_empty() {
        return Some("Submodule 提交变更".to_string());
    }
    if diff.contains("diff --git") {
        return Some("仅元数据变更（无内容差异）".to_string());
    }
    None
}

/// 获取单个文件相对于 HEAD 的 hunks
pub fn get_file_hunks(workdir: &str, path: &str) -> (Vec<serde_json::Value>, Option<String>) {
    let output = background_command("git")
        .current_dir(workdir)
        .args(["diff", "HEAD", "--", path])
        .output();
    let Ok(out) = output else {
        return (vec![], None);
    };
    let diff_text = String::from_utf8_lossy(&out.stdout);
    let hunks = parse_diff_hunks(&diff_text);
    if !hunks.is_empty() {
        return (hunks, None);
    }
    (vec![], parse_diff_note(&diff_text))
}

/// 获取指定 range（如 `base...session`）中单个文件的 hunks
pub fn get_file_hunks_between(
    workdir: &str,
    range: &str,
    path: &str,
) -> (Vec<serde_json::Value>, Option<String>) {
    let output = background_command("git")
        .current_dir(workdir)
        .args(["diff", range, "--", path])
        .output();
    let Ok(out) = output else {
        return (vec![], None);
    };
    let diff_text = String::from_utf8_lossy(&out.stdout);
    let hunks = parse_diff_hunks(&diff_text);
    if !hunks.is_empty() {
        return (hunks, None);
    }
    (vec![], parse_diff_note(&diff_text))
}

// ── 文件列表解析 ──────────────────────────────────────────────────

/// 根据 additions/deletions 判断文件变更类型
fn file_type(additions: u32, deletions: u32, is_binary: bool) -> &'static str {
    if is_binary {
        "modified"
    } else if additions > 0 && deletions == 0 {
        "added"
    } else if additions == 0 && deletions > 0 {
        "deleted"
    } else {
        "modified"
    }
}

/// 解析 `git diff --numstat` 输出为结构化文件列表
fn parse_numstat(
    stdout: &str,
    _workdir: &str,
    get_hunks: impl Fn(&str) -> (Vec<serde_json::Value>, Option<String>),
) -> Vec<serde_json::Value> {
    let mut files = vec![];
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let path = parts[2].to_string();
        let is_binary = parts[0] == "-" && parts[1] == "-";
        let additions = if is_binary {
            0
        } else {
            parts[0].parse::<u32>().unwrap_or(0)
        };
        let deletions = if is_binary {
            0
        } else {
            parts[1].parse::<u32>().unwrap_or(0)
        };

        let (hunks, note) = if is_binary {
            (vec![], None)
        } else {
            get_hunks(&path)
        };

        let mut entry = serde_json::json!({
            "path": path,
            "type": file_type(additions, deletions, is_binary),
            "additions": additions,
            "deletions": deletions,
            "binary": is_binary,
            "hunks": hunks,
        });
        if let Some(n) = note {
            entry["note"] = serde_json::Value::String(n);
        }
        files.push(entry);
    }
    files
}

// ── 公开函数 ──────────────────────────────────────────────────────

/// 获取工作目录相对于 HEAD 的 diff（结构化文件列表）
pub fn get_git_diff_raw(workdir: &str) -> Result<Vec<serde_json::Value>, String> {
    let output = background_command("git")
        .current_dir(workdir)
        .args(["diff", "--numstat", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 不再附带 hunks：列表只需要摘要，内容由 get_file_diff_hunks 按需拉取。
    // 之前每次刷新都要跑一次完整 diff 并把每一行序列化过来，
    // 大改动下是几 MB 的 IPC 负载，而界面默认只显示文件名。
    Ok(parse_numstat(&stdout, workdir, |_| (vec![], None)))
}

/// 计算 base_branch...session_branch 之间的变更文件
pub fn get_git_diff_between(
    workdir: &str,
    base: &str,
    session: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let range = format!("{base}...{session}");
    let numstat = background_command("git")
        .current_dir(workdir)
        .args(["diff", "--numstat", &range])
        .output()
        .map_err(|e| e.to_string())?;

    if !numstat.status.success() {
        return Err(String::from_utf8_lossy(&numstat.stderr).trim().to_string());
    }

    let stdout = String::from_utf8_lossy(&numstat.stdout);
    // 同上：只回传文件摘要，hunks 由 get_file_diff_hunks 按需拉取
    Ok(parse_numstat(&stdout, workdir, |_| (vec![], None)))
}

pub fn get_git_diff_from_base_worktree(
    workdir: &str,
    base: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let merge_base = background_command("git")
        .current_dir(workdir)
        .args(["merge-base", "HEAD", base])
        .output()
        .map_err(|e| e.to_string())?;

    if !merge_base.status.success() {
        return Err(String::from_utf8_lossy(&merge_base.stderr).trim().to_string());
    }

    let merge_base_sha = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();
    if merge_base_sha.is_empty() {
        return Err("无法解析 merge-base".to_string());
    }

    let numstat = background_command("git")
        .current_dir(workdir)
        .args(["diff", "--numstat", &merge_base_sha])
        .output()
        .map_err(|e| e.to_string())?;

    if !numstat.status.success() {
        return Err(String::from_utf8_lossy(&numstat.stderr).trim().to_string());
    }

    let stdout = String::from_utf8_lossy(&numstat.stdout);
    // 同上：只回传文件摘要，hunks 由 get_file_diff_hunks 按需拉取
    Ok(parse_numstat(&stdout, workdir, |_| (vec![], None)))
}

/// 按需获取单个文件的 hunks。
///
/// 列表刷新只回传文件摘要；某个文件被展开时才拉取它的 hunks。
/// 之前每次刷新都把所有文件的所有 hunk 一起序列化，大改动下是几 MB 的
/// IPC 负载，而界面默认只显示文件名。
#[tauri::command]
pub async fn get_file_diff_hunks(
    workdir: String,
    path: String,
    base_branch: Option<String>,
) -> Result<serde_json::Value, String> {
    let expanded = expand_path(&workdir);
    tokio::task::spawn_blocking(move || {
        let (hunks, note) = match base_branch.as_deref().map(str::trim) {
            Some(base) if !base.is_empty() => {
                let merge_base = background_command("git")
                    .current_dir(&expanded)
                    .args(["merge-base", "HEAD", base])
                    .output()
                    .map_err(|e| e.to_string())?;
                let sha = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();
                if merge_base.status.success() && !sha.is_empty() {
                    get_file_hunks_between(&expanded, &sha, &path)
                } else {
                    get_file_hunks(&expanded, &path)
                }
            }
            _ => get_file_hunks(&expanded, &path),
        };
        Ok::<serde_json::Value, String>(serde_json::json!({
            "hunks": hunks,
            "note": note,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Tauri Commands ────────────────────────────────────────────────

#[tauri::command]
pub async fn get_git_diff(
    app: tauri::AppHandle,
    session_id: String,
    workdir: String,
) -> Result<(), String> {
    let expanded = expand_path(&workdir);
    let files = tokio::task::spawn_blocking(move || get_git_diff_raw(&expanded))
        .await
        .map_err(|e| e.to_string())??;
    let _ = app.emit(
        "diff-update",
        serde_json::json!({"session_id": session_id, "files": files}),
    );
    Ok(())
}

#[tauri::command]
pub async fn get_git_diff_branch(
    app: tauri::AppHandle,
    session_id: String,
    workdir: String,
    base_branch: String,
    session_branch: String,
) -> Result<(), String> {
    let expanded = expand_path(&workdir);
    let files = tokio::task::spawn_blocking(move || {
        get_git_diff_between(&expanded, &base_branch, &session_branch)
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = app.emit(
        "diff-update",
        serde_json::json!({"session_id": session_id, "files": files}),
    );
    Ok(())
}

#[tauri::command]
pub async fn get_git_diff_session_worktree(
    app: tauri::AppHandle,
    session_id: String,
    workdir: String,
    base_branch: String,
) -> Result<(), String> {
    let expanded = expand_path(&workdir);
    let files = tokio::task::spawn_blocking(move || {
        get_git_diff_from_base_worktree(&expanded, &base_branch)
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = app.emit(
        "diff-update",
        serde_json::json!({"session_id": session_id, "files": files}),
    );
    Ok(())
}
