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

// ── 批量 diff 解析 ────────────────────────────────────────────────

/// 从单个 `diff --git` 段落中解析出文件路径（优先 b 侧，删除时回退 a 侧）
fn section_path(section: &str) -> Option<String> {
    let mut from_path: Option<String> = None;
    for line in section.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest != "/dev/null" {
                return Some(rest.strip_prefix("b/").unwrap_or(rest).to_string());
            }
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if rest != "/dev/null" && from_path.is_none() {
                from_path = Some(rest.strip_prefix("a/").unwrap_or(rest).to_string());
            }
        } else if line.starts_with("@@") {
            break;
        }
    }
    from_path
}

/// 一次性解析整个仓库的 diff 输出，得到 path → (hunks, note) 映射。
///
/// 取代「每个变更文件 spawn 一次 `git diff -- <path>`」的做法：
/// 大改动下那会在每次刷新时创建上百个进程，在 Windows 上尤其昂贵。
fn parse_multi_file_diff(
    diff_text: &str,
) -> std::collections::HashMap<String, (Vec<serde_json::Value>, Option<String>)> {
    let mut map = std::collections::HashMap::new();
    let mut start: Option<usize> = None;
    let mut offsets: Vec<usize> = vec![];

    for (index, _) in diff_text.match_indices("\ndiff --git ") {
        offsets.push(index + 1);
    }
    if diff_text.starts_with("diff --git ") {
        start = Some(0);
    }

    let mut bounds: Vec<usize> = start.into_iter().collect();
    bounds.extend(offsets);
    bounds.push(diff_text.len());

    for window in bounds.windows(2) {
        let section = &diff_text[window[0]..window[1]];
        let Some(path) = section_path(section) else {
            continue;
        };
        let hunks = parse_diff_hunks(section);
        let note = if hunks.is_empty() {
            parse_diff_note(section)
        } else {
            None
        };
        map.insert(path, (hunks, note));
    }

    map
}

/// 运行一次完整 diff 并解析为 path → (hunks, note) 映射；失败时返回空表（调用方回退到逐文件）
fn collect_diff_map(
    workdir: &str,
    args: &[&str],
) -> std::collections::HashMap<String, (Vec<serde_json::Value>, Option<String>)> {
    let output = background_command("git")
        .current_dir(workdir)
        .args(args)
        .output();
    let Ok(out) = output else {
        return std::collections::HashMap::new();
    };
    if !out.status.success() {
        return std::collections::HashMap::new();
    }
    parse_multi_file_diff(&String::from_utf8_lossy(&out.stdout))
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
    let diff_map = collect_diff_map(workdir, &["diff", "HEAD"]);
    Ok(parse_numstat(&stdout, workdir, |path| {
        match diff_map.get(path) {
            Some(entry) => entry.clone(),
            // 批量解析未覆盖（例如含特殊字符的重命名路径）时才回退到单文件 spawn
            None => get_file_hunks(workdir, path),
        }
    }))
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
    let range_clone = range.clone();
    let diff_map = collect_diff_map(workdir, &["diff", range.as_str()]);
    Ok(parse_numstat(&stdout, workdir, move |path| {
        match diff_map.get(path) {
            Some(entry) => entry.clone(),
            None => get_file_hunks_between(workdir, &range_clone, path),
        }
    }))
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
    let base_clone = merge_base_sha.clone();
    let diff_map = collect_diff_map(workdir, &["diff", merge_base_sha.as_str()]);
    Ok(parse_numstat(&stdout, workdir, move |path| {
        if let Some(entry) = diff_map.get(path) {
            return entry.clone();
        }
        let output = background_command("git")
            .current_dir(workdir)
            .args(["diff", &base_clone, "--", path])
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
    }))
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
