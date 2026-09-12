import { motion } from "framer-motion";
import { useState, type CSSProperties } from "react";
import { useAppI18n } from "../i18n";
import { useSettingsStore, isGlassTheme } from "../store/settingsStore";

/// 新建 Session 的内联命名表单：名称同时用于 session 名与 worktree 目录/分支
export function NewSessionForm({
  onCreate,
  onCancel,
}: {
  onCreate: (name: string) => void;
  onCancel: () => void;
}) {
  const { t, isRtl } = useAppI18n();
  const isGlass = useSettingsStore((s) => isGlassTheme(s.settings.theme));
  const [name, setName] = useState("");

  const submit = () => {
    onCreate(name.trim());
  };

  const inputStyle: CSSProperties = {
    width: "100%",
    boxSizing: "border-box",
    background: "transparent",
    border: "1px solid var(--ci-border)",
    borderRadius: 7,
    padding: "6px 9px",
    color: "var(--ci-text)",
    fontSize: 11.5,
    outline: "none",
    textAlign: "start",
    fontFamily: "-apple-system, BlinkMacSystemFont, 'SF Pro Text', sans-serif",
  };

  const buttonStyle: CSSProperties = {
    background: "none",
    border: "none",
    padding: "4px 8px",
    borderRadius: 6,
    fontSize: 11.5,
    fontWeight: 600,
    cursor: "pointer",
  };

  return (
    <motion.div
      initial={{ opacity: 0, height: 0 }}
      animate={{ opacity: 1, height: "auto" }}
      exit={{ opacity: 0, height: 0 }}
      transition={{ duration: 0.18 }}
      style={{ overflow: "hidden" }}
    >
      <div
        style={{
          background: "var(--ci-surface)",
          border: "1px solid var(--ci-toolbar-border)",
          borderRadius: 10,
          padding: 10,
          display: "flex",
          flexDirection: "column",
          gap: 8,
          marginBottom: 6,
          textShadow: isGlass ? "var(--ci-glass-text-shadow)" : "none",
        }}
      >
        <div style={{ fontSize: 11, color: "var(--ci-text-muted)", fontWeight: 500 }}>
          {t("session.newNameLabel")}
        </div>
        <input
          autoFocus
          value={name}
          dir={isRtl ? "rtl" : "ltr"}
          onChange={(e) => setName(e.target.value)}
          placeholder={t("session.newNamePlaceholder")}
          style={inputStyle}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
            if (e.key === "Escape") onCancel();
          }}
        />
        <div style={{ display: "flex", gap: 6, justifyContent: "flex-end" }}>
          <button onClick={onCancel} style={{ ...buttonStyle, color: "var(--ci-text-muted)" }}>
            {t("common.cancel")}
          </button>
          <button onClick={submit} style={{ ...buttonStyle, color: "var(--ci-accent)" }}>
            {t("common.create")}
          </button>
        </div>
      </div>
    </motion.div>
  );
}
