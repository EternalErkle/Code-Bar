import type { StateStorage } from "zustand/middleware";

const PERSIST_KEYS = [
  "code-bar-sessions",
  "code-bar-workspaces",
  "code-bar-settings",
] as const;

type PersistKey = typeof PERSIST_KEYS[number];

interface DeletedSessionRef {
  sessionId: string;
  workspaceId?: string | null;
}

interface DeletedWorkspaceRef {
  workspaceId: string;
  path?: string | null;
}

interface DeletedUiState {
  sessionIds?: string[];
  workspaceIds?: string[];
  sessions?: DeletedSessionRef[];
  workspaces?: DeletedWorkspaceRef[];
}

interface DeletedMatchers {
  legacySessionIds: Set<string>;
  legacyWorkspaceIds: Set<string>;
  sessionRefs: DeletedSessionRef[];
  workspaceRefs: DeletedWorkspaceRef[];
}

interface PersistedSessionLike {
  id: string;
  workspaceId: string;
  createdAt?: number;
}

interface PersistedSessionsState {
  state?: {
    sessions?: PersistedSessionLike[];
    activeSessionId?: string | null;
    sessionOrderByWorkspace?: Record<string, string[]>;
  };
  version?: number;
}

interface PersistedWorkspaceLike {
  id: string;
  path?: string;
  order?: number;
}

interface PersistedWorkspacesState {
  state?: {
    workspaces?: PersistedWorkspaceLike[];
    activeWorkspaceId?: string | null;
  };
  version?: number;
}

let cachedHomeDir = "";

function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function invokeSafe<T>(command: string, args: Record<string, unknown>): Promise<T | null> {
  if (!isTauriRuntime()) return null;

  try {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<T>(command, args);
  } catch {
    return null;
  }
}

function parseJson<T>(value: string | null): T | null {
  if (!value) return null;

  try {
    return JSON.parse(value) as T;
  } catch {
    return null;
  }
}

function uniqueStrings(values: string[]): string[] {
  const seen = new Set<string>();
  const next: string[] = [];

  values.forEach((value) => {
    if (!value || seen.has(value)) return;
    seen.add(value);
    next.push(value);
  });

  return next;
}

function expandHomePath(path: string): string {
  if (!path.startsWith("~")) return path;
  if (!cachedHomeDir) return path;
  if (path === "~") return cachedHomeDir;
  if (path.startsWith("~/") || path.startsWith("~\\")) {
    return `${cachedHomeDir}/${path.slice(2)}`;
  }
  return path;
}

function normalizePath(path: string | null | undefined): string {
  const trimmed = (path ?? "").trim();
  if (!trimmed) return "";
  return expandHomePath(trimmed).replace(/[\\/]+$/, "");
}

function normalizeDeletedSessionRef(ref: DeletedSessionRef | null | undefined): DeletedSessionRef | null {
  const sessionId = ref?.sessionId?.trim() ?? "";
  if (!sessionId) return null;

  const workspaceId = ref?.workspaceId?.trim() ?? "";
  return workspaceId ? { sessionId, workspaceId } : { sessionId };
}

function normalizeDeletedWorkspaceRef(ref: DeletedWorkspaceRef | null | undefined): DeletedWorkspaceRef | null {
  const workspaceId = ref?.workspaceId?.trim() ?? "";
  if (!workspaceId) return null;

  const path = normalizePath(ref?.path);
  return path ? { workspaceId, path } : { workspaceId };
}

function uniqueByKey<T>(values: T[], keyOf: (value: T) => string): T[] {
  const seen = new Set<string>();
  const next: T[] = [];

  values.forEach((value) => {
    const key = keyOf(value);
    if (!key || seen.has(key)) return;
    seen.add(key);
    next.push(value);
  });

  return next;
}

function buildDeletedMatchers(state: DeletedUiState): DeletedMatchers {
  const legacySessionIds = new Set(uniqueStrings((state.sessionIds ?? []).map((id) => id.trim())));
  const legacyWorkspaceIds = new Set(uniqueStrings((state.workspaceIds ?? []).map((id) => id.trim())));
  const sessionRefs = uniqueByKey(
    (state.sessions ?? [])
      .map((ref) => normalizeDeletedSessionRef(ref))
      .filter((ref): ref is DeletedSessionRef => !!ref),
    (ref) => `${ref.sessionId}::${ref.workspaceId ?? ""}`
  );
  const workspaceRefs = uniqueByKey(
    (state.workspaces ?? [])
      .map((ref) => normalizeDeletedWorkspaceRef(ref))
      .filter((ref): ref is DeletedWorkspaceRef => !!ref),
    (ref) => `${ref.workspaceId}::${normalizePath(ref.path)}`
  );

  return {
    legacySessionIds,
    legacyWorkspaceIds,
    sessionRefs,
    workspaceRefs,
  };
}

function matchesDeletedSession(
  deleted: DeletedMatchers,
  session: PersistedSessionLike
): boolean {
  if (!session?.id) return false;
  if (deleted.legacySessionIds.has(session.id)) return true;
  if (deleted.legacyWorkspaceIds.has(session.workspaceId)) return true;

  return deleted.sessionRefs.some((ref) => {
    if (ref.sessionId !== session.id) return false;
    return !ref.workspaceId || ref.workspaceId === session.workspaceId;
  });
}

function matchesDeletedWorkspace(
  deleted: DeletedMatchers,
  workspace: PersistedWorkspaceLike
): boolean {
  if (!workspace?.id) return false;
  if (deleted.legacyWorkspaceIds.has(workspace.id)) return true;

  return deleted.workspaceRefs.some((ref) => {
    if (ref.workspaceId !== workspace.id) return false;
    const deletedPath = normalizePath(ref.path);
    return !deletedPath || deletedPath === normalizePath(workspace.path);
  });
}

/// 清掉体积大且可重新计算的字段。
///
/// diffFiles 是真正的元凶：单个 session 的 diff 可以达到几百 KB，
/// 而 localStorage 按 UTF-16 计量，几个 session 就能顶满 ~5MB 配额，
/// 之后任何一次写入都会抛 QuotaExceededError 并打断当次交互。
///
/// sessionStore 的 partialize 已经不再写出这些字段，但历史遗留的文件
/// 里仍然存着，合并时若原样搬运就会把它们重新注入 localStorage。
/// 这里按 partialize 的形状归一化（保留空数组而不是删除键），
/// 避免消费方遇到 undefined。
function sanitizePersistedSession<T extends PersistedSessionLike>(session: T): T {
  return {
    ...session,
    diffFiles: [],
    output: [],
    pid: undefined,
  } as T;
}

function mergeSessionValue(
  fileValue: string | null,
  localValue: string | null,
  deletedState: DeletedUiState
): string | null {
  const fileState = parseJson<PersistedSessionsState>(fileValue);
  const localState = parseJson<PersistedSessionsState>(localValue);
  if (!fileState && !localState) {
    return localValue ?? fileValue ?? null;
  }

  const deleted = buildDeletedMatchers(deletedState);
  const shouldKeepSession = (session: PersistedSessionLike) =>
    !!session?.id && !matchesDeletedSession(deleted, session);

  const localSessions = (localState?.state?.sessions ?? [])
    .filter(shouldKeepSession)
    .map(sanitizePersistedSession);
  const fileSessions = (fileState?.state?.sessions ?? [])
    .filter(shouldKeepSession)
    .map(sanitizePersistedSession);
  const mergedSessions = [...localSessions];
  const existingIds = new Set(localSessions.map((session) => session.id));

  fileSessions.forEach((session) => {
    if (existingIds.has(session.id)) return;
    existingIds.add(session.id);
    mergedSessions.push(session);
  });

  const validIds = new Set(mergedSessions.map((session) => session.id));
  const workspaceIds = uniqueStrings([
    ...mergedSessions.map((session) => session.workspaceId),
    ...Object.keys(localState?.state?.sessionOrderByWorkspace ?? {}),
    ...Object.keys(fileState?.state?.sessionOrderByWorkspace ?? {}),
  ]);

  const sessionOrderByWorkspace = workspaceIds.reduce<Record<string, string[]>>((acc, workspaceId) => {
    const preferred = (localState?.state?.sessionOrderByWorkspace?.[workspaceId] ?? [])
      .filter((id) => validIds.has(id));
    const fallback = (fileState?.state?.sessionOrderByWorkspace?.[workspaceId] ?? [])
      .filter((id) => validIds.has(id));
    const owned = mergedSessions
      .filter((session) => session.workspaceId === workspaceId)
      .sort((a, b) => (a.createdAt ?? 0) - (b.createdAt ?? 0))
      .map((session) => session.id);
    const mergedOrder = uniqueStrings([...preferred, ...fallback, ...owned]);

    if (mergedOrder.length > 0) {
      acc[workspaceId] = mergedOrder;
    }
    return acc;
  }, {});

  const activeSessionId = [localState?.state?.activeSessionId, fileState?.state?.activeSessionId]
    .find((id): id is string => !!id && validIds.has(id))
    ?? mergedSessions[0]?.id
    ?? null;

  return JSON.stringify({
    state: {
      sessions: mergedSessions,
      activeSessionId,
      sessionOrderByWorkspace,
    },
    version: localState?.version ?? fileState?.version ?? 0,
  });
}

function mergeWorkspaceValue(
  fileValue: string | null,
  localValue: string | null,
  deletedState: DeletedUiState
): string | null {
  const fileState = parseJson<PersistedWorkspacesState>(fileValue);
  const localState = parseJson<PersistedWorkspacesState>(localValue);
  if (!fileState && !localState) {
    return localValue ?? fileValue ?? null;
  }

  const deleted = buildDeletedMatchers(deletedState);
  const shouldKeepWorkspace = (workspace: PersistedWorkspaceLike) =>
    !!workspace?.id && !matchesDeletedWorkspace(deleted, workspace);

  const sortByOrder = <T extends { order?: number }>(items: T[]) =>
    [...items].sort((a, b) => (a.order ?? Number.MAX_SAFE_INTEGER) - (b.order ?? Number.MAX_SAFE_INTEGER));

  const localWorkspaces = sortByOrder((localState?.state?.workspaces ?? []).filter(shouldKeepWorkspace));
  const fileWorkspaces = sortByOrder((fileState?.state?.workspaces ?? []).filter(shouldKeepWorkspace));
  const mergedWorkspaces = [...localWorkspaces];
  const existingIds = new Set(localWorkspaces.map((workspace) => workspace.id));

  fileWorkspaces.forEach((workspace) => {
    if (existingIds.has(workspace.id)) return;
    existingIds.add(workspace.id);
    mergedWorkspaces.push(workspace);
  });

  const normalizedWorkspaces = mergedWorkspaces.map((workspace, index) => ({
    ...workspace,
    order: index,
  }));
  const validIds = new Set(normalizedWorkspaces.map((workspace) => workspace.id));
  const activeWorkspaceId = [localState?.state?.activeWorkspaceId, fileState?.state?.activeWorkspaceId]
    .find((id): id is string => !!id && validIds.has(id))
    ?? normalizedWorkspaces[0]?.id
    ?? null;

  return JSON.stringify({
    state: {
      workspaces: normalizedWorkspaces,
      activeWorkspaceId,
    },
    version: localState?.version ?? fileState?.version ?? 0,
  });
}

function mergePersistedValue(
  key: PersistKey,
  fileValue: string | null,
  localValue: string | null,
  deletedState: DeletedUiState
): string | null {
  switch (key) {
    case "code-bar-sessions":
      return mergeSessionValue(fileValue, localValue, deletedState);
    case "code-bar-workspaces":
      return mergeWorkspaceValue(fileValue, localValue, deletedState);
    case "code-bar-settings":
      return localValue ?? fileValue ?? null;
    default:
      return localValue ?? fileValue ?? null;
  }
}

export async function bootstrapPersistState(): Promise<void> {
  if (typeof window === "undefined" || !("localStorage" in window)) return;

  if (isTauriRuntime()) {
    try {
      const { homeDir } = await import("@tauri-apps/api/path");
      cachedHomeDir = (await homeDir()).replace(/[\\/]+$/, "");
    } catch {
      cachedHomeDir = "";
    }
  }

  const fromFile = await invokeSafe<Record<string, string | null>>("load_ui_states", {
    keys: [...PERSIST_KEYS],
  });
  const deletedState = (await invokeSafe<DeletedUiState>("load_deleted_ui_state", {})) ?? {};

  for (const key of PERSIST_KEYS) {
    const fileValue = fromFile?.[key] ?? null;
    const localValue = window.localStorage.getItem(key);
    const mergedValue = mergePersistedValue(key, fileValue, localValue, deletedState);

    if (mergedValue !== null) {
      if (window.localStorage.getItem(key) !== mergedValue) {
        safeLocalSet(key, mergedValue);
      }
      if (fileValue !== mergedValue) {
        void invokeSafe("save_ui_state", { key, value: mergedValue });
      }
      continue;
    }

    if (localValue !== null) {
      void invokeSafe("save_ui_state", { key, value: localValue });
    }
  }
}

// zustand 在每一次 store 变更时都会调用 setItem。拖拽分栏、输入等场景下
// 这相当于每帧一次磁盘写 + 一次 IPC。合并成尾部单次写入，
// localStorage 仍然同步写，因此读取路径不受影响。
const UI_STATE_WRITE_DELAY = 400;
const pendingUiStateWrites = new Map<string, string>();
let uiStateFlushTimer: ReturnType<typeof setTimeout> | null = null;

function flushUiStateWrites(): void {
  if (uiStateFlushTimer) {
    clearTimeout(uiStateFlushTimer);
    uiStateFlushTimer = null;
  }
  if (pendingUiStateWrites.size === 0) return;

  const entries = [...pendingUiStateWrites.entries()];
  pendingUiStateWrites.clear();
  for (const [key, value] of entries) {
    void invokeSafe("save_ui_state", { key, value });
  }
}

function scheduleUiStateWrite(key: string, value: string): void {
  pendingUiStateWrites.set(key, value);
  if (uiStateFlushTimer) return;
  uiStateFlushTimer = setTimeout(flushUiStateWrites, UI_STATE_WRITE_DELAY);
}

if (typeof window !== "undefined") {
  // 退出前落盘，避免丢掉最后一次挂起的写入
  window.addEventListener("pagehide", flushUiStateWrites);
  window.addEventListener("beforeunload", flushUiStateWrites);
}

/// 写入 localStorage，配额不足时降级而不是抛异常。
///
/// zustand 的 persist 中间件在 store 更新过程中同步调用 setItem，
/// 因此一次 QuotaExceededError 会穿透到 React 事件处理里，
/// 表现为点击切换 session 直接报未捕获错误。
/// 文件镜像才是权威副本，丢掉这次缓存写入是安全的。
function safeLocalSet(key: string, value: string): boolean {
  try {
    window.localStorage.setItem(key, value);
    return true;
  } catch (error) {
    console.warn(`[persist] localStorage 写入失败，改用文件镜像: ${key}`, error);
    // 残留的超大旧值会一直占着配额，导致后续写入全部失败。
    // 此时文件写入已经排好队，移除缓存键可以自愈；
    // 下次启动会从文件重新填充。
    try {
      window.localStorage.removeItem(key);
    } catch {
      // 忽略：已经处于降级状态
    }
    return false;
  }
}

export const mirroredPersistStorage: StateStorage = {
  getItem: (name) => {
    if (typeof window === "undefined" || !("localStorage" in window)) return null;
    return window.localStorage.getItem(name);
  },

  setItem: (name, value) => {
    if (typeof window === "undefined" || !("localStorage" in window)) return;

    // 先排队文件写入：它是权威副本且不受配额限制。
    // 顺序很重要——safeLocalSet 在失败时会移除缓存键。
    scheduleUiStateWrite(name, value);
    safeLocalSet(name, value);
  },

  removeItem: (name) => {
    if (typeof window === "undefined" || !("localStorage" in window)) return;

    window.localStorage.removeItem(name);
    pendingUiStateWrites.delete(name);
    void invokeSafe("remove_ui_state", { key: name });
  },
};
