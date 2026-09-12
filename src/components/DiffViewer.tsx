import { memo, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, ChevronRight, FileCode2, FilePlus2, FileX2, Minus, Plus } from "lucide-react";
import { useAppI18n } from "../i18n";
import { DiffFile, DiffHunk, DiffLine, useSessionStore } from "../store/sessionStore";
import { type ScmActionMode } from "../store/scmStore";
import { WorkbenchTooltip } from "./ui/WorkbenchTooltip";

const MONO = "'JetBrains Mono', 'Fira Code', 'SF Mono', monospace";

type LineStyle = { bg: string; text: string; gutter: string; prefix: string };
const LINE_STYLES: Record<DiffLine["type"], LineStyle> = {
  added:   {
    bg:     "var(--ci-added-bg)",
    text:   "var(--ci-added-text)",
    gutter: "rgba(52,199,89,0.06)",
    prefix: "var(--ci-green)",
  },
  deleted: {
    bg:     "var(--ci-deleted-bg)",
    text:   "var(--ci-deleted-text)",
    gutter: "rgba(255,59,48,0.05)",
    prefix: "var(--ci-red)",
  },
  context: {
    bg:     "transparent",
    text:   "var(--ci-text-muted)",
    gutter: "transparent",
    prefix: "transparent",
  },
};

/// 一个大 diff 会渲染几千个这样的行。
/// 没有 memo 时，任何一次祖先重渲染（状态翻转、diff 刷新）都会重建全部行。
/// line 对象来自 store 且不会原地修改，因此按引用比较是安全的。
const DiffLineRow = memo(function DiffLineRow({ line }: { line: DiffLine }) {
  const c = LINE_STYLES[line.type];
  const prefix = line.type === "added" ? "+" : line.type === "deleted" ? "−" : " ";
  return (
    <div style={{
      display: "flex",
      width: "max-content",
      minWidth: "100%",
      fontFamily: MONO,
      fontSize: 11,
      lineHeight: "18px",
      background: c.bg,
    }}>
      <span style={{
        width: 36,
        textAlign: "right",
        padding: "0 6px",
        color: "var(--ci-text-dim)",
        flexShrink: 0,
        background: c.gutter,
        userSelect: "none",
        borderInlineEnd: "1px solid var(--ci-toolbar-border)",
      }}>
        {line.oldLineNo ?? ""}
      </span>
      <span style={{
        width: 36,
        textAlign: "right",
        padding: "0 6px",
        color: "var(--ci-text-dim)",
        flexShrink: 0,
        background: c.gutter,
        userSelect: "none",
        borderInlineEnd: "1px solid var(--ci-toolbar-border)",
      }}>
        {line.newLineNo ?? ""}
      </span>
      <span style={{
        width: 18,
        textAlign: "center",
        color: line.type === "context" ? "transparent" : c.prefix,
        flexShrink: 0,
        userSelect: "none",
        fontWeight: 600,
      }}>
        {prefix}
      </span>
      <span style={{ flex: 1, padding: "0 10px", color: c.text, whiteSpace: "pre" }}>
        {line.content || " "}
      </span>
    </div>
  );
});

function FileIcon({ type, binary }: { type: DiffFile["type"]; binary?: boolean }) {
  if (binary) return <FileCode2 size={12} strokeWidth={1.8} />;
  if (type === "added") return <FilePlus2 size={12} strokeWidth={1.8} />;
  if (type === "deleted") return <FileX2 size={12} strokeWidth={1.8} />;
  return <FileCode2 size={12} strokeWidth={1.8} />;
}

function FileStat({ additions, deletions }: { additions: number; deletions: number }) {
  return (
    <span style={{ display: "flex", gap: 8, fontSize: 10, marginInlineStart: "auto", flexShrink: 0 }}>
      {additions > 0 && <span style={{ color: "var(--ci-added-text)" }}>+{additions}</span>}
      {deletions > 0 && <span style={{ color: "var(--ci-deleted-text)" }}>−{deletions}</span>}
    </span>
  );
}

function HunkActionButton({ label, icon, onClick, disabled }: { label: string; icon: React.ReactNode; onClick: () => void; disabled?: boolean }) {
  return (
    <WorkbenchTooltip label={label}>
      <button
        onClick={onClick}
        disabled={disabled}
        style={{
          background: "none",
          border: "none",
          color: disabled ? "var(--ci-text-dim)" : "var(--ci-text-dim)",
          width: 18,
          height: 18,
          padding: 0,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          cursor: disabled ? "default" : "pointer",
          opacity: disabled ? 0.35 : 0.82,
        }}
      >
        {icon}
      </button>
    </WorkbenchTooltip>
  );
}

function DiffFileRow({
  file,
  sessionId,
  defaultOpen = false,
  fileMode,
  onStageHunk,
  onUnstageHunk,
  onDiscardHunk,
  busy,
  contentMaxHeight,
}: {
  file: DiffFile;
  sessionId?: string;
  defaultOpen?: boolean;
  fileMode?: ScmActionMode | null;
  onStageHunk?: (path: string, hunkIndex: number) => void;
  onUnstageHunk?: (path: string, hunkIndex: number) => void;
  onDiscardHunk?: (path: string, hunkIndex: number) => void;
  busy?: boolean;
  contentMaxHeight?: number | string;
}) {
  const { t } = useAppI18n();
  // 多文件列表默认折叠：之前每个文件都展开，一次 diff 刷新就把所有文件的
  // 每一行都变成 DOM 节点，而且没有虚拟化。单文件视图仍然直接展开。
  const [isOpen, setIsOpen] = useState(defaultOpen);
  const [fetchedHunks, setFetchedHunks] = useState<DiffHunk[] | null>(null);
  const [fetchedNote, setFetchedNote] = useState<string | null>(null);
  const isBinary = !!file.binary;
  const useInnerScroll = contentMaxHeight !== "none";

  // 列表刷新只带回文件摘要，hunks 在首次展开时按需拉取。
  useEffect(() => {
    if (!isOpen || isBinary || !sessionId) return;
    if (file.hunks.length > 0 || fetchedHunks !== null) return;

    const session = useSessionStore.getState().sessions.find((s) => s.id === sessionId);
    if (!session) return;

    let cancelled = false;
    void invoke<{ hunks: DiffHunk[] | null; note: string | null }>("get_file_diff_hunks", {
      workdir: session.workdir,
      path: file.path,
      baseBranch: session.baseBranch ?? null,
    })
      .then((result) => {
        if (cancelled) return;
        setFetchedHunks(result.hunks ?? []);
        setFetchedNote(result.note ?? null);
      })
      .catch(() => {
        if (!cancelled) setFetchedHunks([]);
      });

    return () => {
      cancelled = true;
    };
  }, [isOpen, isBinary, sessionId, file.path, file.hunks.length, fetchedHunks]);

  const hunks = file.hunks.length > 0 ? file.hunks : fetchedHunks ?? [];
  const note = file.note ?? fetchedNote;
  const loadingHunks =
    isOpen && !isBinary && !!sessionId && file.hunks.length === 0 && fetchedHunks === null;

  return (
    <div style={{ borderBottom: "1px solid var(--ci-toolbar-border)", background: "transparent" }}>
      <button
        onClick={() => setIsOpen((v) => !v)}
        style={{
          width: "100%",
          display: "flex",
          alignItems: "center",
          gap: 8,
          padding: "6px 12px",
          background: "transparent",
          border: "none",
          cursor: "pointer",
          color: "var(--ci-text)",
          textAlign: "left",
        }}
      >
        <span style={{ width: 12, display: "flex", alignItems: "center", justifyContent: "center", color: "var(--ci-text-dim)", flexShrink: 0 }}>
          {isOpen ? <ChevronDown size={12} strokeWidth={1.8} /> : <ChevronRight size={12} strokeWidth={1.8} />}
        </span>
        <span style={{ width: 12, display: "flex", alignItems: "center", justifyContent: "center", color: file.type === "added" ? "var(--ci-green)" : file.type === "deleted" ? "var(--ci-red)" : "var(--ci-text-dim)", flexShrink: 0 }}>
          <FileIcon type={file.type} binary={isBinary} />
        </span>
        <span style={{ fontSize: 11, fontFamily: MONO, flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
          {file.path}
        </span>
        {isBinary ? (
          <span style={{ fontSize: 10, color: "var(--ci-purple)" }}>{t("diff.binary")}</span>
        ) : (
          <FileStat additions={file.additions} deletions={file.deletions} />
        )}
      </button>

      {isOpen && (
        <div style={{ background: "var(--ci-code-bg)", borderTop: "1px solid var(--ci-toolbar-border)", maxHeight: contentMaxHeight, overflow: useInnerScroll ? "auto" : "visible" }}>
          {isBinary ? (
            <div style={{ padding: "14px 16px", fontSize: 11, color: "var(--ci-text-dim)" }}>
              {t("diff.binaryPreviewUnsupported")}
            </div>
          ) : loadingHunks ? (
            <div style={{ padding: "12px 16px", fontSize: 11, color: "var(--ci-text-dim)", fontFamily: MONO }}>
              …
            </div>
          ) : hunks.length === 0 ? (
            <div style={{ padding: "12px 16px", fontSize: 11, color: "var(--ci-text-muted)", fontFamily: MONO }}>
              {note ?? t("diff.noContentDiff")}
            </div>
          ) : (
            hunks.map((hunk, hi) => (
              <div key={hi}>
                <div style={{
                  padding: "2px 8px 2px 90px",
                  background: "var(--ci-toolbar-bg)",
                  color: "var(--ci-text-dim)",
                  fontSize: 10,
                  fontFamily: MONO,
                  borderTop: "1px solid var(--ci-toolbar-border)",
                  borderBottom: "1px solid var(--ci-toolbar-border)",
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                }}>
                  <span>{hunk.header}</span>
                  <div style={{ marginInlineStart: "auto", display: "flex", gap: 2 }}>
                    {fileMode === "unstaged" && onStageHunk && <HunkActionButton label={t("scm.stageHunk")} icon={<Plus size={12} strokeWidth={1.8} />} onClick={() => onStageHunk(file.path, hi)} disabled={busy} />}
                    {fileMode === "unstaged" && onDiscardHunk && <HunkActionButton label={t("scm.discardHunk")} icon={<Minus size={12} strokeWidth={1.8} />} onClick={() => onDiscardHunk(file.path, hi)} disabled={busy} />}
                    {fileMode === "staged" && onUnstageHunk && <HunkActionButton label={t("scm.unstageHunk")} icon={<Minus size={12} strokeWidth={1.8} />} onClick={() => onUnstageHunk(file.path, hi)} disabled={busy} />}
                  </div>
                </div>
                {hunk.lines.map((line, li) => (
                  <DiffLineRow key={li} line={line} />
                ))}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}

export function DiffViewer({
  files,
  sessionId,
  fileMode,
  onStageHunk,
  onUnstageHunk,
  onDiscardHunk,
  busy = false,
  contentMaxHeight = 420,
}: {
  files: DiffFile[];
  /// 提供它才能在展开时按需拉取 hunks
  sessionId?: string;
  fileMode?: ScmActionMode | null;
  onStageHunk?: (path: string, hunkIndex: number) => void;
  onUnstageHunk?: (path: string, hunkIndex: number) => void;
  onDiscardHunk?: (path: string, hunkIndex: number) => void;
  busy?: boolean;
  contentMaxHeight?: number | string;
}) {
  const { t } = useAppI18n();

  if (files.length === 0) {
    return (
      <div style={{ padding: "20px 0", textAlign: "center", color: "var(--ci-text-muted)", fontSize: 12 }}>
        {t("diff.noChanges")}
      </div>
    );
  }

  const totalAdditions = files.reduce((s, f) => s + f.additions, 0);
  const totalDeletions = files.reduce((s, f) => s + f.deletions, 0);

  return (
    <div style={{ background: "transparent" }}>
      <div style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        padding: "7px 12px",
        borderBottom: "1px solid var(--ci-toolbar-border)",
        background: "var(--ci-toolbar-bg)",
      }}>
        <span style={{ fontSize: 11, color: "var(--ci-text-muted)" }}>
          {t("diff.filesChanged", { count: files.length })}
        </span>
        <span style={{ display: "flex", gap: 8, fontSize: 11 }}>
          <span style={{ color: "var(--ci-added-text)" }}>+{totalAdditions}</span>
          <span style={{ color: "var(--ci-deleted-text)" }}>−{totalDeletions}</span>
        </span>
      </div>
      {files.map((f) => (
        <DiffFileRow
          key={f.path}
          file={f}
          sessionId={sessionId}
          // 单文件视图（SCM 选中某个文件）直接展开；
          // 多文件列表保持折叠，避免一次性铺开成千上万行
          defaultOpen={files.length === 1}
          fileMode={fileMode}
          onStageHunk={onStageHunk}
          onUnstageHunk={onUnstageHunk}
          onDiscardHunk={onDiscardHunk}
          busy={busy}
          contentMaxHeight={contentMaxHeight}
        />
      ))}
    </div>
  );
}
