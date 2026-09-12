use std::{collections::HashMap, sync::Mutex};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppLocale {
    ZhCn,
    EnUs,
    Ar,
}

impl Default for AppLocale {
    fn default() -> Self {
        Self::ZhCn
    }
}

impl AppLocale {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
            Self::Ar => "ar",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" => Self::EnUs,
            "ar" | "ar-sa" | "ar-eg" => Self::Ar,
            _ => Self::ZhCn,
        }
    }
}

#[derive(Default)]
pub struct LocaleState(pub Mutex<AppLocale>);

pub fn current_locale(state: &tauri::State<'_, LocaleState>) -> AppLocale {
    *state.0.lock().unwrap()
}

pub fn translate(locale: AppLocale, key: &str, vars: &[(&str, &str)]) -> String {
    let template = match (locale, key) {
        (AppLocale::ZhCn, "notifications.hook_enabled") => "通知与 Hooks 已启用\n{{detail}}",
        (AppLocale::ZhCn, "notifications.hook_disabled") => "通知与 Hooks 已关闭\n{{detail}}",
        (AppLocale::ZhCn, "notifications.claude_listener_not_ready") => "Claude Code hook listener 未就绪",
        (AppLocale::ZhCn, "notifications.claude_hooks_not_configured") => "Claude Code hooks 未配置完成",
        (AppLocale::ZhCn, "notifications.codex_feature_disabled") => "Codex hooks feature 未启用",
        (AppLocale::ZhCn, "notifications.codex_hooks_not_configured") => "Codex hooks 未配置完成",
        (AppLocale::ZhCn, "notifications.codex_listener_not_ready") => "Codex hook listener 未就绪",
        (AppLocale::ZhCn, "notifications.codex_turn_complete") => "Codex 已完成当前回合",
        (AppLocale::ZhCn, "notifications.codex_generic") => "Codex 通知: {{type}}",
        (AppLocale::ZhCn, "notifications.send_failed") => "通知发送失败: {{error}}",
        (AppLocale::ZhCn, "notifications.session_not_found") => "未找到匹配的 Code Bar session",
        (AppLocale::ZhCn, "notifications.unknown_error") => "未知错误",
        (AppLocale::ZhCn, "session.default_name") => "会话 {{id}}",
        (AppLocale::ZhCn, "session.continue_session") => "继续会话",
        (AppLocale::ZhCn, "tray.quit") => "退出 Code Bar",
        (AppLocale::EnUs, "notifications.hook_enabled") => "Notifications and hooks are enabled\n{{detail}}",
        (AppLocale::EnUs, "notifications.hook_disabled") => "Notifications and hooks are disabled\n{{detail}}",
        (AppLocale::EnUs, "notifications.claude_listener_not_ready") => "Claude Code hook listener is not ready",
        (AppLocale::EnUs, "notifications.claude_hooks_not_configured") => "Claude Code hooks are not fully configured",
        (AppLocale::EnUs, "notifications.codex_feature_disabled") => "Codex hook feature is not enabled",
        (AppLocale::EnUs, "notifications.codex_hooks_not_configured") => "Codex hooks are not fully configured",
        (AppLocale::EnUs, "notifications.codex_listener_not_ready") => "Codex hook listener is not ready",
        (AppLocale::EnUs, "notifications.codex_turn_complete") => "Codex finished the current turn",
        (AppLocale::EnUs, "notifications.codex_generic") => "Codex notification: {{type}}",
        (AppLocale::EnUs, "notifications.send_failed") => "Failed to send notification: {{error}}",
        (AppLocale::EnUs, "notifications.session_not_found") => "No matching Code Bar session found",
        (AppLocale::EnUs, "notifications.unknown_error") => "Unknown error",
        (AppLocale::EnUs, "session.default_name") => "Session {{id}}",
        (AppLocale::EnUs, "session.continue_session") => "Continue session",
        (AppLocale::EnUs, "tray.quit") => "Quit Code Bar",
        (AppLocale::Ar, "notifications.hook_enabled") => "تم تفعيل الإشعارات والـ hooks\n{{detail}}",
        (AppLocale::Ar, "notifications.hook_disabled") => "تم تعطيل الإشعارات والـ hooks\n{{detail}}",
        (AppLocale::Ar, "notifications.claude_listener_not_ready") => "مستمع Claude Code hook غير جاهز",
        (AppLocale::Ar, "notifications.claude_hooks_not_configured") => "لم يكتمل إعداد hooks الخاصة بـ Claude Code",
        (AppLocale::Ar, "notifications.codex_feature_disabled") => "ميزة Codex hooks غير مفعلة",
        (AppLocale::Ar, "notifications.codex_hooks_not_configured") => "لم يكتمل إعداد hooks الخاصة بـ Codex",
        (AppLocale::Ar, "notifications.codex_listener_not_ready") => "مستمع Codex hook غير جاهز",
        (AppLocale::Ar, "notifications.codex_turn_complete") => "أنهى Codex الدور الحالي",
        (AppLocale::Ar, "notifications.codex_generic") => "إشعار Codex: {{type}}",
        (AppLocale::Ar, "notifications.send_failed") => "فشل إرسال الإشعار: {{error}}",
        (AppLocale::Ar, "notifications.session_not_found") => "لم يتم العثور على جلسة Code Bar مطابقة",
        (AppLocale::Ar, "notifications.unknown_error") => "خطأ غير معروف",
        (AppLocale::Ar, "session.default_name") => "الجلسة {{id}}",
        (AppLocale::Ar, "session.continue_session") => "متابعة الجلسة",
        (AppLocale::Ar, "tray.quit") => "إنهاء Code Bar",
        _ => key,
    };

    vars.iter().fold(template.to_string(), |acc, (name, value)| {
        acc.replace(&format!("{{{{{name}}}}}"), value)
    })
}

pub fn translate_owned(locale: AppLocale, key: &str, vars: HashMap<&str, String>) -> String {
    let pairs: Vec<(&str, &str)> = vars.iter().map(|(key, value)| (*key, value.as_str())).collect();
    translate(locale, key, &pairs)
}

/// 托盘菜单在 `setup()` 中创建，那时前端还没上报语言，
/// 因此把菜单项存起来，等 `set_app_locale` 到达时再改写文案。
#[derive(Default)]
pub struct TrayQuitItem(pub Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>>);

#[tauri::command]
pub fn set_app_locale(
    locale: String,
    state: tauri::State<'_, LocaleState>,
    tray_quit: tauri::State<'_, TrayQuitItem>,
) {
    let parsed = AppLocale::parse(&locale);
    *state.0.lock().unwrap() = parsed;

    if let Some(item) = tray_quit.0.lock().unwrap().as_ref() {
        let _ = item.set_text(translate(parsed, "tray.quit", &[]));
    }
}
