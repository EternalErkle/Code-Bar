import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatDate, formatTime, useAppI18n } from "../i18n";
import { RUNNER_LABELS, type RunnerType } from "../store/settingsStore";
import { useSessionStore } from "../store/sessionStore";

interface RunnerUsageSnapshot {
  runner_type: string;
  source: string;
  plan: string | null;
  five_hour_used_percent: number | null;
  five_hour_resets_at_ms: number | null;
  seven_day_used_percent: number | null;
  seven_day_resets_at_ms: number | null;
  credits_balance: string | null;
  credits_unlimited: boolean;
  last_refreshed_at_ms: number;
  error_code: string | null;
  error_detail: string | null;
}

function emptySnapshot(runnerType: string, errorDetail: string): RunnerUsageSnapshot {
  return {
    runner_type: runnerType,
    source: "unsupported",
    plan: null,
    five_hour_used_percent: null,
    five_hour_resets_at_ms: null,
    seven_day_used_percent: null,
    seven_day_resets_at_ms: null,
    credits_balance: null,
    credits_unlimited: false,
    last_refreshed_at_ms: Date.now(),
    error_code: "requestFailed",
    error_detail: errorDetail,
  };
}

function formatReset(resetsAtMs: number | null, locale: string) {
  if (resetsAtMs === null || !Number.isFinite(resetsAtMs) || resetsAtMs <= 0) {
    return { top: "", bottom: "" };
  }
  const date = new Date(resetsAtMs);
  if (Number.isNaN(date.getTime())) return { top: "", bottom: "" };
  return { top: formatTime(date, locale), bottom: formatDate(date, locale) };
}

function FlatProgress({ label, usedPercent, reset }: {
  label: string;
  usedPercent: number;
  reset: { top: string; bottom: string };
}) {
  const leftPercent = Math.max(0, Math.min(100, 100 - usedPercent));
  const tone = leftPercent <= 15 ? "var(--ci-red)" : leftPercent <= 40 ? "var(--ci-yellow-dark)" : "var(--ci-accent)";
  return (
    <div style={{ display: "grid", gap: 6, padding: "8px 9px", borderRadius: 10, background: "var(--ci-surface)", border: "1px solid var(--ci-toolbar-border)" }}>
      <div style={{ display: "flex", justifyContent: "space-between", gap: 8, alignItems: "center" }}>
        <span style={{ fontSize: 10, color: "var(--ci-text-dim)", textTransform: "uppercase", letterSpacing: "0.06em" }}>{label} limit</span>
        <span style={{ fontSize: 11, color: tone, fontWeight: 600 }}>{leftPercent.toFixed(0)}% left</span>
      </div>
      <div style={{ height: 6, background: "var(--ci-btn-ghost-bg)", borderRadius: 999, overflow: "hidden" }}>
        <div style={{ width: `${leftPercent}%`, height: "100%", background: tone, borderRadius: 999 }} />
      </div>
      {(reset.top || reset.bottom) && (
        <div style={{ display: "grid", gap: 1, fontSize: 10, color: "var(--ci-text-dim)", lineHeight: 1.3 }}>
          <span>{reset.top}</span>
          {reset.bottom && <span>{reset.bottom}</span>}
        </div>
      )}
    </div>
  );
}

export function UsageWidgetCard() {
  const { t, locale } = useAppI18n();
  const sessions = useSessionStore((s) => s.sessions);
  const expandedSessionId = useSessionStore((s) => s.expandedSessionId);
  const [loading, setLoading] = useState(false);
  const [snapshot, setSnapshot] = useState<RunnerUsageSnapshot | null>(null);
  // Drives the relative "updated Xm ago" label without refetching.
  const [nowMs, setNowMs] = useState(() => Date.now());
  const refreshAbortRef = useRef(false);

  const runnerType = useMemo<RunnerType>(() => {
    const session = sessions.find((item) => item.id === expandedSessionId) ?? null;
    return session?.runner.type ?? "claude-code";
  }, [expandedSessionId, sessions]);

  const handleRefresh = async () => {
    if (loading) return;
    setLoading(true);
    const requestRunner = runnerType;
    try {
      const next = await invoke<RunnerUsageSnapshot>("refresh_runner_usage", { runnerType: requestRunner });
      if (refreshAbortRef.current || requestRunner !== runnerType) return;
      setSnapshot(next);
      setNowMs(Date.now());
    } catch (error) {
      if (refreshAbortRef.current || requestRunner !== runnerType) return;
      setSnapshot(emptySnapshot(requestRunner, error instanceof Error ? error.message : String(error)));
      setNowMs(Date.now());
    } finally {
      if (!refreshAbortRef.current) {
        setLoading(false);
      }
    }
  };

  useEffect(() => {
    refreshAbortRef.current = false;
    setSnapshot(null);
    setLoading(false);
    void handleRefresh();
    const timer = window.setInterval(() => {
      void handleRefresh();
    }, 3 * 60 * 1000);
    return () => {
      refreshAbortRef.current = true;
      window.clearInterval(timer);
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runnerType]);

  useEffect(() => {
    const tick = window.setInterval(() => setNowMs(Date.now()), 30 * 1000);
    return () => window.clearInterval(tick);
  }, []);

  const updatedLabel = useMemo(() => {
    if (!snapshot?.last_refreshed_at_ms) return "";
    const minutes = Math.max(0, Math.floor((nowMs - snapshot.last_refreshed_at_ms) / 60000));
    return minutes < 1 ? t("usage.updatedJustNow") : t("usage.updatedAgo", { minutes });
  }, [nowMs, snapshot?.last_refreshed_at_ms, t]);

  const errorText = snapshot?.error_code
    ? t(`usage.error.${snapshot.error_code}`, { defaultValue: t("usage.error.unknown") })
    : null;

  const hasWindows =
    snapshot?.five_hour_used_percent !== null && snapshot?.five_hour_used_percent !== undefined
    || snapshot?.seven_day_used_percent !== null && snapshot?.seven_day_used_percent !== undefined;

  return (
    <div style={{
      width: "100%",
      height: "100%",
      padding: 9,
      boxSizing: "border-box",
      display: "flex",
      flexDirection: "column",
      gap: 8,
      color: "var(--ci-text)",
      overflow: "hidden",
    }}>
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
        <div style={{ fontSize: 11, fontWeight: 600, color: "var(--ci-text-muted)" }}>
            {RUNNER_LABELS[runnerType]}
        </div>
        <button
          onClick={handleRefresh}
          disabled={loading}
          style={{
            background: "var(--ci-btn-ghost-bg)",
            border: "1px solid var(--ci-toolbar-border)",
            color: "var(--ci-text-muted)",
            borderRadius: 7,
            padding: "4px 8px",
            fontSize: 11,
            cursor: loading ? "default" : "pointer",
          }}
        >
          {loading ? t("usage.refreshing") : t("usage.refresh")}
        </button>
      </div>

      <div style={{ display: "grid", gap: 8, flex: 1, minHeight: 0, overflowY: "auto" }}>
        {errorText && (
          <div style={{ display: "grid", gap: 3, fontSize: 12, color: "var(--ci-red)", lineHeight: 1.5, padding: "8px 9px", borderRadius: 10, background: "var(--ci-deleted-bg)", border: "1px solid var(--ci-toolbar-border)" }}>
            <span>{errorText}</span>
            {snapshot?.error_detail && (
              <span style={{ fontSize: 10, color: "var(--ci-text-dim)" }}>{snapshot.error_detail}</span>
            )}
          </div>
        )}

        {snapshot?.five_hour_used_percent !== null && snapshot?.five_hour_used_percent !== undefined && (
          <FlatProgress
            label="5h"
            usedPercent={snapshot.five_hour_used_percent}
            reset={formatReset(snapshot.five_hour_resets_at_ms, locale)}
          />
        )}

        {snapshot?.seven_day_used_percent !== null && snapshot?.seven_day_used_percent !== undefined && (
          <FlatProgress
            label="7d"
            usedPercent={snapshot.seven_day_used_percent}
            reset={formatReset(snapshot.seven_day_resets_at_ms, locale)}
          />
        )}

        {snapshot?.credits_balance && (
          <div style={{ fontSize: 11, color: "var(--ci-text-dim)", padding: "0 2px" }}>
            {t("usage.credits", { value: snapshot.credits_balance })}
            {snapshot.credits_unlimited ? ` · ${t("usage.creditsUnlimited")}` : ""}
          </div>
        )}

        {!snapshot && !loading && (
          <div style={{ fontSize: 12, color: "var(--ci-text-dim)", lineHeight: 1.6, padding: "8px 9px", borderRadius: 10, background: "var(--ci-surface)", border: "1px solid var(--ci-toolbar-border)" }}>
            {t("usage.empty")}
          </div>
        )}
      </div>

      {snapshot && (hasWindows || errorText) && (
        <div style={{ fontSize: 10, color: "var(--ci-text-dim)", padding: "0 2px", flexShrink: 0 }}>
          {updatedLabel}
        </div>
      )}
    </div>
  );
}
