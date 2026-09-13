 Vulnerabilities

  - Unauthenticated hook listener. hooks.rs:1484-1531 binds loopback TCP and accepts any JSON. Session ids are small
    integers, and when code_bar_session_id is set the cwd check is skipped at session_lifecycle.rs:128. Any local
    process can flip session status, fire notifications, or bind a fake provider session id.
  - Forged binding reaches the command line. The bound id persists via save_recovery_binding and launches as --resume
    <id> at SessionDetail.tsx:228. On Windows the .cmd shim fallback at pty.rs:186-200 routes through cmd.exe /c, where
    metacharacters in that arg can break out. Fix: random per-launch token in ~/.codebar/run, forwarded by the bridge
    scripts via env, checked on receipt. Also validate provider ids as [A-Za-z0-9_-] before binding at
    provider_sessions.rs:98.
  - API keys are XOR-obfuscated. keystore.rs:15-19 is reversible by inspection. Nothing writes keys anymore, since
    saveProviderApiKey has no caller. Delete the keystore and the apiKeys plumbing, or use the keyring crate.
  - Dead commands leak secrets to the webview. detect_cli_config at cli_detect.rs:434-621 sources shell rc files and
    returns API keys plus the full Claude settings to JS. debug_env returns PATH and HOME. Neither is invoked from src/.
    Remove both from lib.rs:204-206.
  - CSP is disabled. tauri.conf.json:15. No dangerouslySetInnerHTML exists, but with the two items above any injection
    has free reach. Set default-src 'self' with 'unsafe-inline' only on style-src for xterm and CodeMirror.
  - Permission bypass is hard-coded. SessionDetail.tsx:227-230 passes --dangerously-skip-permissions to every Claude
    session with no UI. Prompt injection in any repo file gets unrestricted execution. Codex gets no equivalent flag, so
    runners behave differently. Make it a per-session toggle with a visible badge.
  - Git positional args are unterminated. worktree.rs:113,143,229,244 and all of branch.rs pass branch and path values
    without --. A value starting with a dash becomes a flag. Branch code is dead anyway (see below); add -- to the live
    worktree calls.
  - Session id is unsanitized in one path. session_files.rs:55 interpolates the id into a filename while
    ui_state.rs:285-294 sanitizes. Share the sanitizer.
  - Leaf symlinks escape the session root. session_files.rs:98-104 canonicalizes only the parent. A symlinked file or
    dir at the leaf passes and is read. Canonicalize the leaf when it exists.
  - Error overlay ships to production. App.tsx:1085-1151 shows raw stacks to users. The console.error override at
    App.tsx:128 calls JSON.stringify on arbitrary objects and throws on cycles. Gate on import.meta.env.DEV and wrap the
    stringify.

  Unused command surface (no reference anywhere in src/): all of runner.rs and ProcessMap; cli_detect::{debug_env,
  detect_cli_config, find_in_path}; window::{resize_popup, load_popup_bounds}; every command in git/branch.rs;
  git_worktree_{create,remove,list,merge} and prune_orphan_worktrees; get_git_diff_branch; tauri-plugin-opener in Cargo,
  package.json, and capabilities. Frontend side: the four legacy listeners at App.tsx:830-863,
  appendOutput/clearOutput/session.output in sessionStore.ts, saveProviderApiKey, the overlay branch of SessionDetail
  and with it resize_popup_full, restore_popup_bounds, PreExpandPos, and RestoringLock. The popup-shown emit at
  window.rs:279 has no listener.

  Performance

  - Hidden widgets spawn shells and GPU contexts. SplitSwapLayout.tsx:366-373 portals every widget into a detached div
    whether or not it is docked. TerminalWidgetBody passes active={isActiveTab}, so PtyTerminal.tsx:460-514 spawns
    cmd.exe /K or zsh and PtyTerminal.tsx:574-586 loads WebGL into a node that is not in the document. The panel
    defaults to collapsed at settingsStore.ts:277, so this happens for every new user on first session expand.
    UsageWidgetCard.tsx:114 also polls while undocked. Gate active on the slot being docked and skip the interval when
    undocked.
  - Every keystroke rerenders the whole file tree. ExplorerPane.tsx:66 subscribes to the entire buffersByTabId, :69 to
    the entire explorer store, and :70 recomputes the view model on any change for any session. Rows at :210 are not
    virtualized. updateDraft fires per keystroke. Select only dirty tab ids with useShallow, slice per session, and
    virtualize or memo rows.
  - Explorer graph rebuilds from scratch per directory load. explorerStore.ts:664-687 runs buildNodeGraph over every
    cached dir, then replaceSessionGraph filters both maps. Expanding N dirs is quadratic. patchSessionGraphCreate
    already exists; patch the loaded dir incrementally.
  - Whole-store subscriptions remain. useSessionRunnerController.ts:28-29, SessionDetail.tsx:439,
    SplitWidgetPanel.tsx:623, SplitSwapLayout.tsx:399, WorkspaceStack.tsx:470-471. Every mounted SessionPanel rerenders
    on every diff refresh or status flip, and mountedIds at SessionDetail.tsx:442-449 only grows. contextEnv at
    useSessionRunnerController.ts:328 rebuilds every render.
  - Startup scans the filesystem twice. App.tsx:779 and :806 call two commands that each run load_codex_history_index
    (ui_state.rs:1018,1174), walking all of ~/.codex/sessions and parsing up to 400 files. latest_claude_hint at
    ui_state.rs:510-585 walks ~/.claude/projects once per worktree per call. Merge into one startup command, build both
    indexes once, index Claude projects by suffix.
  - Hidden xterm instances hold full scrollback. PtyTerminal.tsx:207 keeps 5000 lines per terminal and sessions never
    unmount.

  | Terminals open | Approx buffer memory |
  |----------------|----------------------|
  | 1              | 7 MB                 |
  | 10             | 70 MB                |
    Dispose the xterm (not the PTY) after a few minutes hidden and recreate on show. The CLI redraws its screen. Or
    lower scrollback.
  - PTY bytes travel as base64 inside JSON. pty.rs:361-365 encodes, app.emit string-escapes and broadcasts,
    PtyTerminal.tsx:132-139 decodes per byte. Tauri 2 ipc::Channel with ipc::Response::new(Vec<u8>) delivers an
    ArrayBuffer to one listener. Verify against current Tauri docs before switching.
  - Blocking reqwest client built per call. usage.rs:116,236. Each one spins a runtime thread. Cache one in a OnceLock.
  - Theme applies ~80 style properties. App.tsx:188-441. Move token sets into index.css under [data-theme] and set only
    the attribute. One recalc and 250 fewer lines.
  - Preferences read from disk per hook event. integration_control.rs:55-57 from hooks.rs:1322 and notification.rs:117.
    Cache in managed state, invalidate on save.
  - Negative CLI path cache is permanent. cli_detect.rs:56-60 caches misses. After the in-app install, check_cli stays
  Functionality

  - Claude usage widget cannot work for subscription users. usage.rs:209-227 requires ANTHROPIC_API_KEY in the app's own
    env, then spends a real request every 3 minutes to read anthropic-ratelimit-unified-* headers. Those are
    subscription headers and are absent on key traffic. Subscription users always see "not found". Mirror the Codex
    path: read claudeAiOauth.accessToken from ~/.claude/.credentials.json and call the OAuth usage endpoint
    (/api/oauth/usage with anthropic-beta: oauth-2025-04-20). Verify the response shape with a live token. Return
    structured fields instead of the string the frontend regex-parses at UsageWidgetCard.tsx:18-28.
  - Widget terminals die on session switch. SplitSwapLayout.tsx:214 keys each terminal on session.id, so switching
    sessions unmounts them and PtyTerminal.tsx:291-300 kills the PTY. Key on ptySessionKey and send a cd, or keep a
    terminal set per session.
  - Destructive SCM actions have no confirm. ScmSidebar.tsx:204-208 runs git clean -fd (actions.rs:162) and
    DiffViewer.tsx:260 discards hunks on a single click. SessionList.tsx:700-706 already has a two-click confirm; reuse
    it.
  - Codex resume bindings never form on Windows. Codex hooks are empty there at hooks.rs:393-394, and the notify branch
    at hooks.rs:1399-1412 never calls emit_provider_session_bound. Binding then falls back to the cwd scan the code
    itself calls unreliable at provider_sessions.rs:70-72. Codex notify payloads carry thread-id; extract it and bind on
    first notify.
  - trust_workspace likely writes a key the CLI ignores. hooks.rs:1235-1280 appends to trustedDirectories in
    settings.json. Claude Code stores trust in ~/.claude.json under projects.<path>.hasTrustDialogAccepted. Verify
    against the CLI version you target. If it is a no-op, delete it and the startup loop at App.tsx:738-744.
  - Folder picker is hand-rolled and absent on Linux. window.rs:696-760. tauri-plugin-dialog with directory: true
    replaces it on all three platforms.
  - Widget shell hard-codes zsh. SplitSwapLayout.tsx:188. Use $SHELL resolved in the backend.
  - Backend strings are Chinese literals. ui_state.rs:470,569, usage.rs:220, lib.rs:145, pty.rs:181,256,
    scmCommands.ts:156. The frontend is otherwise localized. Return codes and translate in the UI.
  - Global Esc hides the window. App.tsx:644-648. Esc pressed outside the terminal while on the sessions view hides the
    app. Scope it if that is not the intended popup semantics.
  - Refresh time is unused. last_refreshed_at is returned but never shown. A "updated Xm ago" line makes stale data
    visible.