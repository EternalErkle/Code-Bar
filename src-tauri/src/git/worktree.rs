use std::{fs, path::Path, time::{SystemTime, UNIX_EPOCH}};

use crate::runtime_scope::session_worktree_root_dir;
use crate::util::{background_command, expand_path, normalize_expanded_path};

// ── 辅助函数 ──────────────────────────────────────────────────────
pub fn session_branch_prefix() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let token = format!("{:06x}", millis % 0x1000000);
    format!("ci/{token}")
}

pub fn session_branch_name(prefix: &str, session_id: &str) -> String {
    format!("{prefix}/session-{session_id}")
}

/// Windows 保留设备名，不能直接作为目录名
const RESERVED_DIR_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 把用户输入的名称转成文件系统与 git ref 都安全的 slug
pub fn worktree_slug(raw: &str) -> Option<String> {
    let mut slug = String::new();
    let mut pending_dash = false;

    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.extend(ch.to_lowercase());
        } else {
            pending_dash = true;
        }
    }

    if slug.chars().count() > 48 {
        slug = slug.chars().take(48).collect();
    }

    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        return None;
    }
    if RESERVED_DIR_NAMES.contains(&slug.as_str()) {
        return Some(format!("{slug}-wt"));
    }
    Some(slug)
}

/// 选一个尚未被占用的 worktree 目录，重名时追加序号
fn unique_worktree_path(base_dir: &str, slug: &str) -> String {
    let mut candidate = format!("{base_dir}/{slug}");
    let mut suffix = 2_u32;
    while Path::new(&candidate).exists() {
        candidate = format!("{base_dir}/{slug}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// 从 worktree 目录的 HEAD 文件读取分支名
pub fn read_worktree_branch(worktree_path: &Path) -> Option<String> {
    // worktree 中的 HEAD 格式：ref: refs/heads/<branch>
    let content = fs::read_to_string(worktree_path.join("HEAD")).ok()?;
    let branch = content.trim().strip_prefix("ref: refs/heads/")?;
    Some(branch.to_string())
}

/// 强制移除 worktree（先尝试 git worktree remove，失败则手动删目录 + prune）
fn force_remove_worktree(workdir: &str, wt_path: &str) {
    let _ = background_command("git")
        .current_dir(workdir)
        .args(["worktree", "remove", "--force", wt_path])
        .output();

    let p = Path::new(wt_path);
    if p.exists() {
        let _ = fs::remove_dir_all(p);
        let _ = background_command("git")
            .current_dir(workdir)
            .args(["worktree", "prune"])
            .output();
    }
}

// ── Tauri Commands ────────────────────────────────────────────────

/// 创建 git worktree（基于当前 HEAD 创建新分支并 checkout 到指定路径）
#[tauri::command]
pub async fn git_worktree_create(
    workdir: String,
    branch: String,
    worktree_path: String,
) -> Result<String, String> {
    let expanded_workdir = expand_path(&workdir);
    let expanded_wt_path = expand_path(&worktree_path);

    tokio::task::spawn_blocking(move || {
        if let Some(parent) = Path::new(&expanded_wt_path).parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }

        let out = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["worktree", "add", "-b", &branch, &expanded_wt_path, "HEAD"])
            .output()
            .map_err(|e| e.to_string())?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(expanded_wt_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 删除 git worktree（可选同时删除对应分支）
#[tauri::command]
pub async fn git_worktree_remove(
    workdir: String,
    worktree_path: String,
    branch: String,
    delete_branch: bool,
) -> Result<(), String> {
    let expanded_workdir = expand_path(&workdir);
    let expanded_wt_path = expand_path(&worktree_path);

    tokio::task::spawn_blocking(move || {
        force_remove_worktree(&expanded_workdir, &expanded_wt_path);

        if delete_branch && !branch.is_empty() {
            let _ = background_command("git")
                .current_dir(&expanded_workdir)
                .args(["branch", "-D", &branch])
                .output();
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 列出所有 git worktree（返回结构化信息）
#[tauri::command]
pub async fn git_worktree_list(workdir: String) -> Result<Vec<serde_json::Value>, String> {
    let expanded = expand_path(&workdir);

    tokio::task::spawn_blocking(move || {
        let out = background_command("git")
            .current_dir(&expanded)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .map_err(|e| e.to_string())?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut worktrees = vec![];
        let mut current: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        for line in stdout.lines() {
            if line.is_empty() {
                if !current.is_empty() {
                    worktrees.push(serde_json::Value::Object(current.clone()));
                    current.clear();
                }
            } else if let Some(path) = line.strip_prefix("worktree ") {
                current.insert("path".into(), path.into());
            } else if let Some(hash) = line.strip_prefix("HEAD ") {
                current.insert("head".into(), hash.into());
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                current.insert("branch".into(), branch.into());
            } else if line == "bare" {
                current.insert("bare".into(), true.into());
            } else if line == "detached" {
                current.insert("detached".into(), true.into());
            }
        }
        if !current.is_empty() {
            worktrees.push(serde_json::Value::Object(current));
        }

        Ok(worktrees)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 将 worktree 分支 merge 回目标分支，然后删除 worktree 和分支
#[tauri::command]
pub async fn git_worktree_merge(
    workdir: String,
    worktree_path: String,
    branch: String,
    target_branch: String,
) -> Result<(), String> {
    let expanded_workdir = expand_path(&workdir);
    let expanded_wt_path = expand_path(&worktree_path);

    tokio::task::spawn_blocking(move || {
        // 切换到目标分支
        let switch = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["checkout", &target_branch])
            .output()
            .map_err(|e| e.to_string())?;
        if !switch.status.success() {
            return Err(format!(
                "切换到 {} 失败: {}",
                target_branch,
                String::from_utf8_lossy(&switch.stderr).trim()
            ));
        }

        // merge --no-ff
        let merge = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["merge", "--no-ff", &branch])
            .output()
            .map_err(|e| e.to_string())?;
        if !merge.status.success() {
            return Err(format!(
                "merge 失败: {}",
                String::from_utf8_lossy(&merge.stderr).trim()
            ));
        }

        // 删除 worktree 和分支
        force_remove_worktree(&expanded_workdir, &expanded_wt_path);
        if !branch.is_empty() {
            let _ = background_command("git")
                .current_dir(&expanded_workdir)
                .args(["branch", "-D", &branch])
                .output();
        }

        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 为 session 自动创建独立 git worktree
/// 返回 { worktree_path, branch, base_branch }，非 git 仓库则返回 None
#[tauri::command]
pub async fn setup_session_worktree(
    workdir: String,
    session_id: String,
    name: Option<String>,
) -> Result<Option<serde_json::Value>, String> {
    let expanded_workdir = expand_path(&workdir);
    let session_id_clone = session_id.clone();
    let requested_name = name.unwrap_or_default();

    tokio::task::spawn_blocking(move || {
        // 检测是否是 git 仓库
        let branch_out = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .map_err(|e| e.to_string())?;

        if !branch_out.status.success() {
            return Ok(None);
        }

        let base_branch = String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .to_string();
        if base_branch == "HEAD" {
            return Ok(None); // detached HEAD，跳过
        }

        // 计算 worktree 路径（放在 repo 同级的 session worktree 根目录）
        let repo_parent = Path::new(&expanded_workdir)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| expanded_workdir.clone());
        // 目录名来自用户输入的名称；为空或全是非法字符时回落到 session-{id}
        let slug = worktree_slug(&requested_name)
            .unwrap_or_else(|| format!("session-{session_id_clone}"));
        let wt_base = format!("{}/{}", repo_parent, session_worktree_root_dir());
        // 重名时追加序号，而不是强删已存在的目录——它可能属于别的 session
        let worktree_path = unique_worktree_path(&wt_base, &slug);
        let dir_name = Path::new(&worktree_path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(slug.as_str())
            .to_string();
        let branch_prefix = session_branch_prefix();
        let branch = session_branch_name(&branch_prefix, &dir_name);

        // 清理指向已删除目录的悬空注册项；prune 只删登记信息，不碰仍存在的 worktree
        let _ = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["worktree", "prune"])
            .output();

        // 创建 worktree
        if let Some(parent) = Path::new(&worktree_path).parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建 worktree 父目录失败: {e}"))?;
        }

        let out = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["worktree", "add", "-b", &branch, &worktree_path, "HEAD"])
            .output()
            .map_err(|e| e.to_string())?;

        if !out.status.success() {
            return Err(format!(
                "创建 worktree 失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        Ok(Some(serde_json::json!({
            "worktree_path": worktree_path,
            "branch": branch,
            "base_branch": base_branch,
        })))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 清理 session 的 git worktree（静默失败）
#[tauri::command]
pub async fn teardown_session_worktree(
    workdir: String,
    worktree_path: String,
    branch: String,
) -> Result<(), String> {
    let expanded_workdir = expand_path(&workdir);
    let expanded_wt = expand_path(&worktree_path);

    tokio::task::spawn_blocking(move || {
        force_remove_worktree(&expanded_workdir, &expanded_wt);

        // 修剪悬空 worktree 引用
        let _ = background_command("git")
            .current_dir(&expanded_workdir)
            .args(["worktree", "prune"])
            .output();

        if !branch.is_empty() {
            let _ = background_command("git")
                .current_dir(&expanded_workdir)
                .args(["branch", "-D", &branch])
                .output();
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 清理孤儿 worktree：删除不在 known_worktree_paths 中的所有 worktree 目录和分支
#[tauri::command]
pub async fn prune_orphan_worktrees(
    workdir: String,
    known_worktree_paths: Vec<String>,
) -> Result<Vec<String>, String> {
    let expanded_workdir = expand_path(&workdir);

    tokio::task::spawn_blocking(move || {
        let repo_parent = Path::new(&expanded_workdir)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| expanded_workdir.clone());
        let wt_base = format!("{}/{}", repo_parent, session_worktree_root_dir());
        let wt_base_path = Path::new(&wt_base);

        if !wt_base_path.exists() {
            return Ok(vec![]);
        }

        // 规范化已知路径集合
        let known: std::collections::HashSet<String> = known_worktree_paths
            .iter()
            .map(|p| normalize_expanded_path(p))
            .collect();

        let mut pruned = vec![];

        for entry in fs::read_dir(wt_base_path).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let canonical = normalize_expanded_path(path.to_string_lossy().as_ref());
            if known.contains(&canonical) {
                continue;
            }

            // 孤儿 worktree：读取分支名后清理
            let branch = read_worktree_branch(&path);

            let _ = background_command("git")
                .current_dir(&expanded_workdir)
                .args(["worktree", "remove", "--force", &canonical])
                .output();

            if path.exists() {
                let _ = fs::remove_dir_all(&path);
            }

            if let Some(b) = &branch {
                if !b.is_empty() {
                    let _ = background_command("git")
                        .current_dir(&expanded_workdir)
                        .args(["branch", "-D", b])
                        .output();
                }
            }

            pruned.push(canonical);
        }

        if !pruned.is_empty() {
            let _ = background_command("git")
                .current_dir(&expanded_workdir)
                .args(["worktree", "prune"])
                .output();
        }

        Ok(pruned)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalizes_spacing_and_case() {
        assert_eq!(worktree_slug("login fix").as_deref(), Some("login-fix"));
        assert_eq!(worktree_slug("  Login   Fix!!  ").as_deref(), Some("login-fix"));
        assert_eq!(worktree_slug("Fix/Auth#2").as_deref(), Some("fix-auth-2"));
    }

    #[test]
    fn slug_rejects_names_with_no_usable_characters() {
        assert_eq!(worktree_slug(""), None);
        assert_eq!(worktree_slug("   "), None);
        assert_eq!(worktree_slug("!!!///"), None);
    }

    #[test]
    fn slug_avoids_windows_reserved_device_names() {
        assert_eq!(worktree_slug("CON").as_deref(), Some("con-wt"));
        assert_eq!(worktree_slug("nul").as_deref(), Some("nul-wt"));
        // 只有完全等于保留名时才加后缀
        assert_eq!(worktree_slug("console").as_deref(), Some("console"));
    }

    #[test]
    fn slug_truncates_without_leaving_trailing_dash() {
        let slug = worktree_slug(&"a".repeat(60)).expect("slug");
        assert_eq!(slug.chars().count(), 48);

        // 截断点正好落在分隔符上：第 48 个字符是横线，必须被去掉
        let awkward = format!("{} tail", "b".repeat(47));
        let slug = worktree_slug(&awkward).expect("slug");
        assert!(!slug.ends_with('-'), "slug ended with dash: {slug}");
        assert_eq!(slug, "b".repeat(47));
    }

    #[test]
    fn unique_path_appends_suffix_instead_of_reusing_existing_dir() {
        let base = std::env::temp_dir().join(format!(
            "code-bar-wt-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&base).expect("temp base");
        let base_str = base.to_string_lossy().to_string();

        let first = unique_worktree_path(&base_str, "feature");
        assert!(first.ends_with("feature"), "unexpected first path: {first}");

        // 目录被占用后必须换一个新路径，而不是复用
        fs::create_dir_all(&first).expect("first dir");
        let second = unique_worktree_path(&base_str, "feature");
        assert_ne!(first, second);
        assert!(second.ends_with("feature-2"), "unexpected second path: {second}");

        fs::create_dir_all(&second).expect("second dir");
        let third = unique_worktree_path(&base_str, "feature");
        assert!(third.ends_with("feature-3"), "unexpected third path: {third}");

        let _ = fs::remove_dir_all(&base);
    }
}
