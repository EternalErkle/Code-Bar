import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { useAppI18n } from "../i18n";
import { useSettingsStore, isGlassTheme, type ThemeMode } from "../store/settingsStore";

interface Props {
  sessionId: string;
  command: string;     // e.g. "claude"
  args?: string[];     // e.g. ["--dangerously-skip-permissions"]
  workdir: string;
  active: boolean;     // 是否可见/激活
  initialPrompt?: string | null;
  supportsPromptArg?: boolean;
  onReady?: () => void; // PTY 进程启动成功后回调（用于透传初始 query）
  onWaiting?: () => void; // CLI 完成任务、等待下一条 query 时回调
  onRunning?: () => void; // CLI 开始处理 query 时回调
  onError?: (error: string) => void; // API 错误中断回调
  onNotification?: (title: string, message: string, notification_type: string) => void; // CLI hook 通知回调
  // 额外注入的环境变量，透传给 start_pty_session（如 CODE_BAR_* context 信息）
  env?: [string, string][];
  // 仅 CLI 主终端在 Windows 下启用 Ctrl+C / Ctrl+V 文本复制粘贴
  enableWindowsCtrlCv?: boolean;
}

// 每个终端保留的回滚行数。隐藏的会话面板不会卸载，缓冲区会一直占着内存。
const TERMINAL_SCROLLBACK = 1500;

// ── xterm 主题定义 ─────────────────────────────────────────────
const TERM_THEME_DARK = {
  background:           "#0a0a0c",
  foreground:           "#e2e8f0",
  cursor:               "#60a5fa",
  cursorAccent:         "#0a0a0c",
  selectionBackground:  "rgba(96,165,250,0.3)",
  black:                "#1e1e2e",
  red:                  "#f87171",
  green:                "#4ade80",
  yellow:               "#fbbf24",
  blue:                 "#60a5fa",
  magenta:              "#c084fc",
  cyan:                 "#34d399",
  white:                "#e2e8f0",
  brightBlack:          "#374151",
  brightRed:            "#fc8181",
  brightGreen:          "#6ee7b7",
  brightYellow:         "#fde68a",
  brightBlue:           "#93c5fd",
  brightMagenta:        "#d8b4fe",
  brightCyan:           "#6ee7b7",
  brightWhite:          "#f1f5f9",
};

// 浅色模式：终端背景改为暖白/米色，前景改为深色，但保留足够对比度
const TERM_THEME_LIGHT = {
  background:           "#1e1e2e",   // 浅色模式也保持深色终端背景（可读性好）
  foreground:           "#e2e8f0",
  cursor:               "#007AFF",
  cursorAccent:         "#1e1e2e",
  selectionBackground:  "rgba(0,122,255,0.25)",
  black:                "#1e1e2e",
  red:                  "#f87171",
  green:                "#4ade80",
  yellow:               "#fbbf24",
  blue:                 "#60a5fa",
  magenta:              "#c084fc",
  cyan:                 "#34d399",
  white:                "#e2e8f0",
  brightBlack:          "#4b5563",
  brightRed:            "#fc8181",
  brightGreen:          "#6ee7b7",
  brightYellow:         "#fde68a",
  brightBlue:           "#93c5fd",
  brightMagenta:        "#d8b4fe",
  brightCyan:           "#6ee7b7",
  brightWhite:          "#f1f5f9",
};

// 根据 settings.theme + 系统媒体查询，计算是否为深色模式
function getIsDark(theme: ThemeMode): boolean {
  if (theme === "dark") return true;
  if (theme === "light") return false;
  if (theme === "glass") return true;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function getTerminalLook(theme: ThemeMode) {
  const isDark = getIsDark(theme);
  return {
    termTheme: isDark ? TERM_THEME_DARK : TERM_THEME_LIGHT,
    termBg: isDark ? "#0a0a0c" : "#1e1e2e",
  };
}

function getTerminalMetrics(fontSize: number) {
  return {
    fontSize,
    lineHeight: 1.4,
  };
}

function getClampedTerminalSize(term: Terminal) {
  return {
    cols: Math.max(term.cols, 20),
    rows: Math.max(term.rows, 5),
  };
}

function writePtyData(sessionId: string, data: string) {
  const bytes = new TextEncoder().encode(data);
  // 分块拼接：整段 spread 在大段粘贴时会超出函数参数上限并抛 RangeError
  const CHUNK = 0x8000;
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + CHUNK));
  }
  return invoke("write_pty", { sessionId, data: btoa(binary) });
}

/// 不可见终端最多缓冲的输出字节数
const MAX_PENDING_BYTES = 4 * 1024 * 1024;

/// 每个会话独立的数据事件名，必须与 Rust 侧 pty_data_event 保持一致。
/// 统一事件名时每条输出都会分发给所有已挂载的终端，开销随会话数放大。
export function ptyDataEvent(sessionId: string) {
  return `pty-data:${sessionId.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
}

/// base64 → 字节。Uint8Array.from(bin, cb) 会为每个字节调用一次回调，
/// 在满屏重绘的数据量下这是渲染进程里最热的一段代码。
export function decodePtyChunk(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function wheelDeltaToLines(event: WheelEvent, rows: number): number {
  if (event.deltaY === 0) return 0;

  if (event.deltaMode === WheelEvent.DOM_DELTA_LINE) {
    return event.deltaY > 0 ? Math.ceil(event.deltaY) : Math.floor(event.deltaY);
  }

  if (event.deltaMode === WheelEvent.DOM_DELTA_PAGE) {
    return event.deltaY > 0 ? rows : -rows;
  }

  const lines = event.deltaY / 32;
  if (lines > 0) return Math.max(1, Math.round(lines));
  return Math.min(-1, Math.round(lines));
}

export function PtyTerminal({
  sessionId,
  command,
  args = [],
  workdir,
  active,
  initialPrompt,
  supportsPromptArg = false,
  onReady,
  onWaiting,
  onRunning,
  onError,
  onNotification,
  env,
  enableWindowsCtrlCv = false,
}: Props) {
  const { t } = useAppI18n();
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const startedRef = useRef(false);
  const startingRef = useRef(false);
  const launchTokenRef = useRef(0);
  // 不可见时缓冲输出，激活时一次性回放：
  // 隐藏的标签页过去照样解析并重绘每一个字节。
  const pendingWriteRef = useRef<Uint8Array[]>([]);
  const pendingBytesRef = useRef(0);
  const webglRef = useRef<WebglAddon | null>(null);
  const [exited, setExited] = useState(false);

  // 读取当前主题
  const theme = useSettingsStore((s) => s.settings.theme);
  const ptyFontSize = useSettingsStore((s) => s.settings.ptyFontSize);

  // ── 初始化 xterm ──────────────────────────────────────────
  useEffect(() => {
    if (!containerRef.current) return;
    const container = containerRef.current;

    const { termTheme, termBg } = getTerminalLook(theme);
    const { fontSize, lineHeight } = getTerminalMetrics(ptyFontSize);
    const isWindows = navigator.userAgent.toLowerCase().includes("windows");

    const term = new Terminal({
      theme: termTheme,
      fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
      fontSize,
      lineHeight,
      cursorBlink: true,
      cursorStyle: "bar",
      // 会话面板从不卸载，所以每个隐藏终端的回滚缓冲区会一直驻留。
      // 5000 行在终端满载时约 7MB/个，十个终端就是几十 MB。
      scrollback: TERMINAL_SCROLLBACK,
      allowTransparency: false,
      convertEol: true,
    });

    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);

    // WebGL 渲染器不在这里加载：每个实例会占一个 GPU 上下文，
    // 而浏览器上限约 16 个，SplitSwapLayout 又会让所有标签页常驻挂载。
    // 改为首次变为可见时再加载（见下方 active effect）。

    fit.fit();

    termRef.current = term;
    fitRef.current = fit;

    // 更新容器背景色
    container.style.background = termBg;

    const wheelHandler = (event: WheelEvent) => {
      const termInstance = termRef.current;
      if (!termInstance || event.ctrlKey) return;

      const lines = wheelDeltaToLines(event, termInstance.rows || 24);
      if (lines === 0) return;

      event.preventDefault();
      event.stopPropagation();
      termInstance.scrollLines(lines);
      termInstance.focus();
    };

    container.addEventListener("wheel", wheelHandler, { passive: false });

    if (isWindows && enableWindowsCtrlCv) {
      term.attachCustomKeyEventHandler((event: KeyboardEvent) => {
        if (event.type !== "keydown" || !event.ctrlKey || event.altKey || event.metaKey) {
          return true;
        }

        const key = event.key.toLowerCase();
        if (key === "c") {
          if (!term.hasSelection()) return true;
          event.preventDefault();
          event.stopPropagation();
          void navigator.clipboard.writeText(term.getSelection()).catch(() => {});
          return false;
        }

        if (key === "v") {
          event.preventDefault();
          event.stopPropagation();
          void navigator.clipboard.readText()
            .then((text) => {
              if (!text) return;
              return writePtyData(sessionId, text);
            })
            .catch(() => {});
          return false;
        }

        return true;
      });
    }

    // 键盘输入 → 发给 PTY（base64 编码）
    term.onData((data: string) => {
      writePtyData(sessionId, data).catch(() => {});
    });

    return () => {
      container.removeEventListener("wheel", wheelHandler);
      // term.dispose() 会连带释放 addon，这里只清掉引用
      webglRef.current = null;
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // runner/workdir 切换会通过 key 触发卸载；这里顺手停掉旧 PTY，避免旧 CLI 抢到下一条输入。
  useEffect(() => {
    return () => {
      if (!startedRef.current && !startingRef.current) return;
      startingRef.current = false;
      launchTokenRef.current += 1;
      if (startedRef.current) {
        invoke("stop_pty_session", { sessionId }).catch(() => {});
      }
    };
  }, [sessionId]);

  // ── 主题切换时动态更新 xterm 颜色 ────────────────────────
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;

    const { termTheme, termBg } = getTerminalLook(theme);
    const { fontSize, lineHeight } = getTerminalMetrics(ptyFontSize);

    // xterm 5.x 支持直接更新 options.theme
    term.options.theme = termTheme;
    term.options.fontSize = fontSize;
    term.options.lineHeight = lineHeight;

    if (containerRef.current) {
      containerRef.current.style.background = termBg;
    }

    requestAnimationFrame(() => {
      fitRef.current?.fit();
    });

    // system 模式：监听系统颜色变化
    if (theme === "system") {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      const listener = (e: MediaQueryListEvent) => {
        const t = termRef.current;
        if (!t) return;
        t.options.theme = e.matches ? TERM_THEME_DARK : TERM_THEME_LIGHT;
        t.options.fontSize = ptyFontSize;
        t.options.lineHeight = 1.4;
        const bg = e.matches ? "#0a0a0c" : "#1e1e2e";
        if (containerRef.current) containerRef.current.style.background = bg;
        requestAnimationFrame(() => {
          fitRef.current?.fit();
        });
      };
      mq.addEventListener("change", listener);
      return () => mq.removeEventListener("change", listener);
    }
  }, [ptyFontSize, theme]);

  // 用 ref 保存最新回调，避免闭包过时（不加入依赖数组）
  const onWaitingRef = useRef(onWaiting);
  onWaitingRef.current = onWaiting;
  const onRunningRef = useRef(onRunning);
  onRunningRef.current = onRunning;
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const onNotificationRef = useRef(onNotification);
  onNotificationRef.current = onNotification;

  // ── 监听 PTY 数据事件 ─────────────────────────────────────
  useEffect(() => {
    const u1 = listen<{ session_id: string; data: string }>(
      ptyDataEvent(sessionId),
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        const term = termRef.current;
        if (!term) return;
        try {
          const bytes = decodePtyChunk(payload.data);
          if (activeRef.current) {
            term.write(bytes);
            return;
          }
          // 面板不可见：先攒着，激活时回放。上限保护内存，
          // 超出后丢弃最旧的数据（CLI 的整屏重绘会覆盖掉丢失的部分）。
          pendingWriteRef.current.push(bytes);
          pendingBytesRef.current += bytes.length;
          while (pendingBytesRef.current > MAX_PENDING_BYTES && pendingWriteRef.current.length > 1) {
            const dropped = pendingWriteRef.current.shift();
            pendingBytesRef.current -= dropped?.length ?? 0;
          }
        } catch {}
      }
    );

    const u2 = listen<{ session_id: string }>(
      "pty-exit",
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        termRef.current?.writeln("\r\n\x1b[90m─────────────────────────────────────\x1b[0m");
        termRef.current?.writeln(`\x1b[90m${t("pty.processExited")}\x1b[0m`);
        setExited(true);
        startedRef.current = false; // 允许重启
        startingRef.current = false;
        launchTokenRef.current += 1;
      }
    );

    // CLI 完成任务，等待下一条 query（检测到 "? for shortcuts"）
    const u3 = listen<{ session_id: string }>(
      "pty-waiting",
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        onWaitingRef.current?.();
      }
    );

    // CLI 开始处理 query（检测到 "esc to interrupt"）
    const u4 = listen<{ session_id: string }>(
      "pty-running",
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        onRunningRef.current?.();
      }
    );

    // API 错误中断（如 Claude StopFailure hook）
    const u5 = listen<{ session_id: string; error: string }>(
      "pty-error",
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        onErrorRef.current?.(payload.error);
      }
    );

    // CLI hook: Notification（当前主要来自 Claude，需要用户确认/输入）
    const u6 = listen<{ session_id: string; title: string; message: string; notification_type: string }>(
      "pty-notification",
      ({ payload }) => {
        if (payload.session_id !== sessionId) return;
        onNotificationRef.current?.(payload.title, payload.message, payload.notification_type);
      }
    );

    return () => {
      u1.then((f) => f()).catch(() => {});
      u2.then((f) => f()).catch(() => {});
      u3.then((f) => f()).catch(() => {});
      u4.then((f) => f()).catch(() => {});
      u5.then((f) => f()).catch(() => {});
      u6.then((f) => f()).catch(() => {});
    };
  }, [sessionId]);

  // ── 启动 PTY 进程（仅第一次，之后常驻直到 exit）────────
  // 用 ref 保存最新的 args/onReady/env，避免加入依赖导致每次渲染重启
  const argsRef = useRef(args);
  argsRef.current = args;
  const initialPromptRef = useRef(initialPrompt);
  initialPromptRef.current = initialPrompt;
  const supportsPromptArgRef = useRef(supportsPromptArg);
  supportsPromptArgRef.current = supportsPromptArg;
  const onReadyRef = useRef(onReady);
  onReadyRef.current = onReady;
  const envRef = useRef(env);
  envRef.current = env;

  const buildLaunchArgs = () => {
    const launchArgs = [...argsRef.current];
    const prompt = initialPromptRef.current?.trim();
    if (supportsPromptArgRef.current && prompt) {
      launchArgs.push(prompt);
    }
    return launchArgs;
  };

  useEffect(() => {
    if (!active || startedRef.current || startingRef.current) return;
    startingRef.current = true;
    setExited(false);
    const launchToken = launchTokenRef.current + 1;
    launchTokenRef.current = launchToken;

    // 延迟 250ms：等 resize_popup_full 动画完成，容器达到目标尺寸
    const timer = setTimeout(() => {
      if (launchTokenRef.current !== launchToken) return;
      const fit = fitRef.current;
      const term = termRef.current;
      if (fit) fit.fit();
      const cols = Math.max(term?.cols ?? 80, 40);
      const rows = Math.max(term?.rows ?? 24, 12);
      const launchArgs = buildLaunchArgs();
      const prompt = initialPromptRef.current?.trim();

      // 打印启动提示，方便调试确认实际启动的命令
      const displayCmd = prompt && supportsPromptArgRef.current
        ? [command, ...argsRef.current, "<prompt>"].join(" ")
        : [command, ...launchArgs].join(" ");
      term?.writeln(`\x1b[90m$ ${displayCmd}\x1b[0m`);

      invoke("start_pty_session", {
        sessionId,
        workdir,
        command,
        args: launchArgs,
        cols,
        rows,
        env: envRef.current ?? null,
      })
        .then(() => {
          if (launchTokenRef.current !== launchToken) return;
          startedRef.current = true;
          startingRef.current = false;
          // spawn 返回 = CLI 进程已启动（resolve_command_path 保证是完整路径直接 spawn）
          onReadyRef.current?.();
        })
        .catch((e) => {
          if (launchTokenRef.current !== launchToken) return;
          startingRef.current = false;
          startedRef.current = false;
          termRef.current?.writeln(`\x1b[31m${t("session.installFailed", { error: String(e) })}\x1b[0m`);
        });
    }, 250);

    return () => {
      clearTimeout(timer);
      if (launchTokenRef.current === launchToken && !startedRef.current) {
        startingRef.current = false;
      }
    };
  }, [active, sessionId, workdir, command]);

  // ── 重新启动（退出后用户点击重启）───────────────────────
  const handleRestart = () => {
    setExited(false);
    startedRef.current = false;
    startingRef.current = false;
    launchTokenRef.current += 1;
    termRef.current?.clear();

    const fit = fitRef.current;
    const term = termRef.current;
    if (fit) fit.fit();
    const cols = Math.max(term?.cols ?? 80, 40);
    const rows = Math.max(term?.rows ?? 24, 12);
    startingRef.current = true;
    const launchToken = launchTokenRef.current + 1;
    launchTokenRef.current = launchToken;

    const displayCmd = [command, ...argsRef.current].join(" ");
    term?.writeln(`\x1b[90m$ ${displayCmd}\x1b[0m`);

    invoke("start_pty_session", {
      sessionId,
      workdir,
      command,
      args: argsRef.current,
      cols,
      rows,
      env: envRef.current ?? null,
    })
      .then(() => {
        if (launchTokenRef.current !== launchToken) return;
        startedRef.current = true;
        startingRef.current = false;
        onReadyRef.current?.();
      })
      .catch((e) => {
        if (launchTokenRef.current !== launchToken) return;
        startedRef.current = false;
        startingRef.current = false;
        termRef.current?.writeln(`\x1b[31m${t("session.installFailed", { error: String(e) })}\x1b[0m`);
      });
  };

  // ── 可见时 fit + focus（重新展开时恢复焦点，不重启 PTY）──
  useEffect(() => {
    if (!active) {
      // 光标闪烁是一个常驻定时器，会让不可见的终端持续触发重绘
      const hidden = termRef.current;
      if (hidden) hidden.options.cursorBlink = false;
      return;
    }

    const term = termRef.current;
    if (term) {
      term.options.cursorBlink = true;

      // 首次可见时才创建 GPU 上下文。浏览器的 WebGL 上下文上限约 16 个，
      // 超出后最早的会被强制丢弃，终端会变黑。
      if (!webglRef.current) {
        try {
          const webgl = new WebglAddon();
          webgl.onContextLoss(() => {
            webgl.dispose();
            webglRef.current = null;
          });
          term.loadAddon(webgl);
          webglRef.current = webgl;
        } catch {
          // 没有可用 GPU：继续使用 DOM 渲染
        }
      }
    }

    // 回放隐藏期间缓冲的输出
    if (term && pendingWriteRef.current.length > 0) {
      const pending = pendingWriteRef.current;
      pendingWriteRef.current = [];
      pendingBytesRef.current = 0;
      for (const chunk of pending) {
        term.write(chunk);
      }
    }

    const t = setTimeout(() => {
      fitRef.current?.fit();
      const activeTerm = termRef.current;
      activeTerm?.focus();
      if (!activeTerm) return;
      invoke("resize_pty", { sessionId, ...getClampedTerminalSize(activeTerm) }).catch(() => {});
    }, 80);
    return () => clearTimeout(t);
  }, [active, sessionId]);

  // active ref：供 ResizeObserver 回调访问（避免闭包过时）
  const activeRef = useRef(active);
  activeRef.current = active;

  // ── ResizeObserver：自动 fit + 同步 PTY 大小给 Rust ─────
  // 只有 active=true（面板可见）时才向 Rust 同步尺寸，
  // 避免面板收起时窗口缩小导致 resize_pty 传入极小值使进程崩溃
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    // 防抖：拖拽窗口/分栏时 ResizeObserver 每帧都会触发，
    // 而 fit() 要重算全部字符尺寸，resize_pty 还要走一次 IPC。
    let timer: ReturnType<typeof setTimeout> | null = null;
    const ro = new ResizeObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        const fit = fitRef.current;
        const term = termRef.current;
        if (!fit || !term) return;
        fit.fit();
        // 仅在面板可见时同步给 Rust，防止收起时 cols/rows 为 0 导致进程崩溃
        if (!activeRef.current) return;
        invoke("resize_pty", { sessionId, ...getClampedTerminalSize(term) }).catch(() => {});
      }, 60);
    });
    ro.observe(el);
    return () => {
      if (timer) clearTimeout(timer);
      ro.disconnect();
    };
  }, [sessionId]);

  const isGlass = isGlassTheme(theme);
  const { termBg } = getTerminalLook(theme);

  return (
    <div
      className="ci-pty-terminal"
      style={{
        position: "relative",
        width: "100%",
        height: "100%",
        overflow: "hidden",
        background: termBg,
      }}
    >
      {/* xterm canvas */}
      <div
        ref={containerRef}
        style={{ width: "100%", height: "100%", background: termBg }}
      />

      {/* 退出后的重启覆盖层 */}
      {exited && (
        <div style={{
          position: "absolute", bottom: 0, left: 0, right: 0,
          padding: "12px 16px",
          background: `linear-gradient(to top, ${termBg} 70%, transparent)`,
          display: "flex", alignItems: "center", justifyContent: "center", gap: 12,
        }}>
          <span style={{
            fontSize: 11,
            color: isGlass ? "var(--ci-text-dim)" : "rgba(255,255,255,0.35)",
            fontFamily: "monospace",
          }}>
            {t("pty.sessionEnded")}
          </span>
          <button
            onClick={handleRestart}
            style={{
              display: "flex", alignItems: "center", gap: 6,
              padding: "5px 14px", borderRadius: 8,
              border: isGlass ? "1px solid var(--ci-accent-bdr)" : "1px solid rgba(96,165,250,0.35)",
              background: isGlass ? "var(--ci-accent-bg)" : "rgba(96,165,250,0.1)",
              color: isGlass ? "var(--ci-accent)" : "#60a5fa", fontSize: 12, fontWeight: 600,
              cursor: "pointer", transition: "background 0.15s, border-color 0.15s",
            }}
            onMouseEnter={e => {
              e.currentTarget.style.background = isGlass ? "rgba(63,145,255,0.16)" : "rgba(96,165,250,0.2)";
              e.currentTarget.style.borderColor = isGlass ? "rgba(96,175,255,0.26)" : "rgba(96,165,250,0.6)";
            }}
            onMouseLeave={e => {
              e.currentTarget.style.background = isGlass ? "var(--ci-accent-bg)" : "rgba(96,165,250,0.1)";
              e.currentTarget.style.borderColor = isGlass ? "rgba(96,175,255,0.20)" : "rgba(96,165,250,0.35)";
            }}
          >
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="23 4 23 10 17 10" />
              <path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10" />
            </svg>
            {t("common.restart")}
          </button>
        </div>
      )}
    </div>
  );
}
