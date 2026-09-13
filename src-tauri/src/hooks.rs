use std::fs;
use std::io::Read;
use std::path::PathBuf;

use serde_json::{Map, Value};
use serde::Serialize;
use tauri::Manager;

use crate::provider_sessions::emit_provider_session_bound;
use crate::session_lifecycle::{
    emit_session_lifecycle, HookSource, SessionLifecycleSignal, SessionRoutingHint,
};
#[cfg(not(unix))]
use crate::util::home_dir;
use crate::util::resolve_provider_file_path;

#[derive(Debug, Clone)]
struct HookCommandSpec {
    event_name: &'static str,
    matcher: Option<&'static str>,
    command: String,
    /// 存在时切换到 exec 形式：command 作为可执行文件直接启动，不经过任何 shell。
    /// 此时 `shell` 字段会被 CLI 忽略，必须保持为 None。
    args: Option<Vec<String>>,
    shell: Option<&'static str>,
    timeout: Option<u64>,
    status_message: Option<&'static str>,
    /// 写出 `"async": true`，让 hook 不阻塞 CLI 的这一轮对话。
    ///
    /// 这个桥接脚本是单向的：读 stdin、转发到本地端口，不向 CLI 返回任何决策。
    /// 同步执行时，每次事件都要等解释器冷启动完成，那段时间 CLI 一直在等。
    /// 仅用于 Claude Code：Codex 的 hooks 配置是否支持该字段未经确认。
    run_async: bool,
}

#[cfg(unix)]
fn hook_bridge_command(source: HookSource) -> Result<String, String> {
    let source_name = source.label();
    let socket_path = source.socket_path();
    Ok(format!(
        "/usr/bin/python3 -c 'import json, os, socket, sys; payload=json.load(sys.stdin); sid=os.environ.get(\"CODE_BAR_SESSION_ID\"); runner=os.environ.get(\"CODE_BAR_RUNNER_TYPE\"); tok=os.environ.get(\"CODE_BAR_HOOK_TOKEN\"); payload[\"code_bar_source\"]=\"{source_name}\"; payload[\"code_bar_session_id\"]=sid if sid else payload.get(\"code_bar_session_id\"); payload[\"code_bar_runner_type\"]=runner if runner else payload.get(\"code_bar_runner_type\"); payload[\"code_bar_hook_token\"]=tok if tok else \"\"; sock=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sock.connect(\"{socket_path}\"); sock.sendall(json.dumps(payload).encode(\"utf-8\")); sock.close()' >/dev/null 2>&1 || true"
    ))
}

#[cfg(not(unix))]
const WINDOWS_POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

#[cfg(not(unix))]
const WINDOWS_BRIDGE_SCRIPT: &str = r#"param(
  [Parameter(Mandatory = $true)][string]$Source,
  [Parameter(Mandatory = $true)][int]$Port,
  [Parameter(ValueFromRemainingArguments = $true)][string[]]$PayloadArgs
)

$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

try {
  $raw = [Console]::In.ReadToEnd()
  if ([string]::IsNullOrWhiteSpace($raw) -and $PayloadArgs.Count -gt 0) {
    $raw = $PayloadArgs -join ' '
  }
  if ([string]::IsNullOrWhiteSpace($raw)) {
    exit 0
  }

  $payload = $raw | ConvertFrom-Json
  if (-not $payload) {
    exit 0
  }

  if ($env:CODE_BAR_SESSION_ID) {
    $payload | Add-Member -NotePropertyName code_bar_session_id -NotePropertyValue $env:CODE_BAR_SESSION_ID -Force
  }
  if ($env:CODE_BAR_RUNNER_TYPE) {
    $payload | Add-Member -NotePropertyName code_bar_runner_type -NotePropertyValue $env:CODE_BAR_RUNNER_TYPE -Force
  }
  if ($env:CODE_BAR_HOOK_TOKEN) {
    $payload | Add-Member -NotePropertyName code_bar_hook_token -NotePropertyValue $env:CODE_BAR_HOOK_TOKEN -Force
  }
  $payload | Add-Member -NotePropertyName code_bar_source -NotePropertyValue $Source -Force

  $json = $payload | ConvertTo-Json -Depth 32 -Compress
  $client = [System.Net.Sockets.TcpClient]::new()
  $client.Connect([System.Net.IPAddress]::Loopback, $Port)

  try {
    $stream = $client.GetStream()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
  } finally {
    if ($stream) {
      $stream.Dispose()
    }
    $client.Dispose()
  }
} catch {
  try {
    $logPath = Join-Path (Split-Path -Parent $PSCommandPath) 'bridge-error.log'
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff'
    Add-Content -LiteralPath $logPath -Value "$stamp [$Source] $($_.Exception.Message)" -Encoding utf8
  } catch {
  }
  exit 0
}
"#;

#[cfg(not(unix))]
fn windows_hook_dir() -> Result<PathBuf, String> {
    let home = home_dir().ok_or("无法获取 HOME 环境变量")?;
    Ok(home.join(".codebar").join("hooks"))
}

#[cfg(not(unix))]
fn windows_bridge_script_path() -> Result<PathBuf, String> {
    Ok(windows_hook_dir()?.join("hook-bridge.ps1"))
}

#[cfg(not(unix))]
fn windows_bridge_node_script_path() -> Result<PathBuf, String> {
    Ok(windows_hook_dir()?.join("hook-bridge.js"))
}

/// Node 版桥接脚本，与 PowerShell 版行为一致。
///
/// Windows 上每次 hook 事件都要冷启动一次解释器。实测同一台机器上，
/// PowerShell 版约 256ms/次，Node 版约 93ms/次。
/// claude / codex 本身就是 Node 程序，所以 node 一定存在。
#[cfg(not(unix))]
const WINDOWS_BRIDGE_NODE_SCRIPT: &str = r##""use strict";
// Code Bar hook bridge：读取 hook 负载，注入会话标识后转发到本地端口。
const net = require("net");

const source = process.argv[2] || "";
const port = Number(process.argv[3] || 0);
const payloadArgs = process.argv.slice(4);

function send(raw) {
  if (!raw || !raw.trim()) process.exit(0);

  let payload;
  try {
    payload = JSON.parse(raw);
  } catch (e) {
    process.exit(0);
  }
  if (!payload || typeof payload !== "object") process.exit(0);

  if (process.env.CODE_BAR_SESSION_ID) {
    payload.code_bar_session_id = process.env.CODE_BAR_SESSION_ID;
  }
  if (process.env.CODE_BAR_RUNNER_TYPE) {
    payload.code_bar_runner_type = process.env.CODE_BAR_RUNNER_TYPE;
  }
  // 鉴权令牌必须转发，否则监听端会拒绝该请求（fail closed），
  // 会话绑定事件丢失，下次进入会话时无法 resume。
  if (process.env.CODE_BAR_HOOK_TOKEN) {
    payload.code_bar_hook_token = process.env.CODE_BAR_HOOK_TOKEN;
  }
  payload.code_bar_source = source;

  const socket = net.connect(port, "127.0.0.1", function () {
    socket.end(JSON.stringify(payload));
  });
  socket.on("error", function () {
    process.exit(0);
  });
}

// Codex 用命令行参数传负载，Claude 用 stdin。
// 有参数时不去读 stdin，否则在没有管道输入的场景下会一直等待。
if (payloadArgs.length > 0) {
  send(payloadArgs.join(" "));
} else {
  let raw = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", function (chunk) {
    raw += chunk;
  });
  process.stdin.on("end", function () {
    send(raw);
  });
  process.stdin.on("error", function () {
    process.exit(0);
  });
}
"##;

/// 可用的 node 解释器绝对路径。
///
/// exec 形式要求 command 指向真正的可执行文件，解析不到就回退 PowerShell。
#[cfg(not(unix))]
fn windows_bridge_node() -> Option<String> {
    let resolved = crate::cli_detect::resolve_command_path("node");
    let path = std::path::Path::new(&resolved);
    if path.is_absolute() && path.exists() {
        return Some(resolved);
    }
    None
}

#[cfg(not(unix))]
fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(not(unix))]
fn powershell_script_command(source: HookSource) -> Result<String, String> {
    let script = windows_bridge_script_path()?;
    let script = escape_powershell_single_quoted(&script.to_string_lossy());
    Ok(format!(
        "& '{script}' -Source '{}' -Port {}",
        source.label(),
        source.tcp_port()
    ))
}

#[cfg(not(unix))]
fn codex_notify_command() -> Result<Vec<String>, String> {
    // Codex 的 notify 本身就是「程序 + 参数」数组，直接换成 node 即可
    if let Some(node) = windows_bridge_node() {
        let node_script = windows_bridge_node_script_path()?;
        return Ok(vec![
            node,
            node_script.to_string_lossy().to_string(),
            HookSource::Codex.label().to_string(),
            HookSource::Codex.tcp_port().to_string(),
        ]);
    }

    let script = windows_bridge_script_path()?;
    Ok(vec![
        WINDOWS_POWERSHELL.to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-File".to_string(),
        script.to_string_lossy().to_string(),
        "-Source".to_string(),
        HookSource::Codex.label().to_string(),
        "-Port".to_string(),
        HookSource::Codex.tcp_port().to_string(),
    ])
}

#[cfg(not(unix))]
fn write_text_file_if_changed(path: &PathBuf, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }

    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => return Err(format!("读取 {} 失败: {err}", path.display())),
    };

    if needs_write {
        fs::write(path, content).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn ensure_windows_hook_bridge_assets() -> Result<(), String> {
    let script_path = windows_bridge_script_path()?;
    write_text_file_if_changed(&script_path, WINDOWS_BRIDGE_SCRIPT)?;
    // 两个脚本都写出：node 解析失败时仍要能回退到 PowerShell 版
    let node_script_path = windows_bridge_node_script_path()?;
    write_text_file_if_changed(&node_script_path, WINDOWS_BRIDGE_NODE_SCRIPT)?;
    Ok(())
}

#[cfg(not(unix))]
fn hook_bridge_command(source: HookSource) -> Result<String, String> {
    ensure_windows_hook_bridge_assets()?;
    powershell_script_command(source)
}

/// 桥接脚本的调用方式。
struct BridgeInvocation {
    command: String,
    args: Option<Vec<String>>,
    shell: Option<&'static str>,
}

#[cfg(unix)]
fn hook_bridge_invocation(source: HookSource) -> Result<BridgeInvocation, String> {
    Ok(BridgeInvocation {
        command: hook_bridge_command(source)?,
        args: None,
        shell: None,
    })
}

#[cfg(not(unix))]
fn hook_bridge_invocation(source: HookSource) -> Result<BridgeInvocation, String> {
    ensure_windows_hook_bridge_assets()?;

    // 首选 exec 形式 + node：完全不经过 shell，解释器启动成本约为 PowerShell 的三分之一
    if let Some(node) = windows_bridge_node() {
        let script = windows_bridge_node_script_path()?;
        return Ok(BridgeInvocation {
            command: node,
            args: Some(vec![
                script.to_string_lossy().to_string(),
                source.label().to_string(),
                source.tcp_port().to_string(),
            ]),
            // args 存在时 shell 字段会被忽略，这里必须留空
            shell: None,
        });
    }

    // 回退：解析不到 node 时继续走 PowerShell 脚本
    Ok(BridgeInvocation {
        command: powershell_script_command(source)?,
        args: None,
        shell: Some("powershell"),
    })
}

fn hook_specs(source: HookSource) -> Result<Vec<HookCommandSpec>, String> {
    let BridgeInvocation {
        command,
        args,
        shell,
    } = hook_bridge_invocation(source)?;

    match source {
        HookSource::ClaudeCode => Ok(vec![
            HookCommandSpec {
                event_name: "UserPromptSubmit",
                matcher: Some(""),
                command: command.clone(),
                args: args.clone(),
                shell,
                timeout: None,
                status_message: None,
                run_async: true,
            },
            HookCommandSpec {
                event_name: "Stop",
                matcher: Some(""),
                command: command.clone(),
                args: args.clone(),
                shell,
                timeout: None,
                status_message: None,
                run_async: true,
            },
            HookCommandSpec {
                event_name: "StopFailure",
                matcher: Some(""),
                command: command.clone(),
                args: args.clone(),
                shell,
                timeout: None,
                status_message: None,
                run_async: true,
            },
            HookCommandSpec {
                event_name: "Notification",
                matcher: Some(""),
                command,
                args: args.clone(),
                shell,
                timeout: None,
                status_message: None,
                run_async: true,
            },
        ]),
        #[cfg(unix)]
        HookSource::Codex => Ok(vec![
            HookCommandSpec {
                event_name: "UserPromptSubmit",
                matcher: None,
                command: command.clone(),
                args: args.clone(),
                shell: None,
                timeout: Some(5),
                status_message: None,
                run_async: false,
            },
            HookCommandSpec {
                event_name: "Stop",
                matcher: None,
                command,
                args: args.clone(),
                shell: None,
                timeout: Some(5),
                status_message: None,
                run_async: false,
            },
        ]),
        #[cfg(not(unix))]
        HookSource::Codex => Ok(Vec::new()),
    }
}

#[cfg(unix)]
fn managed_legacy_commands(source: HookSource) -> Vec<String> {
    let mut commands = Vec::new();
    match source {
        HookSource::ClaudeCode => {
            commands.push("nc -U /tmp/code-bar-hook.sock".to_string());
            commands.push("/tmp/code-bar-hook-claude.sock".to_string());
            commands.push(
                crate::util::codebar_runtime_dir()
                    .join("code-bar-hook-claude.sock")
                    .to_string_lossy()
                    .to_string(),
            );
        }
        HookSource::Codex => {
            commands.push(
                crate::util::codebar_runtime_dir()
                    .join("code-bar-hook-codex.sock")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    commands
}

fn is_managed_command(command: &str, source: HookSource) -> bool {
    #[cfg(unix)]
    {
        let socket_path = source.socket_path();
        return command.contains(&socket_path)
            || managed_legacy_commands(source)
                .iter()
                .any(|legacy| command.contains(legacy));
    }

    #[cfg(not(unix))]
    {
        let fingerprint = command.contains("hook-bridge.ps1")
            || command.contains("hook-bridge.js")
            || command.contains(".codebar")
            || (command.contains(WINDOWS_POWERSHELL) && command.contains("EncodedCommand"));
        if fingerprint {
            return true;
        }
        hook_bridge_command(source)
            .map(|managed| command == managed)
            .unwrap_or(false)
    }
}

/// 把 hook 条目的 command 与 args 拼成一条用于指纹匹配的字符串。
///
/// exec 形式下脚本路径在 args 里，command 只是解释器路径。
/// 只看 command 会认不出自己写过的条目：旧条目永远不被清理，
/// 同时每次启动又追加一条新的，hooks 数组会不断堆积。
fn managed_hook_haystack(hook: &Value) -> String {
    let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let args = hook
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    format!("{command} {args}")
}

fn is_managed_hook(hook: &Value, source: HookSource) -> bool {
    is_managed_command(&managed_hook_haystack(hook), source)
}

/// 条目是否与 spec 完全一致：command 和 args 都要匹配。
fn hook_matches_spec(hook: &Value, spec: &HookCommandSpec) -> bool {
    if hook.get("command").and_then(|v| v.as_str()) != Some(spec.command.as_str()) {
        return false;
    }

    let hook_args: Vec<&str> = hook
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    match &spec.args {
        Some(args) => {
            hook_args.len() == args.len()
                && hook_args
                    .iter()
                    .zip(args.iter())
                    .all(|(actual, expected)| *actual == expected.as_str())
        }
        None => hook_args.is_empty(),
    }
}

fn load_json_file(path: &PathBuf) -> Result<Value, String> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let content =
        fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    Ok(serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({})))
}

fn ensure_hooks_object<'a>(
    root: &'a mut Value,
    path_label: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    if root.get("hooks").is_none() {
        root["hooks"] = serde_json::json!({});
    }
    root["hooks"]
        .as_object_mut()
        .ok_or_else(|| format!("{path_label} 中 hooks 字段格式异常"))
}

fn build_hook_entry(spec: &HookCommandSpec) -> Value {
    let mut hook = serde_json::json!({
        "type": "command",
        "command": spec.command.clone(),
    });
    if let Some(args) = &spec.args {
        hook["args"] = Value::Array(args.iter().map(|a| Value::from(a.clone())).collect());
    }
    if let Some(shell) = spec.shell {
        hook["shell"] = Value::from(shell);
    }
    if let Some(timeout) = spec.timeout {
        hook["timeout"] = Value::from(timeout);
    }
    if let Some(status_message) = spec.status_message {
        hook["statusMessage"] = Value::from(status_message);
    }
    if spec.run_async {
        hook["async"] = Value::from(true);
    }

    let mut group = Map::new();
    if let Some(matcher) = spec.matcher {
        group.insert("matcher".to_string(), Value::from(matcher));
    }
    group.insert("hooks".to_string(), Value::Array(vec![hook]));
    Value::Object(group)
}

fn normalize_managed_hook(hook: &mut Value, spec: &HookCommandSpec) -> bool {
    let Some(obj) = hook.as_object_mut() else {
        *hook = serde_json::json!({});
        return normalize_managed_hook(hook, spec);
    };

    let mut changed = false;

    if obj.get("type").and_then(|v| v.as_str()) != Some("command") {
        obj.insert("type".to_string(), Value::from("command"));
        changed = true;
    }
    if obj.get("command").and_then(|v| v.as_str()) != Some(spec.command.as_str()) {
        obj.insert("command".to_string(), Value::from(spec.command.clone()));
        changed = true;
    }
    match spec.shell {
        Some(shell) => {
            if obj.get("shell").and_then(|v| v.as_str()) != Some(shell) {
                obj.insert("shell".to_string(), Value::from(shell));
                changed = true;
            }
        }
        None => {
            if obj.remove("shell").is_some() {
                changed = true;
            }
        }
    }

    match &spec.args {
        Some(args) => {
            let desired = Value::Array(args.iter().map(|a| Value::from(a.clone())).collect());
            if obj.get("args") != Some(&desired) {
                obj.insert("args".to_string(), desired);
                changed = true;
            }
        }
        None => {
            if obj.remove("args").is_some() {
                changed = true;
            }
        }
    }

    match spec.timeout {
        Some(timeout) => {
            if obj.get("timeout").and_then(|v| v.as_u64()) != Some(timeout) {
                obj.insert("timeout".to_string(), Value::from(timeout));
                changed = true;
            }
        }
        None => {
            if obj.remove("timeout").is_some() {
                changed = true;
            }
        }
    }

    match spec.status_message {
        Some(status_message) => {
            if obj.get("statusMessage").and_then(|v| v.as_str()) != Some(status_message) {
                obj.insert("statusMessage".to_string(), Value::from(status_message));
                changed = true;
            }
        }
        None => {
            if obj.remove("statusMessage").is_some() {
                changed = true;
            }
        }
    }

    if spec.run_async {
        if obj.get("async").and_then(|v| v.as_bool()) != Some(true) {
            obj.insert("async".to_string(), Value::from(true));
            changed = true;
        }
    } else if obj.remove("async").is_some() {
        changed = true;
    }

    changed
}

fn merge_hook_specs(
    root: &mut Value,
    source: HookSource,
    specs: &[HookCommandSpec],
    path_label: &str,
) -> Result<Vec<String>, String> {
    let hooks_obj = ensure_hooks_object(root, path_label)?;
    let mut changed_events = Vec::new();

    for spec in specs {
        let event_value = hooks_obj
            .entry(spec.event_name.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));

        if !event_value.is_array() {
            *event_value = Value::Array(Vec::new());
        }
        let event_arr = event_value
            .as_array_mut()
            .ok_or_else(|| format!("{path_label} 中 {} hooks 不是数组", spec.event_name))?;

        let mut has_current_command = false;
        let mut event_changed = false;

        event_arr.retain_mut(|group| {
            let Some(group_obj) = group.as_object_mut() else {
                event_changed = true;
                return false;
            };

            let hooks = group_obj
                .entry("hooks".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));

            if !hooks.is_array() {
                *hooks = Value::Array(Vec::new());
                event_changed = true;
            }

            let hook_arr = hooks.as_array_mut().expect("hooks array just normalized");
            let before_len = hook_arr.len();

            hook_arr.retain_mut(|hook| {
                if hook_matches_spec(hook, spec) {
                    if has_current_command {
                        event_changed = true;
                        return false;
                    }
                    has_current_command = true;
                    if normalize_managed_hook(hook, spec) {
                        event_changed = true;
                    }
                    return true;
                }
                if is_managed_hook(hook, source) {
                    event_changed = true;
                    return false;
                }
                true
            });

            if hook_arr.len() != before_len {
                event_changed = true;
            }

            !hook_arr.is_empty()
        });

        if !has_current_command {
            event_arr.push(build_hook_entry(spec));
            event_changed = true;
        }

        if event_changed {
            changed_events.push(spec.event_name.to_string());
        }
    }

    Ok(changed_events)
}

fn save_json_file(path: &PathBuf, json: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }
    let output = serde_json::to_string_pretty(json)
        .map_err(|e| format!("序列化 {} 失败: {e}", path.display()))?;
    fs::write(path, output).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

fn strip_managed_hooks(root: &mut Value, source: HookSource) -> bool {
    let Some(hooks_obj) = root.get_mut("hooks").and_then(|value| value.as_object_mut()) else {
        return false;
    };

    let event_names: Vec<String> = hooks_obj.keys().cloned().collect();
    let mut changed = false;
    let mut empty_events = Vec::new();

    for event_name in event_names {
        let Some(event_value) = hooks_obj.get_mut(&event_name) else {
            continue;
        };
        let Some(event_arr) = event_value.as_array_mut() else {
            continue;
        };

        let before_group_len = event_arr.len();
        event_arr.retain_mut(|group| {
            let Some(group_obj) = group.as_object_mut() else {
                changed = true;
                return false;
            };

            let Some(hooks_value) = group_obj.get_mut("hooks") else {
                changed = true;
                return false;
            };
            let Some(hook_arr) = hooks_value.as_array_mut() else {
                changed = true;
                return false;
            };

            let before_hook_len = hook_arr.len();
            hook_arr.retain(|hook| !is_managed_hook(hook, source));
            if hook_arr.len() != before_hook_len {
                changed = true;
            }

            !hook_arr.is_empty()
        });

        if event_arr.len() != before_group_len {
            changed = true;
        }

        if event_arr.is_empty() {
            empty_events.push(event_name);
        }
    }

    for event_name in empty_events {
        hooks_obj.remove(&event_name);
        changed = true;
    }

    changed
}

fn has_managed_hook_for_spec(root: &Value, spec: &HookCommandSpec) -> bool {
    root.get("hooks")
        .and_then(|value| value.as_object())
        .and_then(|hooks_obj| hooks_obj.get(spec.event_name))
        .and_then(|value| value.as_array())
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command").and_then(|value| value.as_str())
                                == Some(spec.command.as_str())
                        })
                    })
            })
        })
}

fn missing_managed_events(root: &Value, specs: &[HookCommandSpec]) -> Vec<String> {
    specs.iter()
        .filter(|spec| !has_managed_hook_for_spec(root, spec))
        .map(|spec| spec.event_name.to_string())
        .collect()
}

fn ensure_claude_hook_settings() -> Result<String, String> {
    let settings_path = resolve_provider_file_path("claude-code", "", "settings.json")
        .ok_or("无法解析 Claude Code 配置目录")?;
    let mut settings = load_json_file(&settings_path)?;
    let specs = hook_specs(HookSource::ClaudeCode)?;
    let changed = merge_hook_specs(
        &mut settings,
        HookSource::ClaudeCode,
        &specs,
        &settings_path.display().to_string(),
    )?;

    if changed.is_empty() {
        return Ok("Claude Code hooks 已是最新，无需修改".to_string());
    }

    save_json_file(&settings_path, &settings)?;
    Ok(format!(
        "已配置 Claude Code hooks: {} ({})",
        settings_path.display(),
        changed.join(", ")
    ))
}

fn disable_claude_hook_settings() -> Result<String, String> {
    let settings_path = resolve_provider_file_path("claude-code", "", "settings.json")
        .ok_or("无法解析 Claude Code 配置目录")?;
    let mut settings = load_json_file(&settings_path)?;
    let changed = strip_managed_hooks(&mut settings, HookSource::ClaudeCode);

    if !changed {
        return Ok("Claude Code hooks 已关闭或未配置".to_string());
    }

    save_json_file(&settings_path, &settings)?;
    Ok(format!("已关闭 Claude Code hooks: {}", settings_path.display()))
}

fn load_toml_file(path: &PathBuf) -> Result<toml::Table, String> {
    let content = if path.exists() {
        fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?
    } else {
        String::new()
    };

    if content.trim().is_empty() {
        Ok(toml::Table::new())
    } else {
        Ok(content
            .parse::<toml::Table>()
            .unwrap_or_else(|_| toml::Table::new()))
    }
}

fn save_toml_file(path: &PathBuf, table: &toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }

    let output = toml::to_string_pretty(table)
        .map_err(|e| format!("序列化 {} 失败: {e}", path.display()))?;
    fs::write(path, output).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

#[cfg(unix)]
fn ensure_codex_feature_flag() -> Result<String, String> {
    let config_path = resolve_provider_file_path("codex", "", "config.toml")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut config = load_toml_file(&config_path)?;

    let features = config
        .entry("features")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));

    if !features.is_table() {
        *features = toml::Value::Table(toml::Table::new());
    }

    let features_table = features
        .as_table_mut()
        .ok_or_else(|| format!("{} 中 [features] 配置格式异常", config_path.display()))?;

    let already_enabled = features_table.get("codex_hooks").and_then(|v| v.as_bool()) == Some(true);

    if already_enabled {
        return Ok(format!(
            "Codex hooks feature 已启用: {}",
            config_path.display()
        ));
    }

    features_table.insert("codex_hooks".to_string(), toml::Value::Boolean(true));
    save_toml_file(&config_path, &config)?;

    Ok(format!(
        "已启用 Codex hooks feature: {}",
        config_path.display()
    ))
}

#[cfg(not(unix))]
fn ensure_codex_notify_settings() -> Result<String, String> {
    ensure_windows_hook_bridge_assets()?;

    let config_path = resolve_provider_file_path("codex", "", "config.toml")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut config = load_toml_file(&config_path)?;
    let desired = codex_notify_command()?;
    let desired_values: Vec<toml::Value> =
        desired.iter().cloned().map(toml::Value::String).collect();

    let notify_changed = match config.get("notify") {
        Some(toml::Value::Array(current)) => current != &desired_values,
        Some(_) => true,
        None => true,
    };

    if notify_changed {
        config.insert("notify".to_string(), toml::Value::Array(desired_values));
        save_toml_file(&config_path, &config)?;
        return Ok(format!(
            "已配置 Codex Windows notify: {}",
            config_path.display()
        ));
    }

    Ok(format!(
        "Codex Windows notify 已是最新: {}",
        config_path.display()
    ))
}

#[cfg(unix)]
fn ensure_codex_hook_settings() -> Result<String, String> {
    let hooks_path = resolve_provider_file_path("codex", "", "hooks.json")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut hooks = load_json_file(&hooks_path)?;
    let specs = hook_specs(HookSource::Codex)?;
    let changed = merge_hook_specs(
        &mut hooks,
        HookSource::Codex,
        &specs,
        &hooks_path.display().to_string(),
    )?;

    if changed.is_empty() {
        return Ok("Codex hooks 已是最新，无需修改".to_string());
    }

    save_json_file(&hooks_path, &hooks)?;
    Ok(format!(
        "已配置 Codex hooks: {} ({})",
        hooks_path.display(),
        changed.join(", ")
    ))
}

#[cfg(not(unix))]
fn ensure_codex_hook_settings() -> Result<String, String> {
    ensure_codex_notify_settings()
}

#[cfg(unix)]
fn disable_codex_feature_flag() -> Result<String, String> {
    let config_path = resolve_provider_file_path("codex", "", "config.toml")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut config = load_toml_file(&config_path)?;

    let Some(features) = config.get_mut("features").and_then(|value| value.as_table_mut()) else {
        return Ok("Codex hooks feature 已关闭或未配置".to_string());
    };

    let already_disabled = features
        .get("codex_hooks")
        .and_then(|value| value.as_bool())
        != Some(true);

    if already_disabled {
        return Ok("Codex hooks feature 已关闭或未配置".to_string());
    }

    features.insert("codex_hooks".to_string(), toml::Value::Boolean(false));
    save_toml_file(&config_path, &config)?;
    Ok(format!("已关闭 Codex hooks feature: {}", config_path.display()))
}

#[cfg(not(unix))]
fn disable_codex_feature_flag() -> Result<String, String> {
    Ok("Windows 平台无需关闭 Codex hooks feature".to_string())
}

#[cfg(unix)]
fn disable_codex_hook_settings() -> Result<String, String> {
    let hooks_path = resolve_provider_file_path("codex", "", "hooks.json")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut hooks = load_json_file(&hooks_path)?;
    let changed = strip_managed_hooks(&mut hooks, HookSource::Codex);

    let hook_message = if changed {
        save_json_file(&hooks_path, &hooks)?;
        format!("已关闭 Codex hooks: {}", hooks_path.display())
    } else {
        "Codex hooks 已关闭或未配置".to_string()
    };

    let feature_message = disable_codex_feature_flag()?;
    Ok(format!("{feature_message}\n{hook_message}"))
}

#[cfg(not(unix))]
fn disable_codex_hook_settings() -> Result<String, String> {
    let config_path = resolve_provider_file_path("codex", "", "config.toml")
        .ok_or("无法解析 Codex 配置目录")?;
    let mut config = load_toml_file(&config_path)?;

    if config.remove("notify").is_none() {
        return Ok("Codex Windows notify 已关闭或未配置".to_string());
    }

    save_toml_file(&config_path, &config)?;
    Ok(format!("已关闭 Codex Windows notify: {}", config_path.display()))
}

pub fn disable_all_hooks() -> Result<String, String> {
    let claude = disable_claude_hook_settings()?;
    let codex = disable_codex_hook_settings()?;
    Ok(format!("{claude}\n{codex}"))
}

#[derive(Debug, Serialize)]
pub struct NotificationHookStatus {
    enabled: bool,
    claude_hooks_configured: bool,
    codex_hooks_configured: bool,
    codex_feature_enabled: bool,
    claude_listener_ready: bool,
    codex_listener_ready: bool,
    healthy: bool,
    issues: Vec<String>,
}

fn claude_hooks_configured() -> Result<bool, String> {
    let settings_path = resolve_provider_file_path("claude-code", "", "settings.json")
        .ok_or("无法解析 Claude Code 配置目录")?;
    let settings = load_json_file(&settings_path)?;
    let specs = hook_specs(HookSource::ClaudeCode)?;
    Ok(missing_managed_events(&settings, &specs).is_empty())
}

fn codex_hooks_configured() -> Result<bool, String> {
    #[cfg(unix)]
    {
        let hooks_path = resolve_provider_file_path("codex", "", "hooks.json")
            .ok_or("无法解析 Codex 配置目录")?;
        let hooks = load_json_file(&hooks_path)?;
        let specs = hook_specs(HookSource::Codex)?;
        return Ok(missing_managed_events(&hooks, &specs).is_empty());
    }

    #[cfg(not(unix))]
    {
        let config_path = resolve_provider_file_path("codex", "", "config.toml")
            .ok_or("无法解析 Codex 配置目录")?;
        let config = load_toml_file(&config_path)?;
        return Ok(matches!(config.get("notify"), Some(toml::Value::Array(values)) if !values.is_empty()));
    }
}

fn codex_feature_enabled() -> Result<bool, String> {
    #[cfg(unix)]
    {
        let config_path = resolve_provider_file_path("codex", "", "config.toml")
            .ok_or("无法解析 Codex 配置目录")?;
        let config = load_toml_file(&config_path)?;
        let enabled = config
            .get("features")
            .and_then(|value| value.as_table())
            .and_then(|table| table.get("codex_hooks"))
            .and_then(|value| value.as_bool())
            == Some(true);
        return Ok(enabled);
    }

    #[cfg(not(unix))]
    {
        Ok(true)
    }
}

fn hook_listener_ready(source: HookSource) -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new(&source.socket_path()).exists()
    }

    #[cfg(not(unix))]
    {
        let _ = source;
        true
    }
}

#[tauri::command]
pub fn setup_claude_hooks() -> Result<String, String> {
    ensure_claude_hook_settings()
}

#[tauri::command]
pub fn setup_codex_hooks() -> Result<String, String> {
    #[cfg(unix)]
    {
        let feature = ensure_codex_feature_flag()?;
        let hooks = ensure_codex_hook_settings()?;
        return Ok(format!("{feature}\n{hooks}"));
    }

    #[cfg(not(unix))]
    {
        let notify = ensure_codex_hook_settings()?;
        return Ok(format!(
            "Codex 官方文档当前声明 Windows hooks 已禁用；已改为配置 notify。\n{notify}"
        ));
    }

    #[allow(unreachable_code)]
    Ok(String::new())
}

#[tauri::command]
pub fn setup_all_hooks() -> Result<String, String> {
    let claude = ensure_claude_hook_settings()?;
    #[cfg(unix)]
    {
        let codex_feature = ensure_codex_feature_flag()?;
        let codex_hooks = ensure_codex_hook_settings()?;
        return Ok(format!("{claude}\n{codex_feature}\n{codex_hooks}"));
    }

    #[cfg(not(unix))]
    {
        let codex_notify = ensure_codex_hook_settings()?;
        return Ok(format!(
            "{claude}\nCodex 官方文档当前声明 Windows hooks 已禁用；已改为配置 notify。\n{codex_notify}"
        ));
    }

    #[allow(unreachable_code)]
    Ok(claude)
}

pub fn reconcile_integrations_on_startup(app: &tauri::AppHandle) -> Result<String, String> {
    let locale = crate::i18n::current_locale(&app.state::<crate::i18n::LocaleState>());
    if crate::integration_control::notifications_and_hooks_enabled(app) {
        let configured = setup_all_hooks()?;
        return Ok(crate::i18n::translate(locale, "notifications.hook_enabled", &[("detail", &configured)]));
    }

    let disabled = disable_all_hooks()?;
    Ok(crate::i18n::translate(locale, "notifications.hook_disabled", &[("detail", &disabled)]))
}

#[tauri::command]
pub fn set_notifications_and_hooks_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<String, String> {
    let result = if enabled {
        setup_all_hooks()?
    } else {
        disable_all_hooks()?
    };

    crate::integration_control::save_preferences(&app, enabled)?;
    Ok(result)
}

#[tauri::command]
pub fn get_notifications_and_hooks_status(
    app: tauri::AppHandle,
) -> Result<NotificationHookStatus, String> {
    let locale = crate::i18n::current_locale(&app.state::<crate::i18n::LocaleState>());
    let enabled = crate::integration_control::notifications_and_hooks_enabled(&app);
    let claude_hooks_configured = claude_hooks_configured()?;
    let codex_hooks_configured = codex_hooks_configured()?;
    let codex_feature_enabled = codex_feature_enabled()?;
    let claude_listener_ready = hook_listener_ready(HookSource::ClaudeCode);
    let codex_listener_ready = hook_listener_ready(HookSource::Codex);

    let mut issues = Vec::new();
    if enabled && !claude_hooks_configured {
        issues.push(crate::i18n::translate(locale, "notifications.claude_hooks_not_configured", &[]));
    }
    if enabled && !codex_hooks_configured {
        issues.push(crate::i18n::translate(locale, "notifications.codex_hooks_not_configured", &[]));
    }
    if enabled && !codex_feature_enabled {
        issues.push(crate::i18n::translate(locale, "notifications.codex_feature_disabled", &[]));
    }
    if enabled && !claude_listener_ready {
        issues.push(crate::i18n::translate(locale, "notifications.claude_listener_not_ready", &[]));
    }
    if enabled && !codex_listener_ready {
        issues.push(crate::i18n::translate(locale, "notifications.codex_listener_not_ready", &[]));
    }

    let healthy = if enabled {
        issues.is_empty()
    } else {
        true
    };

    Ok(NotificationHookStatus {
        enabled,
        claude_hooks_configured,
        codex_hooks_configured,
        codex_feature_enabled,
        claude_listener_ready,
        codex_listener_ready,
        healthy,
        issues,
    })
}

/// Claude Code 用正斜杠、保留大小写的绝对路径作为 `projects` 的 key。
fn normalize_trust_path(path: &str) -> String {
    let mut normalized = path.trim().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        // 保留 `C:/` 这类盘符根路径
        if normalized.len() == 3 && normalized.as_bytes()[1] == b':' {
            break;
        }
        normalized.pop();
    }
    normalized
}

/// 把工作区标记为“已信任”，写入 Claude CLI 真正读取的位置。
///
/// Verified against Claude Code 2.1.269 on 2026-09-12: the shipped `claude.exe`
/// contains `hasTrustDialogAccepted` (24 occurrences) and **zero** occurrences of
/// `trustedDirectories`, and every directory the CLI has actually been run in is
/// recorded in `~/.claude.json` under `projects.<path>.hasTrustDialogAccepted`.
/// The previous implementation appended to a `trustedDirectories` array in
/// `~/.claude/settings.json`, which the CLI ignores entirely.
///
/// `~/.claude.json` is owned by the CLI and can be large, so this rewrites the file
/// only when the flag is actually missing. App startup calls this for every
/// workspace on every launch, and after the first run those calls are now no-ops.
#[tauri::command]
pub fn trust_workspace(path: String) -> Result<(), String> {
    let normalized = normalize_trust_path(&path);
    if normalized.is_empty() {
        return Err("工作区路径为空".to_string());
    }

    let config_path = home_dir()
        .ok_or("无法解析 Claude Code 配置目录")?
        .join(".claude.json");

    let content = if config_path.exists() {
        fs::read_to_string(&config_path).map_err(|e| e.to_string())?
    } else {
        "{}".to_string()
    };

    let mut json: Value = serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    {
        let root = json.as_object_mut().ok_or("~/.claude.json 格式错误")?;

        let projects = root
            .entry("projects".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !projects.is_object() {
            *projects = serde_json::json!({});
        }
        let projects = projects
            .as_object_mut()
            .ok_or("~/.claude.json projects 格式错误")?;

        let project = projects
            .entry(normalized)
            .or_insert_with(|| serde_json::json!({}));
        if !project.is_object() {
            *project = serde_json::json!({});
        }
        let project = project
            .as_object_mut()
            .ok_or("~/.claude.json project 格式错误")?;

        if project
            .get("hasTrustDialogAccepted")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Ok(());
        }

        project.insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
    }

    let out = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&config_path, out).map_err(|e| e.to_string())?;

    Ok(())
}

fn codex_notify_message(locale: crate::i18n::AppLocale, json: &Value) -> Option<(String, String, String)> {
    let notification_type = json
        .get("type")
        .or_else(|| json.get("event"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "Codex".to_string());

    let message = [
        json.get("message"),
        json.get("last-assistant-message"),
        json.get("last_assistant_message"),
        json.get("summary"),
        json.get("detail"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|value| value.as_str())
    .map(str::trim)
    .find(|s| !s.is_empty())
    .map(ToString::to_string)
    .unwrap_or_else(|| match notification_type.as_str() {
        "agent-turn-complete" => crate::i18n::translate(locale, "notifications.codex_turn_complete", &[]),
        other => crate::i18n::translate(locale, "notifications.codex_generic", &[("type", other)]),
    });

    Some((title, message, notification_type))
}

fn dispatch_hook_event(app: &tauri::AppHandle, source: HookSource, json: &Value) {
    // 鉴权闸口：两种传输（Unix Socket / 回环 TCP）都汇聚到这里，
    // 因此校验放在这一处即可覆盖全部入口，不存在绕过路径。
    // 缺失 / 为空 / 不匹配，以及本次运行没能生成令牌时，一律丢弃。
    // 注意：以下任何分支都不得把令牌本身写进日志。
    if !crate::hook_auth::verify(
        json.get(crate::hook_auth::HOOK_TOKEN_FIELD)
            .and_then(|v| v.as_str()),
    ) {
        eprintln!(
            "[hooks:{}] rejected: missing or invalid auth token",
            source.label()
        );
        return;
    }

    let locale = crate::i18n::current_locale(&app.state::<crate::i18n::LocaleState>());
    if !crate::integration_control::notifications_and_hooks_enabled(app) {
        eprintln!(
            "[hooks:{}] ignored because notifications and hooks are disabled",
            source.label()
        );
        return;
    }

    let event_name = json
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let routing = SessionRoutingHint {
        source,
        code_bar_session_id: json
            .get("code_bar_session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        cwd: json
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    eprintln!(
        "[hooks:{}] received: {} code_bar_session={:?} cwd={:?}",
        source.label(),
        event_name,
        routing.code_bar_session_id,
        routing.cwd
    );

    match source {
        HookSource::ClaudeCode => match event_name {
            "UserPromptSubmit" => {
                emit_provider_session_bound(app, &routing, json);
                emit_session_lifecycle(app, routing, SessionLifecycleSignal::Running);
            }
            "Stop" => {
                emit_session_lifecycle(app, routing, SessionLifecycleSignal::Waiting);
            }
            "StopFailure" => {
                let translated_unknown_error = crate::i18n::translate(locale, "notifications.unknown_error", &[]);
                let error = json
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or(translated_unknown_error.as_str());
                emit_session_lifecycle(
                    app,
                    routing,
                    SessionLifecycleSignal::Error {
                        message: error.to_string(),
                    },
                );
            }
            "Notification" => {
                let title = json
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Claude Code");
                let message = json.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let notification_type = json
                    .get("notification_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                emit_session_lifecycle(
                    app,
                    routing,
                    SessionLifecycleSignal::Attention {
                        title: title.to_string(),
                        message: message.to_string(),
                        notification_type: notification_type.to_string(),
                    },
                );
            }
            _ => {}
        },
        HookSource::Codex => match event_name {
            "" => {
                // Codex notify payload. On Windows this is the only structured signal we
                // get, because Codex disables hooks there and hook_specs returns an empty
                // list. Without binding here the resume path falls back to the cwd-based
                // history scan, which provider_sessions documents as unreliable when
                // several sessions share a directory. emit_provider_session_bound is a
                // no-op when the payload carries no recognisable identifier.
                emit_provider_session_bound(app, &routing, json);
                if let Some((title, message, notification_type)) = codex_notify_message(locale, json) {
                    emit_session_lifecycle(
                        app,
                        routing,
                        SessionLifecycleSignal::Attention {
                            title,
                            message,
                            notification_type,
                        },
                    );
                }
            }
            "UserPromptSubmit" => {
                emit_provider_session_bound(app, &routing, json);
                emit_session_lifecycle(app, routing, SessionLifecycleSignal::Running);
            }
            "Stop" => {
                emit_session_lifecycle(app, routing, SessionLifecycleSignal::Waiting);
            }
            _ => {}
        },
    }
}

#[cfg(unix)]
fn start_hook_socket_server(app: tauri::AppHandle, source: HookSource) {
    use std::os::unix::net::UnixListener;

    let socket_path = source.socket_path();
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "[hooks:{}] 创建 socket 目录失败 {}: {e}",
                source.label(),
                parent.display()
            );
            return;
        }
    }

    let _ = fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[hooks:{}] bind 失败: {e}", source.label());
            return;
        }
    };

    eprintln!("[hooks:{}] listening on {}", source.label(), socket_path);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("[hooks:{}] accept 错误: {e}", source.label());
                    continue;
                }
            };

            let mut reader = std::io::BufReader::new(stream);
            let mut payload = String::new();
            if let Err(e) = reader.read_to_string(&mut payload) {
                eprintln!("[hooks:{}] read 失败: {e}", source.label());
                continue;
            }

            let json: Value = match serde_json::from_str(&payload) {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("[hooks:{}] JSON 解析失败: {e}", source.label());
                    continue;
                }
            };

            dispatch_hook_event(&app, source, &json);
        }
    });
}

#[cfg(not(unix))]
fn start_hook_socket_server(app: tauri::AppHandle, source: HookSource) {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, source.tcp_port()));
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[hooks:{}] bind {} 失败: {e}", source.label(), addr);
            return;
        }
    };

    eprintln!("[hooks:{}] listening on {}", source.label(), addr);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("[hooks:{}] accept 错误: {e}", source.label());
                    continue;
                }
            };

            let mut reader = std::io::BufReader::new(stream);
            let mut payload = String::new();
            if let Err(e) = reader.read_to_string(&mut payload) {
                eprintln!("[hooks:{}] read 失败: {e}", source.label());
                continue;
            }

            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }

            let json: Value = match serde_json::from_str(payload) {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("[hooks:{}] JSON 解析失败: {e}", source.label());
                    continue;
                }
            };

            dispatch_hook_event(&app, source, &json);
        }
    });
}

pub fn start_hook_socket_servers(app: tauri::AppHandle) {
    start_hook_socket_server(app.clone(), HookSource::ClaudeCode);
    start_hook_socket_server(app, HookSource::Codex);
}

/// 发送系统通知（支持点击回调，委托给 notification 模块）
///
/// macOS 下使用 mac-notification-sys 实现常驻通知 + 点击回调；
/// 点击后统一走 focus_popup(session_id) 链路。
#[tauri::command]
pub fn send_notification(
    app: tauri::AppHandle,
    title: String,
    body: String,
    session_id: Option<String>,
) -> Result<(), String> {
    crate::notification::send_notification_with_callback(
        app,
        title,
        body,
        None,
        Some(true),
        session_id,
    )
}
