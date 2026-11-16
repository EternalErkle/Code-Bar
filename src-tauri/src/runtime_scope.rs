use std::path::{Path, PathBuf};

use crate::util::normalize_expanded_path;

pub const UI_STATE_NAMESPACE_DEV: &str = "dev";
pub const WORKTREE_ROOT_DIR: &str = "code-bar-worktrees";
pub const WORKTREE_ROOT_DIR_DEV: &str = "code-bar-worktrees-dev";

/// Worktrees live on this drive when it exists, keeping large checkouts off the system drive.
#[cfg(windows)]
const PREFERRED_WORKTREE_DRIVE: &str = "D:\\";

pub fn ui_state_namespace_dir() -> Option<&'static str> {
    if cfg!(debug_assertions) {
        Some(UI_STATE_NAMESPACE_DEV)
    } else {
        None
    }
}

fn session_worktree_root_dir() -> &'static str {
    if cfg!(debug_assertions) {
        WORKTREE_ROOT_DIR_DEV
    } else {
        WORKTREE_ROOT_DIR
    }
}

/// Top-level directory shared by all repos' session worktrees.
fn session_worktree_root(repo_root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let drive = Path::new(PREFERRED_WORKTREE_DRIVE);
        if drive.is_dir() {
            return drive.join(session_worktree_root_dir());
        }
    }
    repo_root
        .parent()
        .unwrap_or(repo_root)
        .join(format!(".{}", session_worktree_root_dir()))
}

/// Stable per-repo directory name: readable basename plus a hash of the full path,
/// so two repos with the same folder name never share (or prune) each other's worktrees.
pub fn repo_worktree_key(repo_root: &Path) -> String {
    let normalized = normalize_expanded_path(&repo_root.to_string_lossy());
    // FNV-1a, 32-bit. Deterministic across builds, unlike std's DefaultHasher.
    let mut hash: u32 = 0x811c9dc5;
    for byte in normalized.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let name: String = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let name = name.trim_matches('-');
    let name = if name.is_empty() { "repo" } else { name };
    format!("{name}-{hash:08x}")
}

/// Directory holding the session worktrees of one repo.
pub fn session_worktree_base(repo_root: &Path) -> PathBuf {
    session_worktree_root(repo_root).join(repo_worktree_key(repo_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_is_stable_and_case_insensitive_on_windows() {
        let a = repo_worktree_key(Path::new(r"C:\Users\erelg\Documents\GitHub\UPPA-BOT"));
        assert!(a.starts_with("UPPA-BOT-"), "{a}");
        #[cfg(windows)]
        {
            let b = repo_worktree_key(Path::new("c:/users/erelg/documents/github/UPPA-BOT/"));
            assert_eq!(a, b);
        }
    }

    #[test]
    fn repo_key_separates_same_named_repos() {
        let a = repo_worktree_key(Path::new("/one/app"));
        let b = repo_worktree_key(Path::new("/two/app"));
        assert_ne!(a, b);
        assert!(a.starts_with("app-") && b.starts_with("app-"));
    }

    #[test]
    fn repo_key_sanitizes_spaces() {
        let key = repo_worktree_key(Path::new("/x/Genshin translation thing"));
        assert!(key.starts_with("Genshin-translation-thing-"), "{key}");
    }
}
