import { lazy, Suspense, useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { consumeSuppressedEditorReveal, filterVisibleExplorerDirectories, reloadExplorerDirectories, reloadVisibleDirectories, revealExplorerPath } from "./services/editorCommands";
import { setLiquidGlassEffect, GlassMaterialVariant } from "tauri-plugin-liquid-glass-api";
import { motion } from "framer-motion";
import { WorkspaceStack } from "./components/WorkspaceStack";
import { SessionList } from "./components/SessionList";
import { SplitWidgetPanel } from "./components/SplitWidgetPanel";
import { SplitSwapProvider } from "./components/SplitSwapLayout";
import { WorkbenchSidebar } from "./workbench/WorkbenchSidebar";
import { WorkbenchCenter } from "./workbench/WorkbenchCenter";
// 设置面板按需加载：它只在用户打开设置时才需要，
// 静态引入会把它连同依赖一起塞进首屏 chunk。
const Settings = lazy(() => import("./components/Settings"));
import { ensureI18n, getLocaleDirection, resolveEffectiveLocale, useAppI18n } from "./i18n";
import { useSessionStore, type DiffFile, type ClaudeSession } from "./store/sessionStore";
import {
  useSettingsStore,
  isGlassTheme,
  type ThemeMode,
} from "./store/settingsStore";
import { useWorkspaceStore } from "./store/workspaceStore";
import { useWorkbenchStore } from "./store/workbenchStore";
import { useScmStore } from "./store/scmStore";
import { useExplorerStore, type ExplorerEntry } from "./store/explorerStore";
import { useEditorStore } from "./store/editorStore";
import { useShallow } from "zustand/react/shallow";

const spring = { type: "spring" as const, stiffness: 320, damping: 28, mass: 1 };
const MAX_FRONTEND_ERROR_LOGS = 50;

function getExplorerSyncSelection(state: ReturnType<typeof useEditorStore.getState>) {
  return Object.fromEntries(
    Object.entries(state.activeGroupIdBySessionId).flatMap(([sessionId, groupId]) => {
      if (!groupId) return [];
      const group = state.groupsById[groupId];
      if (!group?.activeTabId) return [];
      const tab = state.tabsById[group.activeTabId];
      if (!tab) return [];
      return [[sessionId, `${tab.sessionId}:${tab.viewMode}:${tab.path}:${groupId}`]];
    }),
  );
}

interface FrontendErrorLog {
  id: number;
  source: "window.error" | "unhandledrejection" | "console.error" | "explore-boundary";
  message: string;
  stack?: string | null;
  detail?: string | null;
}

interface BackfilledSessionBinding {
  sessionId: string;
  providerSessionId: string;
}

interface WorkspaceSessionBootstrap {
  recovered: ClaudeSession[];
  backfilled: BackfilledSessionBinding[];
}

export default function App() {
  const { t } = useAppI18n();
  // 只订阅用到的字段。App 是整棵树的根，整店订阅意味着
  // sessionOrderByWorkspace、splitCardItemIdsBySlot 之类的变化
  // 也会让整个界面重渲染。
  const {
    sessions,
    activeSessionId,
    expandedSessionId,
    appendOutput,
    updateSession,
    setDiffFiles,
    setActiveSession,
    setExpandedSession,
  } = useSessionStore(
    useShallow((s) => ({
      sessions: s.sessions,
      activeSessionId: s.activeSessionId,
      expandedSessionId: s.expandedSessionId,
      appendOutput: s.appendOutput,
      updateSession: s.updateSession,
      setDiffFiles: s.setDiffFiles,
      setActiveSession: s.setActiveSession,
      setExpandedSession: s.setExpandedSession,
    }))
  );
  const setScmSnapshot = useScmStore((s) => s.setSnapshot);
  const setScmStatus = useScmStore((s) => s.setStatus);
  const setScmDiffOverride = useScmStore((s) => s.setDiffOverride);

  const settings = useSettingsStore((s) => s.settings);
  const patchSettings = useSettingsStore((s) => s.patchSettings);
  const effectiveLocale = resolveEffectiveLocale(settings.locale);
  const direction = getLocaleDirection(effectiveLocale);
  const settingsOpen = useSettingsStore((s) => s.settingsOpen);
  const closeSettings = useSettingsStore((s) => s.closeSettings);
  const activeWorkspaceId = useWorkspaceStore((s) => s.activeWorkspaceId);
  const sidebarSection = useWorkbenchStore((s) => s.sidebarSection);
  const focusSession = useWorkbenchStore((s) => s.focusSession);
  const focusedSessionId = useWorkbenchStore((s) => s.focusedSessionId);
  const isGlass = isGlassTheme(settings.theme);
  const isSubPageOpen = settingsOpen;
  const refreshInFlightRef = useRef<Record<string, Promise<void> | null>>({});
  const refreshQueuedRef = useRef<Record<string, boolean>>({});
  const refreshQueuedOptionsRef = useRef<Record<string, { reloadExplorer?: boolean; reloadDirs?: string[] } | undefined>>({});
  const refreshLastRunAtRef = useRef<Record<string, number>>({});
  const refreshCooldownTimerRef = useRef<Record<string, ReturnType<typeof setTimeout> | undefined>>({});
  /// 同一个 session 两轮 git 刷新之间的最小间隔
  const MIN_REFRESH_INTERVAL = 400;
  const startedWatcherSessionsRef = useRef<Set<string>>(new Set());
  const watchedWorkdirBySessionRef = useRef<Record<string, string>>({});
  const [frontendErrorLogs, setFrontendErrorLogs] = useState<FrontendErrorLog[]>([]);
  const frontendErrorIdRef = useRef(1);

  const pushFrontendErrorLog = useCallback((log: Omit<FrontendErrorLog, "id">) => {
    setFrontendErrorLogs((current) => {
      const next = [{ id: frontendErrorIdRef.current++, ...log }, ...current];
      return next.slice(0, MAX_FRONTEND_ERROR_LOGS);
    });
  }, []);

  useEffect(() => {
    const originalConsoleError = console.error;
    console.error = (...args: unknown[]) => {
      pushFrontendErrorLog({
        source: "console.error",
        message: args.map((value) => {
          if (value instanceof Error) return value.message;
          return typeof value === "string" ? value : JSON.stringify(value, null, 2);
        }).join(" "),
        stack: args.find((value) => value instanceof Error) instanceof Error ? (args.find((value) => value instanceof Error) as Error).stack ?? null : null,
        detail: null,
      });
      originalConsoleError(...args);
    };

    const handleWindowError = (event: ErrorEvent) => {
      pushFrontendErrorLog({
        source: "window.error",
        message: event.message,
        stack: event.error instanceof Error ? event.error.stack ?? null : null,
        detail: `${event.filename}:${event.lineno}:${event.colno}`,
      });
    };

    const handleUnhandledRejection = (event: PromiseRejectionEvent) => {
      const reason = event.reason;
      pushFrontendErrorLog({
        source: "unhandledrejection",
        message: reason instanceof Error ? reason.message : String(reason),
        stack: reason instanceof Error ? reason.stack ?? null : null,
        detail: null,
      });
    };

    const handleExploreBoundary = (nativeEvent: Event) => {
      const customEvent = nativeEvent as CustomEvent<{ message: string; stack?: string | null }>;
      pushFrontendErrorLog({
        source: "explore-boundary",
        message: customEvent.detail?.message ?? t("notifications.unknownError"),
        stack: customEvent.detail?.stack ?? null,
        detail: null,
      });
    };

    window.addEventListener("error", handleWindowError);
    window.addEventListener("unhandledrejection", handleUnhandledRejection);
    window.addEventListener("explore-boundary-error", handleExploreBoundary as EventListener);

    return () => {
      console.error = originalConsoleError;
      window.removeEventListener("error", handleWindowError);
      window.removeEventListener("unhandledrejection", handleUnhandledRejection);
      window.removeEventListener("explore-boundary-error", handleExploreBoundary as EventListener);
    };
  }, [pushFrontendErrorLog]);

  // ── 主题注入：根据 settings.theme 向 :root 写入 CSS 变量 ──────
  useEffect(() => {
    void ensureI18n(effectiveLocale);
    document.documentElement.lang = effectiveLocale;
    document.documentElement.dir = direction;
    if ("__TAURI_INTERNALS__" in window) {
      invoke("set_app_locale", { locale: effectiveLocale }).catch(() => {});
    }
  }, [direction, effectiveLocale]);

  useEffect(() => {
    // 主题 token 定义在 index.css 的 [data-theme=...] 选择器里，
    // 这里只切属性：一次样式重算代替约 80 次 root.style.setProperty。
    const applyTheme = (mode: Exclude<ThemeMode, "system">) => {
      document.documentElement.setAttribute("data-theme", mode);
    };

    if (settings.theme === "system") {
      // 跟随系统
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      applyTheme(mq.matches ? "dark" : "light");
      const listener = (e: MediaQueryListEvent) => applyTheme(e.matches ? "dark" : "light");
      mq.addEventListener("change", listener);
      return () => mq.removeEventListener("change", listener);
    } else {
      applyTheme(settings.theme);
    }
  }, [settings.theme]);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    if (settings.theme === "glass") {
      setLiquidGlassEffect({
        cornerRadius: 24,
        variant: GlassMaterialVariant.Clear,
      }).catch(() => {});
      return;
    }

    setLiquidGlassEffect({ enabled: false }).catch(() => {});
  }, [settings.theme]);

  // ── 切换 Workspace 时自动将 activeSession 切换到该 Workspace 的第一个 session ──
  useEffect(() => {
    const currentActive = useSessionStore.getState().activeSessionId;
    const currentSession = useSessionStore.getState().sessions.find((s) => s.id === currentActive);
    // 当前 activeSession 不属于当前 workspace 时，重新选择
    if (currentSession?.workspaceId !== activeWorkspaceId) {
      const wsSessions = useSessionStore.getState().sessions.filter(
        (s) => s.workspaceId === activeWorkspaceId
      );
      const fallbackId = wsSessions[0]?.id ?? null;
      setActiveSession(fallbackId);
      focusSession(fallbackId);
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeWorkspaceId]);

  // activeSession 必须属于当前 workspace，防止切换后仍显示旧 workspace 的内容
  const activeSession = sessions.find(
    (s) => s.id === activeSessionId && s.workspaceId === activeWorkspaceId
  );
  const expandedSession = sessions.find((s) => s.id === expandedSessionId) ?? null;
  const visibleSplitSessionId = expandedSession?.workspaceId === activeWorkspaceId
    ? expandedSession.id
    : null;
  const workbenchSession = sessions.find(
    (s) => s.id === focusedSessionId && s.workspaceId === activeWorkspaceId
  ) ?? activeSession ?? null;

  const refreshSessionDiff = useCallback((sessionId?: string | null, options?: { reloadExplorer?: boolean; reloadDirs?: string[] }) => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    const targetId = sessionId ?? useSessionStore.getState().activeSessionId;
    if (!targetId) return;

    const queueRefresh = () => {
      refreshQueuedRef.current[targetId] = true;
      const previousOptions = refreshQueuedOptionsRef.current[targetId];
      refreshQueuedOptionsRef.current[targetId] = {
        reloadExplorer: previousOptions?.reloadExplorer || options?.reloadExplorer,
        reloadDirs: [...new Set([...(previousOptions?.reloadDirs ?? []), ...(options?.reloadDirs ?? [])])],
      };
    };

    if (refreshInFlightRef.current[targetId]) {
      queueRefresh();
      return;
    }

    // 冷却窗口：文件监听事件是成串到达的，每串都跑一整轮 git
    // 会让刷新本身成为 CPU 大头。窗口内的请求合并成一次尾部刷新。
    const sinceLastRun = Date.now() - (refreshLastRunAtRef.current[targetId] ?? 0);
    if (sinceLastRun < MIN_REFRESH_INTERVAL) {
      queueRefresh();
      if (!refreshCooldownTimerRef.current[targetId]) {
        refreshCooldownTimerRef.current[targetId] = setTimeout(() => {
          delete refreshCooldownTimerRef.current[targetId];
          if (!refreshQueuedRef.current[targetId]) return;
          refreshQueuedRef.current[targetId] = false;
          const queuedOptions = refreshQueuedOptionsRef.current[targetId];
          delete refreshQueuedOptionsRef.current[targetId];
          refreshSessionDiff(targetId, queuedOptions);
        }, MIN_REFRESH_INTERVAL - sinceLastRun);
      }
      return;
    }

    refreshLastRunAtRef.current[targetId] = Date.now();

    const runRefresh = async () => {
      const session = useSessionStore.getState().sessions.find((s) => s.id === targetId);
      if (!session) return;

      await Promise.all([
        invoke("get_git_status", {
          sessionId: session.id,
          workdir: session.workdir,
        }),
        session.baseBranch
          ? invoke("get_git_diff_session_worktree", {
              sessionId: session.id,
              workdir: session.workdir,
              baseBranch: session.baseBranch,
            })
          : invoke("get_git_diff", {
              sessionId: session.id,
              workdir: session.workdir,
            }),
      ]).catch(() => {});

      if (options?.reloadDirs && options.reloadDirs.length > 0) {
        await reloadExplorerDirectories(session.id, options.reloadDirs).catch(() => {});
      } else if (options?.reloadExplorer) {
        await reloadVisibleDirectories(session.id).catch(() => {});
      }
    };

    const task = runRefresh().finally(() => {
      refreshInFlightRef.current[targetId] = null;
      if (refreshQueuedRef.current[targetId]) {
        refreshQueuedRef.current[targetId] = false;
        const queuedOptions = refreshQueuedOptionsRef.current[targetId];
        delete refreshQueuedOptionsRef.current[targetId];
        refreshSessionDiff(targetId, queuedOptions ?? options);
      }
    });

    refreshInFlightRef.current[targetId] = task;
  }, []);


  useEffect(() => {
    const unsubscribe = useEditorStore.subscribe((state, prevState) => {
      const nextSelections = getExplorerSyncSelection(state);
      const prevSelections = getExplorerSyncSelection(prevState);
      Object.entries(nextSelections).forEach(([sessionId, nextSelectionKey]) => {
        if (prevSelections[sessionId] === nextSelectionKey) return;
        const groupId = state.activeGroupIdBySessionId[sessionId];
        if (!groupId) return;
        const group = state.groupsById[groupId];
        if (!group?.activeTabId) return;
        const tab = state.tabsById[group.activeTabId];
        if (!tab) return;
        if (consumeSuppressedEditorReveal(sessionId)) return;
        revealExplorerPath(sessionId, tab.path, true, "editor");
      });
    });

    return () => unsubscribe();
  }, []);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    const visibleSessions = [activeSession, workbenchSession].filter((session): session is ClaudeSession => !!session);
    const nextWatchMap = new Map(visibleSessions.map((session) => [session.id, session.workdir]));

    nextWatchMap.forEach((workdir, sessionId) => {
      const knownWorkdir = watchedWorkdirBySessionRef.current[sessionId];
      if (knownWorkdir === workdir && startedWatcherSessionsRef.current.has(sessionId)) {
        return;
      }
      if (startedWatcherSessionsRef.current.has(sessionId) && knownWorkdir && knownWorkdir !== workdir) {
        void invoke("stop_git_watch", { sessionId }).catch(() => {});
        startedWatcherSessionsRef.current.delete(sessionId);
      }
      watchedWorkdirBySessionRef.current[sessionId] = workdir;
      startedWatcherSessionsRef.current.add(sessionId);
      void invoke("start_git_watch", { sessionId, workdir }).catch(() => {});
    });

    [...startedWatcherSessionsRef.current].forEach((sessionId) => {
      if (nextWatchMap.has(sessionId)) return;
      startedWatcherSessionsRef.current.delete(sessionId);
      delete watchedWorkdirBySessionRef.current[sessionId];
      void invoke("stop_git_watch", { sessionId }).catch(() => {});
    });
  }, [activeSession, workbenchSession]);

  useEffect(() => () => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    [...startedWatcherSessionsRef.current].forEach((sessionId) => {
      void invoke("stop_git_watch", { sessionId }).catch(() => {});
    });
    startedWatcherSessionsRef.current.clear();
    watchedWorkdirBySessionRef.current = {};
  }, []);

  // ── Esc 关闭 ──────────────────────────────────────────────
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (settingsOpen) {
        closeSettings();
        return;
      }
      if (sidebarSection !== "sessions") {
        useWorkbenchStore.getState().resetWorkbenchMode();
        return;
      }
      invoke("close_popup").catch(() => {});
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [settingsOpen, closeSettings, sidebarSection]);

  // ── 浮窗位置 / 大小记忆：用户拖动/调整后防抖 500ms 写盘 ──
  // 注意：只在基础状态（非展开）下保存，展开状态是临时的，不应覆盖记忆。
  // expandedSessionId 不为 null 表示终端面板已展开。
  const boundsTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const suppressBoundsPersistenceRef = useRef(false);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    const win = getCurrentWindow();

    // onMoved payload = PhysicalPosition { x, y }（物理像素）
    // onResized payload = PhysicalSize { width, height }（物理像素）
    // 直接从 payload 读取，无需额外异步调用。
    const debouncedSave = (physX: number, physY: number, physW: number, physH: number) => {
      // 仅 overlay 展开状态下跳过，避免把临时放大的尺寸写盘
      if (suppressBoundsPersistenceRef.current) return;
      if (boundsTimerRef.current) clearTimeout(boundsTimerRef.current);
      boundsTimerRef.current = setTimeout(async () => {
        try {
          const scaleFactor = await win.scaleFactor();
          await invoke("save_popup_bounds", {
            x: physX / scaleFactor,
            y: physY / scaleFactor,
            width: physW / scaleFactor,
            height: physH / scaleFactor,
          });
        } catch {
          // 静默失败
        }
      }, 500);
    };

    // onMoved：payload 只有位置，宽高需读当前值
    const unlistenMoved = win.onMoved(async ({ payload: pos }) => {
      if (suppressBoundsPersistenceRef.current) return;
      try {
        const size = await win.innerSize();
        debouncedSave(pos.x, pos.y, size.width, size.height);
      } catch { /* 静默 */ }
    });

    // onResized：payload 只有尺寸，位置需读当前值
    const unlistenResized = win.onResized(async ({ payload: size }) => {
      if (suppressBoundsPersistenceRef.current) return;
      try {
        const pos = await win.outerPosition();
        debouncedSave(pos.x, pos.y, size.width, size.height);
      } catch { /* 静默 */ }
    });

    return () => {
      if (boundsTimerRef.current) clearTimeout(boundsTimerRef.current);
      unlistenMoved.then((f) => f()).catch(() => {});
      unlistenResized.then((f) => f()).catch(() => {});
    };
  }, []);

  // ── 弹窗重新显示时（托盘点击），保持当前界面状态（PTY 或菜单）──
  // 不再强制收起 PTY，让用户留在上次的位置

  // ── 通知点击唤起弹窗时，展开最近活跃的 session ──
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    const unlisten = listen<{ session_id?: string | null }>("popup-focused", ({ payload }) => {
      const sid = payload?.session_id?.trim();
      const { activeSessionId: aid, sessions: ss } = useSessionStore.getState();
      const target =
        sid && ss.some((s) => s.id === sid)
          ? sid
          : (aid ?? ss[ss.length - 1]?.id ?? null);
      if (target) {
        requestAnimationFrame(() => {
          setActiveSession(target);
          setExpandedSession(target);
          focusSession(target);
          refreshSessionDiff(target);
        });
      }
    });
    return () => { unlisten.then((f) => f()).catch(() => {}); };
  }, [setExpandedSession, refreshSessionDiff]);

  // ── 启动时批量信任所有已有 workspace 目录（写入 claude settings）──
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    const { workspaces } = useWorkspaceStore.getState();
    workspaces.forEach((ws) => {
      invoke("trust_workspace", { path: ws.path }).catch(() => {});
    });
  }, []);

  // ── 启动时加载保存的 API Key ──────────────────────────────
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    (["anthropic", "openai"] as const).forEach((provider) => {
      invoke<string>("load_api_key", { provider })
        .then((key) => {
          if (key) {
            useSettingsStore.setState((s) => ({
              settings: {
                ...s.settings,
                apiKeys: { ...s.settings.apiKeys, [provider]: key },
              },
            }));
          }
        })
        .catch(() => {});
    });
  }, []);

  // ── 启动时恢复缺失 session，并为已有旧 session 回填 provider resume 绑定 ──
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;

    let cancelled = false;

    (async () => {
      const workspaces = useWorkspaceStore.getState().workspaces;
      const workspaceInputs = workspaces.map((workspace) => ({
        workspaceId: workspace.id,
        workspacePath: workspace.path,
      }));
      const knownIds = new Set(useSessionStore.getState().sessions.map((s) => s.id));

      // 回填候选只会来自已经在 store 里的 session：恢复出来的 session 一定带
      // providerSessionId，永远不满足候选条件，而恢复本身会跳过已存在的 id，
      // 所以在调用前算候选和原来在恢复之后算是等价的。
      const backfillCandidates = useSessionStore
        .getState()
        .sessions
        .filter((session) => {
          if (!session.worktreePath?.trim()) return false;
          if (session.providerSessionId?.trim()) return false;
          return session.runner.type === "claude-code" || session.runner.type === "codex";
        })
        .map((session) => ({
          sessionId: session.id,
          runnerType: session.runner.type,
          worktreePath: session.worktreePath ?? null,
          providerSessionId: session.providerSessionId ?? null,
        }));

      const bootstrap = await invoke<WorkspaceSessionBootstrap>("bootstrap_workspace_sessions", {
        workspaces: workspaceInputs,
        existingSessionIds: [...knownIds],
        sessions: backfillCandidates,
      }).catch(() => null);

      if (cancelled || !bootstrap) return;

      if (bootstrap.recovered.length > 0) {
        useSessionStore.getState().mergeRecoveredSessions(bootstrap.recovered);
      }

      bootstrap.backfilled.forEach(({ sessionId, providerSessionId }) => {
        const current = useSessionStore.getState().sessions.find((session) => session.id === sessionId);
        if (!current || current.providerSessionId?.trim()) return;
        updateSession(sessionId, { providerSessionId });
      });
    })();

    return () => {
      cancelled = true;
    };
  }, [updateSession]);

  // ── 监听 Rust 侧事件 ──────────────────────────────────────
  useEffect(() => {
    // 非 Tauri 环境（纯浏览器 dev）下 listen 会因为缺少 __TAURI_INTERNALS__ 而报错，跳过
    if (!("__TAURI_INTERNALS__" in window)) return;

    // 旧接口：claude-output（claude-code CLI）
    const u1 = listen<{ session_id: string; line: string }>(
      "claude-output",
      ({ payload }) => appendOutput(payload.session_id, payload.line)
    );

    // 新接口：runner-output（统一 Runner）
    const u2 = listen<{ session_id: string; line: string }>(
      "runner-output",
      ({ payload }) => appendOutput(payload.session_id, payload.line)
    );

    // Runner 完成
    const u3 = listen<{ session_id: string; error?: string }>(
      "runner-done",
      ({ payload }) => {
        if (payload.error) {
          updateSession(payload.session_id, { status: "error", currentTask: payload.error });
        } else {
          updateSession(payload.session_id, { status: "done", currentTask: "已完成" });
        }
        refreshSessionDiff(payload.session_id);
      }
    );

    // 旧接口：claude-status
    const u4 = listen<{ session_id: string; status: string; task: string }>(
      "claude-status",
      ({ payload }) => {
        updateSession(payload.session_id, {
          status: payload.status as Parameters<typeof updateSession>[1]["status"],
          currentTask: payload.task,
        });
      }
    );

    // git diff 更新
    const u5 = listen<{ session_id: string; files: DiffFile[] }>(
      "diff-update",
      ({ payload }) => {
        setDiffFiles(payload.session_id, payload.files);
        setScmSnapshot(payload.session_id, payload.files);
      }
    );

    const u5b = listen<{ session_id: string; groups: import("./store/scmStore").ScmStatusGroups }>(
      "scm-status-update",
      ({ payload }) => {
        setScmStatus(payload.session_id, payload.groups);
      }
    );

    const u5c = listen<{ session_id: string; mode: string; file: DiffFile }>(
      "scm-diff-side-update",
      ({ payload }) => {
        if (payload.mode === "staged" || payload.mode === "unstaged") {
          setScmDiffOverride(payload.session_id, payload.file);
        }
      }
    );

    const u5d = listen<{ session_id: string; reason?: string; event_type?: "create" | "delete" | "rename" | "change" | "git" | "batch"; paths?: string[]; reload_dirs?: string[]; path_kinds?: Record<string, ExplorerEntry["kind"]>; rename_pairs?: Array<{ oldPath: string; newPath: string }> }>(
      "scm-refresh-requested",
      ({ payload }) => {
        const eventType = payload.event_type ?? "batch";
        const graphPatched = useExplorerStore.getState().applyWatcherEvent(payload.session_id, {
          eventType,
          paths: payload.paths ?? [],
          pathKinds: payload.path_kinds,
          renamePairs: payload.rename_pairs,
        });

        const reloadDirs = filterVisibleExplorerDirectories(payload.session_id, payload.reload_dirs ?? []);
        const shouldRefreshScmOnly = eventType === "git" || eventType === "batch" || (payload.paths?.length ?? 0) === 0;
        if (reloadDirs.length > 0) {
          refreshSessionDiff(payload.session_id, { reloadDirs });
          return;
        }

        if (!graphPatched && shouldRefreshScmOnly) {
          refreshSessionDiff(payload.session_id);
        }
      }
    );

    // PTY 退出：将 running/waiting/suspended 状态的 session 标记为 done
    // SessionPanel 关闭后不再常驻，此处补全全局兜底监听
    const u6 = listen<{ session_id: string }>(
      "pty-exit",
      ({ payload }) => {
        // 延迟 1.2s 与 SessionPanel 内的逻辑保持一致
        setTimeout(() => {
          const s = useSessionStore.getState().sessions.find((x) => x.id === payload.session_id);
          if (s && (s.status === "running" || s.status === "waiting" || s.status === "suspended")) {
            updateSession(payload.session_id, { status: "done" });
          }
        }, 1200);
      }
    );

    // Provider 原生会话绑定（用于下次进入时 resume）
    const u7 = listen<{ session_id: string; runner_type: string; provider_session_id: string }>(
      "provider-session-bound",
      ({ payload }) => {
        if (!payload.session_id || !payload.provider_session_id) return;
        const session = useSessionStore
          .getState()
          .sessions.find((x) => x.id === payload.session_id);
        const existing = session?.providerSessionId?.trim();
        // 避免被“新建但空壳”的 provider 会话覆盖已有可恢复会话 ID
        if (existing && existing !== payload.provider_session_id) {
          return;
        }
        updateSession(payload.session_id, {
          providerSessionId: payload.provider_session_id,
        });
        void invoke("save_recovery_binding", {
          input: {
            sessionId: payload.session_id,
            runnerType: payload.runner_type,
            providerSessionId: payload.provider_session_id,
            worktreePath: session?.worktreePath ?? null,
          },
        }).catch(() => {});
      }
    );

    return () => {
      [u1, u2, u3, u4, u5, u5b, u5c, u5d, u6, u7].forEach((p) => p.then((f) => f()).catch(() => {}));
    };
  }, [appendOutput, updateSession, setDiffFiles, setScmSnapshot, setScmStatus, setScmDiffOverride, refreshSessionDiff]);

  // ── 会话切换时主动拉一次 Diff（覆盖非 running / 外部改动场景）──
  useEffect(() => {
    if (!activeSession?.id) return;
    refreshSessionDiff(activeSession.id);
  }, [activeSession?.id, refreshSessionDiff]);

  useEffect(() => {
    if (sidebarSection === "sessions") return;
    if (workbenchSession) return;
    useWorkbenchStore.getState().resetWorkbenchMode();
  }, [sidebarSection, workbenchSession]);

  const splitSidebarWidth = settings.splitPaneSidebarWidth;
  const splitWidgetPanelWidth = settings.splitWidgetPanelWidth;
  const splitWidgetPanelCollapsed = settings.splitWidgetPanelCollapsed;

  const handleSplitPanePointerDown = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = splitSidebarWidth;
    const minWidth = 280;
    const maxWidth = 560;

    // 按帧合并：pointermove 每秒能触发上百次，
    // 每次都写 store 会连带触发持久化与整棵树的重渲染。
    let frame: number | null = null;
    let nextWidth = startWidth;

    const applyWidth = () => {
      frame = null;
      patchSettings({ splitPaneSidebarWidth: nextWidth });
    };

    const handlePointerMove = (moveEvent: PointerEvent) => {
      nextWidth = Math.min(maxWidth, Math.max(minWidth, Math.round(startWidth + moveEvent.clientX - startX)));
      if (frame === null) frame = requestAnimationFrame(applyWidth);
    };

    const handlePointerUp = () => {
      if (frame !== null) cancelAnimationFrame(frame);
      applyWidth();
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
    };

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp);
  }, [patchSettings, splitSidebarWidth]);

  const handleWidgetPanePointerDown = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = splitWidgetPanelWidth;
    const minWidth = 220;
    const maxWidth = 720;

    // 同上：按帧合并 pointermove，避免每次移动都写 store
    let frame: number | null = null;
    let nextWidth = startWidth;

    const applyWidth = () => {
      frame = null;
      patchSettings({
        splitWidgetPanelWidth: nextWidth,
        splitWidgetPanelCollapsed: false,
      });
    };

    const handlePointerMove = (moveEvent: PointerEvent) => {
      nextWidth = Math.min(maxWidth, Math.max(minWidth, Math.round(startWidth - (moveEvent.clientX - startX))));
      if (frame === null) frame = requestAnimationFrame(applyWidth);
    };

    const handlePointerUp = () => {
      if (frame !== null) cancelAnimationFrame(frame);
      applyWidth();
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
    };

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp);
  }, [patchSettings, splitWidgetPanelWidth]);

  const menuContent = (
    <div style={{
      flex: 1,
      minHeight: 0,
      overflowY: "auto",
      overflowX: "hidden",
      position: "relative",
      scrollbarWidth: "none",
      zIndex: 1,
    }}>
      <div style={{ padding: "6px 18px 4px" }}>
        <WorkspaceStack />
      </div>

      <div style={{ padding: "0 18px 12px" }}>
        <SessionList />
      </div>

      <div style={{
        position: "sticky",
        bottom: 0,
        left: 0,
        right: 0,
        height: 28,
        background: "linear-gradient(to bottom, transparent, var(--ci-bg-grad))",
        pointerEvents: "none",
        flexShrink: 0,
      }} />
    </div>
  );

  return (
    <>
      <div style={{
        width: "100vw",
        height: "100vh",
        padding: 0,
        boxSizing: "border-box",
        background: "transparent",
      }}>
        {frontendErrorLogs.length > 0 && (
          <div style={{
            position: "fixed",
            right: 12,
            bottom: 36,
            width: 420,
            maxHeight: 260,
            overflow: "auto",
            zIndex: 9999,
            border: "1px solid var(--ci-toolbar-border)",
            background: "rgba(20,20,24,0.96)",
            color: "var(--ci-text)",
            fontSize: 11,
            lineHeight: 1.5,
            boxShadow: "0 12px 30px rgba(0,0,0,0.28)",
          }}>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", padding: "8px 10px", borderBottom: "1px solid var(--ci-toolbar-border)", position: "sticky", top: 0, background: "rgba(20,20,24,0.98)" }}>
              <span style={{ fontWeight: 700, color: "var(--ci-deleted-text)" }}>{t("app.frontendErrors")}</span>
              <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                <button
                  onClick={() => {
                    const content = frontendErrorLogs.map((log) => [
                      `[${log.source}] ${log.message}`,
                      log.detail ?? "",
                      log.stack ?? "",
                    ].filter(Boolean).join("\n")).join("\n\n---\n\n");
                    void navigator.clipboard.writeText(content).catch(() => {});
                  }}
                  style={{ background: "none", border: "none", color: "var(--ci-text-dim)", cursor: "pointer", fontSize: 11 }}
                >
                  {t("app.copyAll")}
                </button>
                <button
                  onClick={() => setFrontendErrorLogs([])}
                  style={{ background: "none", border: "none", color: "var(--ci-text-dim)", cursor: "pointer", fontSize: 11 }}
                >
                  {t("common.clear")}
                </button>
              </div>
            </div>
            <div style={{ padding: 10, display: "flex", flexDirection: "column", gap: 10 }}>
              {frontendErrorLogs.map((log) => (
                <div key={log.id} style={{ borderBottom: "1px solid rgba(255,255,255,0.06)", paddingBottom: 8 }}>
                  <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
                    <div style={{ color: "var(--ci-yellow)", fontWeight: 600 }}>{log.source}</div>
                    <button
                      onClick={() => {
                        const content = [
                          `[${log.source}] ${log.message}`,
                          log.detail ?? "",
                          log.stack ?? "",
                        ].filter(Boolean).join("\n");
                        void navigator.clipboard.writeText(content).catch(() => {});
                      }}
                      style={{ background: "none", border: "none", color: "var(--ci-text-dim)", cursor: "pointer", fontSize: 10, padding: 0 }}
                    >
                      {t("common.copy")}
                    </button>
                  </div>
                  <div style={{ marginTop: 4, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{log.message}</div>
                  {log.detail && <div style={{ marginTop: 4, color: "var(--ci-text-dim)", whiteSpace: "pre-wrap" }}>{log.detail}</div>}
                  {log.stack && <pre style={{ marginTop: 6, whiteSpace: "pre-wrap", color: "var(--ci-text-dim)", fontSize: 10 }}>{log.stack}</pre>}
                </div>
              ))}
            </div>
          </div>
        )}
        <motion.div
          transition={spring}
          style={{
            width: "100%",
            height: "100%",
            position: "relative",
            borderRadius: "var(--ci-shell-radius)",
            border: isGlass ? "none" : "1px solid var(--ci-window-edge)",
            background: isGlass ? "transparent" : "var(--ci-window-bg)",
            boxShadow: "var(--ci-window-shadow)",
            clipPath: "inset(0 round var(--ci-shell-radius))",
            overflow: "hidden",
            display: "flex",
            flexDirection: "column",
            isolation: "isolate",
          }}
        >
          <div style={{
            display: "flex",
            flex: 1,
            minHeight: 0,
            flexDirection: "column",
          }}>
            <Suspense fallback={null}>
              <Settings />
            </Suspense>

            <div
              style={{
                display: "flex",
                flexDirection: "column",
                flex: 1,
                minHeight: 0,
                opacity: isSubPageOpen ? 0 : 1,
                pointerEvents: isSubPageOpen ? "none" : "auto",
                visibility: isSubPageOpen ? "hidden" : "visible",
              }}
            >
              <SplitSwapProvider
                sessionDetailEmptyState={
                  <div style={{
                    height: "100%",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    padding: 24,
                  }}>
                    <div style={{
                      maxWidth: 260,
                      padding: "20px 22px",
                      borderRadius: 18,
                      background: "var(--ci-surface)",
                      border: "1px solid var(--ci-toolbar-border)",
                      color: "var(--ci-text-dim)",
                      fontSize: 12,
                      textAlign: "center",
                      lineHeight: 1.7,
                    }}>
                      {expandedSessionId && !visibleSplitSessionId
                        ? t("app.split.emptyOtherWorkspace")
                        : t("app.split.emptyPickSession")}
                    </div>
                  </div>
                }
              >
                <div style={{ display: "flex", flex: 1, minHeight: 0 }}>
                  <div style={{
                    width: splitSidebarWidth,
                    flexShrink: 0,
                    display: "flex",
                    flexDirection: "column",
                    minHeight: 0,
                    background: isGlass ? "var(--ci-toolbar-bg)" : "transparent",
                  }}>
                    <WorkbenchSidebar
                      session={workbenchSession}
                      menuContent={menuContent}
                      onRefreshDiff={refreshSessionDiff}
                    />
                  </div>

                  <div
                    onPointerDown={handleSplitPanePointerDown}
                    title={t("app.split.resizeSidebar")}
                    style={{
                      width: 10,
                      marginInlineStart: -5,
                      marginInlineEnd: -5,
                      cursor: "col-resize",
                      zIndex: 3,
                      touchAction: "none",
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "center",
                      flexShrink: 0,
                    }}
                  >
                    <div style={{
                      width: 2,
                      height: "100%",
                      background: "var(--ci-toolbar-border)",
                      borderRadius: 999,
                    }} />
                  </div>

                  <div style={{ flex: 1, minWidth: 0, minHeight: 0, position: "relative", display: "flex", borderInlineStart: "1px solid var(--ci-toolbar-border)" }}>
                    <div style={{ flex: 1, minWidth: 0, minHeight: 0 }}>
                      <WorkbenchCenter session={workbenchSession} onRefreshDiff={refreshSessionDiff} />
                    </div>
                  </div>

                  {!splitWidgetPanelCollapsed && (
                    <div
                      onPointerDown={handleWidgetPanePointerDown}
                      title={t("app.split.resizeWidgets")}
                      style={{
                        width: 10,
                        marginInlineStart: -5,
                        marginInlineEnd: -5,
                        cursor: "col-resize",
                        zIndex: 3,
                        touchAction: "none",
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "center",
                        flexShrink: 0,
                      }}
                    >
                      <div style={{
                        width: 2,
                        height: "100%",
                        background: "var(--ci-toolbar-border)",
                        borderRadius: 999,
                        opacity: 0.8,
                      }} />
                    </div>
                  )}

                  {splitWidgetPanelCollapsed ? (
                    <div style={{
                      width: 34,
                      flexShrink: 0,
                      borderInlineStart: "1px solid var(--ci-toolbar-border)",
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "center",
                      background: "transparent",
                    }}>
                      <button
                        onClick={() => patchSettings({ splitWidgetPanelCollapsed: false })}
                        title={t("app.split.expandWidgets")}
                        onMouseEnter={e => {
                          e.currentTarget.style.color = "var(--ci-text)";
                          e.currentTarget.style.opacity = "0.8";
                        }}
                        onMouseLeave={e => {
                          e.currentTarget.style.color = "var(--ci-text-dim)";
                          e.currentTarget.style.opacity = "1";
                        }}
                        style={{
                          background: "none",
                          border: "none",
                          color: "var(--ci-text-dim)",
                          cursor: "pointer",
                          fontSize: 10,
                          fontWeight: 700,
                          letterSpacing: "0.08em",
                          textTransform: "uppercase",
                          writingMode: "vertical-rl",
                          padding: "0",
                          transition: "color 0.12s, opacity 0.12s",
                        }}
                      >
                        {t("app.split.widgets")}
                      </button>
                    </div>
                  ) : (
                    <div style={{
                      width: splitWidgetPanelWidth,
                      flexShrink: 0,
                      minHeight: 0,
                      background: isGlass ? "var(--ci-toolbar-bg)" : "transparent",
                    }}>
                      <SplitWidgetPanel />
                    </div>
                  )}
                </div>
              </SplitSwapProvider>
            </div>
          </div>
        </motion.div>
      </div>
    </>
  );
}
