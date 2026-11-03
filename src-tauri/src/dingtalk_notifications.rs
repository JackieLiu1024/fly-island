use crate::messenger_activation::activate_existing_window;
use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{BOOL, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM},
        System::{
            Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ},
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetForegroundWindow, GetWindow, GetWindowPlacement,
            GetWindowThreadProcessId, IsWindowVisible, GW_OWNER, WINDOWPLACEMENT,
        },
    },
};

const DISPLAY_NAME: &str = "钉钉";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn has_dingtalk_executable_name(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy().to_ascii_lowercase();
        name.contains("dingtalk") || name.contains("dingding")
    })
}

fn is_dingtalk_executable(path: &Path) -> bool {
    path.is_absolute() && path.is_file() && has_dingtalk_executable_name(path)
}

fn app_paths_executable() -> Option<PathBuf> {
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for executable in ["DingTalk.exe", "DingtalkLauncher.exe", "DingDing.exe"] {
            let key = wide(&format!(
                "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{executable}"
            ));
            let mut data = [0u16; 2048];
            let mut bytes = (data.len() * 2) as u32;
            let result = unsafe {
                RegGetValueW(
                    root,
                    PCWSTR(key.as_ptr()),
                    PCWSTR::null(),
                    RRF_RT_REG_SZ,
                    None,
                    Some(data.as_mut_ptr().cast()),
                    Some(&mut bytes),
                )
            };
            if result.is_ok() {
                let length = data.iter().position(|ch| *ch == 0).unwrap_or(data.len());
                let value = String::from_utf16_lossy(&data[..length]);
                let path = PathBuf::from(value.trim().trim_matches('"'));
                if is_dingtalk_executable(&path) {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn common_location_executable() -> Option<PathBuf> {
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        let Some(root) = std::env::var_os(variable) else {
            continue;
        };
        for relative in [
            "DingTalk/DingTalk.exe",
            "DingTalk/DingtalkLauncher.exe",
            "DingDing/DingtalkLauncher.exe",
            "Programs/DingTalk/DingTalk.exe",
        ] {
            let path = PathBuf::from(&root).join(relative);
            if is_dingtalk_executable(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn dingtalk_executable() -> Result<PathBuf, String> {
    app_paths_executable()
        .or_else(common_location_executable)
        .ok_or_else(|| "未找到钉钉安装路径，请确认已安装 Windows 版钉钉。".into())
}

struct DingTalkWindowSearch<'a> {
    executable: &'a Path,
    windows: Vec<DingTalkWindow>,
    running: bool,
}

struct DingTalkWindow {
    handle: HWND,
    visible: bool,
    area: i64,
}

fn preferred_dingtalk_window(windows: &[DingTalkWindow], foreground: HWND) -> Option<&DingTalkWindow> {
    windows.iter().max_by_key(|window| {
        (
            window.visible && window.handle == foreground,
            window.visible,
            window.area,
        )
    })
}

unsafe extern "system" fn find_dingtalk_window(window: HWND, context: LPARAM) -> BOOL {
    let search = &mut *(context.0 as *mut DingTalkWindowSearch<'_>);
    let mut process_id = 0;
    GetWindowThreadProcessId(window, Some(&mut process_id));
    if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
        let mut path = [0u16; 32768];
        let mut length = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        );
        let matches = result.is_ok()
            && String::from_utf16_lossy(&path[..length as usize])
                .eq_ignore_ascii_case(&search.executable.to_string_lossy());
        let _ = CloseHandle(process);
        if !matches {
            return true.into();
        }
        search.running = true;
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindow(window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
            || GetWindowPlacement(window, &mut placement).is_err()
        {
            return true.into();
        }
        let width = placement.rcNormalPosition.right - placement.rcNormalPosition.left;
        let height = placement.rcNormalPosition.bottom - placement.rcNormalPosition.top;
        if width >= 500 && height >= 300 {
            search.windows.push(DingTalkWindow {
                handle: window,
                visible: IsWindowVisible(window).as_bool(),
                area: i64::from(width) * i64::from(height),
            });
        }
    }
    true.into()
}

fn existing_dingtalk_windows(executable: &Path) -> Result<DingTalkWindowSearch<'_>, String> {
    let mut search = DingTalkWindowSearch {
        executable,
        windows: Vec::new(),
        running: false,
    };
    unsafe {
        EnumWindows(
            Some(find_dingtalk_window),
            LPARAM((&mut search as *mut DingTalkWindowSearch<'_>) as isize),
        )
        .map_err(|error| format!("无法查找已运行的钉钉窗口：{error}"))?;
    }
    Ok(search)
}

fn activate_dingtalk() -> Result<(), String> {
    let executable = dingtalk_executable()?;
    let search = existing_dingtalk_windows(&executable)?;
    if let Some(window) = preferred_dingtalk_window(&search.windows, unsafe { GetForegroundWindow() })
    {
        return activate_existing_window(window.handle, DISPLAY_NAME);
    }
    if search.running {
        return Err("钉钉已在运行，但未找到可用的主窗口，请从系统托盘打开钉钉。".into());
    }
    Command::new(&executable)
        .spawn()
        .map_err(|error| format!("无法启动钉钉：{error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
        let current = existing_dingtalk_windows(&executable)?;
        if let Some(window) =
            preferred_dingtalk_window(&current.windows, unsafe { GetForegroundWindow() })
        {
            return activate_existing_window(window.handle, DISPLAY_NAME);
        }
    }
    Err("钉钉没有在 10 秒内恢复主窗口，请从开始菜单打开钉钉。".into())
}

#[tauri::command]
pub async fn open_dingtalk() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(activate_dingtalk)
        .await
        .map_err(|error| format!("打开钉钉的线程失败：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_dingtalk_window_is_preferred() {
        let windows = [
            DingTalkWindow {
                handle: HWND::default(),
                visible: false,
                area: 2_000_000,
            },
            DingTalkWindow {
                handle: HWND::default(),
                visible: true,
                area: 800_000,
            },
        ];
        assert_eq!(
            preferred_dingtalk_window(&windows, HWND::default())
                .unwrap()
                .area,
            800_000
        );
    }

    #[test]
    fn only_executables_named_after_dingtalk_are_accepted() {
        assert!(has_dingtalk_executable_name(Path::new(
            "C:\\Program Files\\DingTalk\\DingTalk.exe"
        )));
        assert!(has_dingtalk_executable_name(Path::new(
            "C:\\DingDing\\DingtalkLauncher.exe"
        )));
        assert!(!has_dingtalk_executable_name(Path::new(
            "C:\\Program Files\\Other\\Other.exe"
        )));
    }
}
