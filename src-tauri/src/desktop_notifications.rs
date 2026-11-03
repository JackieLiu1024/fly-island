use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use std::{
    collections::HashMap,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use windows::{
    core::{HRESULT, HSTRING},
    UI::Notifications::{
        KnownNotificationBindings,
        Management::{UserNotificationListener, UserNotificationListenerAccessStatus},
        NotificationKinds, NotificationSetting, ToastNotificationManager, UserNotification,
    },
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

const MAX_ITEMS: usize = 20;
const REGISTRATION_REPAIR: &str =
    include_str!("../../scripts/repair-qq-notification-registration.ps1");
const DIAGNOSTIC_TITLE: &str = "FocuSD QQ 通知诊断";
const DIAGNOSTIC_BODY: &str = "这是本机通道测试，不是聊天消息。";
static SERVICE: OnceLock<Arc<NotificationService>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationSource {
    Qq,
    DingTalk,
}

const SOURCES: [NotificationSource; 2] = [NotificationSource::Qq, NotificationSource::DingTalk];

impl NotificationSource {
    fn index(self) -> usize {
        match self {
            Self::Qq => 0,
            Self::DingTalk => 1,
        }
    }

    fn event_name(self) -> &'static str {
        match self {
            Self::Qq => "qq-notifications-changed",
            Self::DingTalk => "dingtalk-notifications-changed",
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::Qq => "QQ",
            Self::DingTalk => "DingTalk",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Qq => "QQ",
            Self::DingTalk => "钉钉",
        }
    }

    /// The QQ desktop client registers toasts under the exact `QQ` id. DingTalk builds vary,
    /// so match any id that ends in a DingTalk executable name.
    fn matches_app_id(self, app_id: &str) -> bool {
        match self {
            Self::Qq => app_id == "QQ",
            Self::DingTalk => {
                let normalized = app_id.to_ascii_lowercase();
                let tail = normalized
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(normalized.as_str());
                tail.contains("dingtalk") || tail.contains("dingding")
            }
        }
    }

    fn probes_registration(self) -> bool {
        matches!(self, Self::Qq)
    }

    fn repair_script(self) -> Option<&'static str> {
        match self {
            Self::Qq => Some(REGISTRATION_REPAIR),
            Self::DingTalk => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopNotification {
    id: String,
    title: String,
    body: String,
    created_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NotificationStatus {
    Disabled,
    Waiting,
    Listening,
    PermissionDenied,
    PermissionRequired,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationSnapshot {
    revision: u64,
    enabled: bool,
    status: NotificationStatus,
    items: Vec<DesktopNotification>,
    error: Option<String>,
}

impl Default for NotificationSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: false,
            status: NotificationStatus::Disabled,
            items: Vec::new(),
            error: None,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationEvent {
    snapshot: NotificationSnapshot,
    newest_id: Option<String>,
    window_visible: bool,
}

#[derive(Default)]
struct NotificationBuffer {
    baseline: Option<HashMap<String, DesktopNotification>>,
    items: Vec<DesktopNotification>,
}

impl NotificationBuffer {
    fn sync(&mut self, mut current: Vec<DesktopNotification>) -> Option<String> {
        current.sort_by_key(|item| item.created_at);
        let mut newest = None;
        if let Some(previous) = &self.baseline {
            for item in &current {
                if previous.get(&item.id) != Some(item) {
                    self.items.retain(|existing| existing.id != item.id);
                    self.items.insert(0, item.clone());
                    self.items.truncate(MAX_ITEMS);
                    newest = Some(item.id.clone());
                }
            }
        }
        self.baseline = Some(
            current
                .into_iter()
                .map(|item| (item.id.clone(), item))
                .collect(),
        );
        newest
    }
}

struct ChannelState {
    snapshot: NotificationSnapshot,
    generation: u64,
    buffer: NotificationBuffer,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            snapshot: NotificationSnapshot::default(),
            generation: 0,
            buffer: NotificationBuffer::default(),
        }
    }
}

#[derive(Default)]
struct MonitorState {
    channels: [ChannelState; 2],
}

struct NotificationService {
    app: AppHandle,
    state: Mutex<MonitorState>,
    wake: mpsc::SyncSender<()>,
}

pub fn init(app: &AppHandle) -> Result<(), String> {
    if SERVICE.get().is_some() {
        return Ok(());
    }
    let (wake, receiver) = mpsc::sync_channel(1);
    let service = Arc::new(NotificationService {
        app: app.clone(),
        state: Mutex::new(MonitorState::default()),
        wake,
    });
    SERVICE
        .set(service.clone())
        .map_err(|_| "桌面通知服务已启动。".to_string())?;
    thread::Builder::new()
        .name("desktop-notifications".into())
        .spawn(move || monitor(service, receiver))
        .map_err(|error| format!("无法启动桌面通知监听：{error}"))?;
    Ok(())
}

fn service() -> Result<&'static Arc<NotificationService>, String> {
    SERVICE
        .get()
        .ok_or_else(|| "桌面通知服务尚未就绪。".to_string())
}

fn channel_state(source: NotificationSource) -> Result<NotificationSnapshot, String> {
    Ok(service()?
        .state
        .lock()
        .map_err(|_| "桌面通知状态不可用。")?
        .channels[source.index()]
        .snapshot
        .clone())
}

pub(crate) fn diagnostic_state(
    source: NotificationSource,
) -> Result<serde_json::Value, String> {
    let state = service()?
        .state
        .lock()
        .map_err(|_| "桌面通知状态不可用。")?;
    Ok(notification_diagnostics(&state.channels[source.index()].snapshot))
}

fn notification_diagnostics(snapshot: &NotificationSnapshot) -> serde_json::Value {
    serde_json::json!({
        "enabled": snapshot.enabled,
        "status": snapshot.status,
        "error": snapshot.error,
        "itemCount": snapshot.items.len(),
    })
}

fn set_enabled(source: NotificationSource, enabled: bool) -> Result<NotificationSnapshot, String> {
    let service = service()?;
    let snapshot = {
        let mut state = service
            .state
            .lock()
            .map_err(|_| "桌面通知状态不可用。")?;
        let channel = &mut state.channels[source.index()];
        if channel.snapshot.enabled == enabled {
            return Ok(channel.snapshot.clone());
        }
        channel.generation += 1;
        channel.buffer = NotificationBuffer::default();
        channel.snapshot = NotificationSnapshot {
            revision: channel.snapshot.revision + 1,
            enabled,
            status: if enabled {
                NotificationStatus::Waiting
            } else {
                NotificationStatus::Disabled
            },
            ..NotificationSnapshot::default()
        };
        channel.snapshot.clone()
    };
    publish(service, source, snapshot.clone(), None);
    let _ = service.wake.try_send(());
    Ok(snapshot)
}

fn clear(source: NotificationSource) -> Result<NotificationSnapshot, String> {
    let service = service()?;
    let snapshot = {
        let mut state = service
            .state
            .lock()
            .map_err(|_| "桌面通知状态不可用。")?;
        let channel = &mut state.channels[source.index()];
        channel.buffer.items.clear();
        channel.snapshot.items.clear();
        channel.snapshot.revision += 1;
        channel.snapshot.clone()
    };
    publish(service, source, snapshot.clone(), None);
    Ok(snapshot)
}

#[tauri::command]
pub fn get_qq_notification_state() -> Result<NotificationSnapshot, String> {
    channel_state(NotificationSource::Qq)
}

#[tauri::command]
pub fn set_qq_notifications_enabled(enabled: bool) -> Result<NotificationSnapshot, String> {
    set_enabled(NotificationSource::Qq, enabled)
}

#[tauri::command]
pub fn clear_qq_notifications() -> Result<NotificationSnapshot, String> {
    clear(NotificationSource::Qq)
}

#[tauri::command]
pub fn get_dingtalk_notification_state() -> Result<NotificationSnapshot, String> {
    channel_state(NotificationSource::DingTalk)
}

#[tauri::command]
pub fn set_dingtalk_notifications_enabled(enabled: bool) -> Result<NotificationSnapshot, String> {
    set_enabled(NotificationSource::DingTalk, enabled)
}

#[tauri::command]
pub fn clear_dingtalk_notifications() -> Result<NotificationSnapshot, String> {
    clear(NotificationSource::DingTalk)
}

async fn request_access(app: AppHandle) -> Result<String, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let operation = (|| {
            let listener = UserNotificationListener::Current()?;
            let access = listener.GetAccessStatus()?;
            let operation = if access == UserNotificationListenerAccessStatus::Unspecified {
                Some(listener.RequestAccessAsync()?)
            } else {
                None
            };
            windows::core::Result::Ok((access, operation))
        })();
        let _ = sender.send(operation);
    })
    .map_err(|error| format!("无法发起通知授权：{error}"))?;
    let (access, operation) =
        tauri::async_runtime::spawn_blocking(move || receiver.recv_timeout(Duration::from_secs(5)))
            .await
            .map_err(|error| format!("通知授权线程失败：{error}"))?
            .map_err(|error| format!("无法发起通知授权：{error}"))?
            .map_err(|error| format!("无法请求 Windows 通知权限：{error}"))?;
    let access = match operation {
        Some(operation) => operation
            .await
            .map_err(|error| format!("Windows 通知授权失败：{error}"))?,
        None => access,
    };
    let _ = service()?.wake.try_send(());
    Ok(match access {
        UserNotificationListenerAccessStatus::Allowed => "allowed",
        UserNotificationListenerAccessStatus::Denied => "denied",
        _ => "unspecified",
    }
    .to_string())
}

#[tauri::command]
pub async fn request_qq_notification_access(app: AppHandle) -> Result<String, String> {
    request_access(app).await
}

#[tauri::command]
pub async fn request_dingtalk_notification_access(app: AppHandle) -> Result<String, String> {
    request_access(app).await
}

fn open_settings(app: AppHandle, access: bool) -> Result<(), String> {
    let url = if access {
        "ms-settings:privacy-notifications"
    } else {
        "ms-settings:notifications"
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|error| format!("无法打开 Windows 通知设置：{error}"))
}

#[tauri::command]
pub fn open_qq_notification_settings(app: AppHandle, access: bool) -> Result<(), String> {
    open_settings(app, access)
}

#[tauri::command]
pub fn open_dingtalk_notification_settings(app: AppHandle, access: bool) -> Result<(), String> {
    open_settings(app, access)
}

fn publish(
    service: &NotificationService,
    source: NotificationSource,
    snapshot: NotificationSnapshot,
    newest_id: Option<String>,
) {
    let window_visible = service
        .app
        .get_webview_window("main")
        .is_some_and(|window| window.is_visible().unwrap_or(false));
    let _ = service.app.emit_to(
        "main",
        source.event_name(),
        NotificationEvent {
            snapshot,
            newest_id,
            window_visible,
        },
    );
}

fn monitor(service: Arc<NotificationService>, receiver: mpsc::Receiver<()>) {
    let mut repair_generation: [Option<u64>; 2] = [None, None];
    let mut repair_error: [Option<String>; 2] = [None, None];
    loop {
        let (enabled, generations) = match service.state.lock() {
            Ok(state) => (
                [
                    state.channels[0].snapshot.enabled,
                    state.channels[1].snapshot.enabled,
                ],
                [state.channels[0].generation, state.channels[1].generation],
            ),
            Err(_) => return,
        };
        if !enabled[0] && !enabled[1] {
            if receiver.recv().is_err() {
                return;
            }
            continue;
        }
        let (status, items, poll_error) = match read_notifications() {
            Ok((status, items)) => (status, items, None),
            Err(error) => (NotificationStatus::Error, Vec::new(), Some(error)),
        };
        let mut channel_errors = [None, None];
        if status == NotificationStatus::Listening {
            for source in SOURCES {
                let index = source.index();
                if !enabled[index] || !source.probes_registration() {
                    continue;
                }
                let setting = notification_setting(source);
                if repair_generation[index] != Some(generations[index]) {
                    repair_error[index] = None;
                    if should_repair_registration(&setting) {
                        repair_generation[index] = Some(generations[index]);
                        repair_error[index] = repair_registration(service.as_ref(), source, generations[index]).err();
                    }
                }
                let setting = notification_setting(source);
                if matches!(setting, Ok(NotificationSetting::Enabled)) {
                    repair_error[index] = None;
                }
                channel_errors[index] = repair_error[index]
                    .clone()
                    .or_else(|| registration_error(setting, source));
            }
        }
        let changes = {
            let Ok(mut state) = service.state.lock() else {
                return;
            };
            let mut changes = Vec::new();
            for source in SOURCES {
                let index = source.index();
                let channel = &mut state.channels[index];
                if channel.generation != generations[index] || !channel.snapshot.enabled {
                    continue;
                }
                let before = channel.snapshot.clone();
                let newest = if status == NotificationStatus::Listening {
                    channel.snapshot.status = NotificationStatus::Listening;
                    channel.snapshot.error = channel_errors[index].clone();
                    let current = items
                        .iter()
                        .filter(|(item_source, _)| *item_source == source)
                        .map(|(_, item)| item.clone())
                        .collect();
                    let newest = channel.buffer.sync(current);
                    channel.snapshot.items = channel.buffer.items.clone();
                    newest
                } else {
                    channel.buffer = NotificationBuffer::default();
                    channel.snapshot.items.clear();
                    channel.snapshot.status = status;
                    channel.snapshot.error = poll_error.clone();
                    None
                };
                if channel.snapshot != before {
                    channel.snapshot.revision += 1;
                    changes.push((source, channel.snapshot.clone(), newest));
                }
            }
            changes
        };
        for (source, snapshot, newest) in changes {
            publish(service.as_ref(), source, snapshot, newest);
        }
        if receiver.recv_timeout(Duration::from_secs(1))
            == Err(mpsc::RecvTimeoutError::Disconnected)
        {
            return;
        }
    }
}

fn notification_setting(source: NotificationSource) -> windows::core::Result<NotificationSetting> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
    let _apartment = WinRT;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(source.app_id()))?.Setting()
}

fn should_repair_registration(setting: &windows::core::Result<NotificationSetting>) -> bool {
    setting
        .as_ref()
        .is_err_and(|error| error.code() == HRESULT(0x80070490u32 as i32))
}

fn registration_error(
    setting: windows::core::Result<NotificationSetting>,
    source: NotificationSource,
) -> Option<String> {
    let name = source.display_name();
    match setting {
        Ok(NotificationSetting::Enabled) => None,
        Ok(NotificationSetting::DisabledForApplication) => {
            Some(format!("{name} 的 Windows 通知已关闭，请在系统通知设置中开启。"))
        }
        Ok(NotificationSetting::DisabledForUser) => {
            Some("Windows 系统通知已关闭，请在系统通知设置中开启。".into())
        }
        Ok(NotificationSetting::DisabledByGroupPolicy) => {
            Some("管理员策略禁止了 Windows 通知，请联系管理员。".into())
        }
        Ok(_) => Some(format!("{name} 当前安装方式不支持系统通知，请检查或更新 {name}。")),
        Err(error) => Some(format!(
            "{name} 通知通道尚未就绪，请确认已安装 {name}；可关闭后重新开启接收以重试：{error}"
        )),
    }
}

fn repair_registration(
    service: &NotificationService,
    source: NotificationSource,
    generation: u64,
) -> Result<(), String> {
    // Embed the existing guarded repair so installed builds do not depend on the repository.
    let script = format!(
        "& {{\n{}\n}} -Repair",
        source
            .repair_script()
            .ok_or_else(|| "该消息来源不支持通知注册修复。".to_string())?
    );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let system_root = std::env::var_os("SystemRoot").ok_or("无法定位 Windows 通知修复工具。")?;
    let executable =
        PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(bytes),
        ])
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("无法启动通知自动修复：{error}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err("通知自动修复未完成，请确认客户端安装正常；可关闭后重新开启接收以重试。"
                        .into())
                }
            }
            Ok(None)
                if Instant::now() < deadline
                    && service.state.lock().is_ok_and(|state| {
                        let channel = &state.channels[source.index()];
                        channel.snapshot.enabled && channel.generation == generation
                    }) =>
            {
                thread::sleep(Duration::from_millis(50))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => format!("通知自动修复失败：{error}"),
                    _ => "通知自动修复已停止或超时，可关闭后重新开启接收以重试。".into(),
                });
            }
        }
    }
}

/// Adds `app_id()` to the source so the toast registration probe can stay source-agnostic.
impl NotificationSource {
    fn app_id(self) -> &'static str {
        match self {
            Self::Qq => "QQ",
            Self::DingTalk => "DingTalk",
        }
    }
}

struct WinRT;
impl Drop for WinRT {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn read_notifications(
) -> Result<(NotificationStatus, Vec<(NotificationSource, DesktopNotification)>), String> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
        .map_err(|error| format!("无法初始化 Windows 通知：{error}"))?;
    let _apartment = WinRT;
    let listener = UserNotificationListener::Current()
        .map_err(|error| format!("Windows 通知监听不可用：{error}"))?;
    let access = listener
        .GetAccessStatus()
        .map_err(|error| format!("无法检查通知权限：{error}"))?;
    if access != UserNotificationListenerAccessStatus::Allowed {
        return Ok((
            if access == UserNotificationListenerAccessStatus::Denied {
                NotificationStatus::PermissionDenied
            } else {
                NotificationStatus::PermissionRequired
            },
            Vec::new(),
        ));
    }
    let operation = listener
        .GetNotificationsAsync(NotificationKinds::Toast)
        .map_err(|error| format!("无法读取 Windows 通知：{error}"))?;
    let notifications =
        pollster::block_on(operation).map_err(|error| format!("无法读取 Windows 通知：{error}"))?;
    let mut items = Vec::new();
    for notification in notifications {
        if let Ok(Some(item)) = read_notification(&notification) {
            items.push(item);
        }
    }
    Ok((NotificationStatus::Listening, items))
}

fn read_notification(
    notification: &UserNotification,
) -> windows::core::Result<Option<(NotificationSource, DesktopNotification)>> {
    let app_id = notification.AppInfo()?.AppUserModelId()?;
    let app_id = app_id.to_string_lossy();
    let Some(source) = SOURCES
        .iter()
        .copied()
        .find(|source| source.matches_app_id(&app_id))
    else {
        return Ok(None);
    };
    let id = notification.Id()?;
    let ticks = notification.CreationTime()?.UniversalTime;
    let mut parts = Vec::new();
    if let Ok(elements) = notification
        .Notification()?
        .Visual()?
        .GetBinding(&KnownNotificationBindings::ToastGeneric()?)
        .and_then(|binding| binding.GetTextElements())
    {
        for element in elements {
            if let Ok(text) = element.Text() {
                parts.push(text.to_string_lossy());
            }
        }
    }
    if parts.first().is_some_and(|title| title == DIAGNOSTIC_TITLE)
        && parts.iter().any(|body| body == DIAGNOSTIC_BODY)
    {
        return Ok(None);
    }
    if parts
        .first()
        .is_some_and(|title| title == "FocuSD QQ notification diagnostic")
        && parts
            .iter()
            .any(|body| body == "Local channel test; this is not a chat message.")
    {
        return Ok(None);
    }
    let title = parts
        .first()
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| source.display_name().into());
    let body = parts.iter().skip(1).cloned().collect::<Vec<_>>().join("\n");
    Ok(Some((
        source,
        DesktopNotification {
            id: format!("{}:{id}:{ticks}", source.id_prefix()),
            title: title.chars().take(120).collect(),
            body: body.chars().take(1000).collect(),
            created_at: ticks / 10_000 - 11_644_473_600_000,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_reports_never_include_chat_content() {
        let snapshot = NotificationSnapshot {
            items: vec![DesktopNotification {
                id: "private-id".into(),
                title: "private-sender".into(),
                body: "private-message".into(),
                created_at: 123,
            }],
            ..Default::default()
        };
        let report = notification_diagnostics(&snapshot);
        assert_eq!(report["itemCount"], 1);
        assert!(!report.to_string().contains("private-"));
        assert!(report.get("items").is_none());
    }

    #[test]
    fn dingtalk_toast_ids_are_recognized_without_matching_other_apps() {
        let source = NotificationSource::DingTalk;
        for app_id in [
            "DingTalk",
            "DingTalk.exe",
            "com.alibaba.DingTalk",
            "C:\\Program Files\\DingTalk\\DingTalk.exe",
        ] {
            assert!(source.matches_app_id(app_id), "{app_id} must match");
        }
        for app_id in ["QQ", "WeChat.exe", "Slack.exe", "DingxHelper"] {
            assert!(!source.matches_app_id(app_id), "{app_id} must not match");
        }
        assert!(NotificationSource::Qq.matches_app_id("QQ"));
        assert!(!NotificationSource::Qq.matches_app_id("DingTalk"));
    }

    #[test]
    fn registration_repair_only_handles_missing_sender_records() {
        assert!(should_repair_registration(&Err(
            windows::core::Error::from_hresult(HRESULT(0x80070490u32 as i32),)
        )));
        assert!(!should_repair_registration(&Err(
            windows::core::Error::from_hresult(HRESULT(0x80070005u32 as i32),)
        )));
        for setting in [
            NotificationSetting::Enabled,
            NotificationSetting::DisabledForApplication,
            NotificationSetting::DisabledForUser,
            NotificationSetting::DisabledByGroupPolicy,
            NotificationSetting::DisabledByManifest,
        ] {
            assert!(!should_repair_registration(&Ok(setting)));
        }
    }

    #[test]
    fn blocked_notification_channels_report_the_actionable_cause() {
        assert!(registration_error(Ok(NotificationSetting::Enabled), NotificationSource::Qq).is_none());
        assert!(
            registration_error(
                Ok(NotificationSetting::DisabledForApplication),
                NotificationSource::Qq
            )
            .unwrap()
            .contains("QQ 的 Windows 通知已关闭")
        );
        assert!(registration_error(
            Ok(NotificationSetting::DisabledForUser),
            NotificationSource::Qq
        )
        .unwrap()
        .contains("Windows 系统通知已关闭"));
        assert!(
            registration_error(
                Ok(NotificationSetting::DisabledByGroupPolicy),
                NotificationSource::Qq
            )
            .unwrap()
            .contains("管理员")
        );
    }

    fn notice(id: u32, body: &str) -> DesktopNotification {
        DesktopNotification {
            id: id.to_string(),
            title: "QQ".into(),
            body: body.into(),
            created_at: id as i64,
        }
    }
    #[test]
    fn startup_notifications_do_not_replay_and_new_messages_are_deduplicated() {
        let mut buffer = NotificationBuffer::default();
        assert_eq!(buffer.sync(vec![notice(1, "existing")]), None);
        assert!(buffer.items.is_empty());
        let current = vec![notice(1, "existing"), notice(2, "new")];
        assert_eq!(buffer.sync(current.clone()), Some("2".into()));
        assert_eq!(buffer.sync(current), None);
        assert_eq!(buffer.items, vec![notice(2, "new")]);
    }
    #[test]
    fn updated_notifications_replace_content_without_increasing_count() {
        let mut buffer = NotificationBuffer::default();
        buffer.sync(vec![]);
        buffer.sync(vec![notice(1, "first")]);
        assert_eq!(buffer.sync(vec![notice(1, "updated")]), Some("1".into()));
        assert_eq!(buffer.items, vec![notice(1, "updated")]);
    }
    #[test]
    fn clearing_history_does_not_redisplay_unchanged_system_notifications() {
        let mut buffer = NotificationBuffer::default();
        buffer.sync(vec![]);
        let current = vec![notice(1, "new")];
        buffer.sync(current.clone());
        buffer.items.clear();
        assert_eq!(buffer.sync(current), None);
        assert!(buffer.items.is_empty());
    }
    #[test]
    fn notification_history_is_bounded_and_keeps_latest_first() {
        let mut buffer = NotificationBuffer::default();
        buffer.sync(vec![]);
        buffer.sync((1..=25).map(|id| notice(id, "new")).collect());
        assert_eq!(buffer.items.len(), MAX_ITEMS);
        assert_eq!(buffer.items.first().unwrap().id, "25");
        assert_eq!(buffer.items.last().unwrap().id, "6");
    }

    #[test]
    #[ignore = "Requires a real QQ system notification on this Windows account"]
    fn reads_a_real_qq_system_notification() {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let (status, items) =
                read_notifications().expect("Windows notifications must be readable");
            assert_eq!(status, NotificationStatus::Listening);
            let qq_items: Vec<_> = items
                .iter()
                .filter(|(source, _)| *source == NotificationSource::Qq)
                .collect();
            if !qq_items.is_empty() {
                assert!(qq_items
                    .iter()
                    .all(|(_, item)| item.id.starts_with("QQ:") && !item.title.is_empty()));
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Receive a real QQ notification during this test"
            );
            thread::sleep(Duration::from_secs(1));
        }
    }
}
