use crate::messenger_activation::{activate_existing_window, is_interactive_window};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{w, BOOL, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, WPARAM},
        System::{
            Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ},
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::{
            Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER},
            WindowsAndMessaging::{
                AllowSetForegroundWindow, EnumWindows, GetClassNameW, GetForegroundWindow,
                GetWindow, GetWindowPlacement, GetWindowTextW, GetWindowThreadProcessId,
                IsWindowVisible, SendMessageTimeoutW, GW_OWNER, SMTO_ABORTIFHUNG, SMTO_BLOCK,
                SMTO_ERRORONEXIT, WINDOWPLACEMENT, WM_APP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
                WM_LBUTTONUP,
            },
        },
    },
};

fn qq_executable() -> Result<PathBuf, String> {
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for key in [
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\QQ"),
            w!("Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\QQ"),
        ] {
            let mut data = [0u16; 2048];
            let mut bytes = (data.len() * 2) as u32;
            let result = unsafe {
                RegGetValueW(
                    root,
                    key,
                    w!("DisplayIcon"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(data.as_mut_ptr().cast()),
                    Some(&mut bytes),
                )
            };
            if result.is_ok() {
                let length = data.iter().position(|ch| *ch == 0).unwrap_or(data.len());
                let icon = String::from_utf16_lossy(&data[..length]);
                let executable = icon
                    .rsplit_once(',')
                    .filter(|(_, index)| index.trim().parse::<i32>().is_ok())
                    .map_or(icon.as_str(), |(path, _)| path)
                    .trim()
                    .trim_matches('"');
                let path = PathBuf::from(executable);
                if path.is_absolute()
                    && path.is_file()
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("QQ.exe"))
                {
                    return Ok(path);
                }
            }
        }
    }
    Err("未找到 QQ 安装路径，请确认已安装 Windows 版 QQ。".into())
}

struct QQWindowSearch<'a> {
    executable: &'a Path,
    windows: Vec<QQWindow>,
    tray_windows: Vec<(HWND, u32)>,
    running: bool,
}

struct QQWindow {
    handle: HWND,
    process_id: u32,
    visible: bool,
    area: i64,
}

fn preferred_qq_window(windows: &[QQWindow], foreground: HWND) -> Option<&QQWindow> {
    windows.iter().max_by_key(|window| {
        (
            window.visible && window.handle == foreground,
            window.visible,
            window.area,
        )
    })
}

unsafe extern "system" fn find_qq_window(window: HWND, context: LPARAM) -> BOOL {
    let search = &mut *(context.0 as *mut QQWindowSearch<'_>);
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
        let mut title = [0u16; 32];
        let length = GetWindowTextW(window, &mut title) as usize;
        let mut class = [0u16; 256];
        let class_length = GetClassNameW(window, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..class_length]);
        if class == "Electron_NotifyIconHostWindow" {
            search.tray_windows.push((window, process_id));
            return true.into();
        }
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if String::from_utf16_lossy(&title[..length]) != "QQ"
            || !["Chrome_WidgetWin_1", "TXGuiFoundation"].contains(&class.as_str())
            || GetWindow(window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
            || GetWindowPlacement(window, &mut placement).is_err()
        {
            return true.into();
        }
        let width = placement.rcNormalPosition.right - placement.rcNormalPosition.left;
        let height = placement.rcNormalPosition.bottom - placement.rcNormalPosition.top;
        if width >= 500 && height >= 300 {
            search.windows.push(QQWindow {
                handle: window,
                process_id,
                visible: IsWindowVisible(window).as_bool(),
                area: i64::from(width) * i64::from(height),
            });
        }
    }
    true.into()
}

fn existing_qq_windows(executable: &Path) -> Result<QQWindowSearch<'_>, String> {
    let mut search = QQWindowSearch {
        executable,
        windows: Vec::new(),
        tray_windows: Vec::new(),
        running: false,
    };
    unsafe {
        EnumWindows(
            Some(find_qq_window),
            LPARAM((&mut search as *mut QQWindowSearch<'_>) as isize),
        )
        .map_err(|error| format!("无法查找已运行的 QQ 窗口：{error}"))?;
    }
    Ok(search)
}

fn activate_existing_qq(executable: &Path) -> Result<(), String> {
    let search = existing_qq_windows(executable)?;
    let original = preferred_qq_window(&search.windows, unsafe { GetForegroundWindow() })
        .ok_or_else(|| {
            if search.running {
                "QQ 已在运行，但未找到可用的主窗口，请从系统托盘打开 QQ。"
            } else {
                "未找到正在运行的 QQ 窗口，请先打开并登录桌面版 QQ。"
            }
            .to_string()
        })?;
    let expected_process = original.process_id;
    unsafe {
        let _ = AllowSetForegroundWindow(expected_process);
    }
    let mut trays = search
        .tray_windows
        .iter()
        .filter(|(_, process_id)| *process_id == expected_process);
    let tray = trays
        .next()
        .ok_or("未找到 QQ 的托盘唤回入口，请双击系统托盘中的 QQ 图标。")?;
    if trays.next().is_some() {
        return Err("QQ 有多个托盘入口，无法确认主入口，请从系统托盘打开 QQ。".into());
    }
    // Electron allocates icon ids from 2 and uses WM_APP + 1. Resolve the live id from the shell,
    // since QQ may recreate its tray icon after login. Never invoke an ambiguous icon.
    // https://github.com/electron/electron/blob/main/shell/browser/ui/win/notify_icon_host.cc
    let mut icon_id = None;
    let lookup_deadline = Instant::now() + Duration::from_secs(1);
    for id in 2..=256 {
        if Instant::now() >= lookup_deadline {
            return Err("查询 QQ 托盘图标超时，请从系统托盘打开 QQ。".into());
        }
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: tray.0,
            uID: id,
            ..Default::default()
        };
        if unsafe { Shell_NotifyIconGetRect(&identifier) }.is_ok() {
            if icon_id.replace(id).is_some() {
                return Err(
                    "QQ 有多个托盘图标，无法确认主入口，请双击系统托盘中的 QQ 图标。".into(),
                );
            }
        }
    }
    let icon_id = icon_id.ok_or("QQ 托盘图标尚未就绪，请从系统托盘打开 QQ。")?;
    unsafe {
        for message in [WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDBLCLK, WM_LBUTTONUP] {
            if SendMessageTimeoutW(
                tray.0,
                WM_APP + 1,
                WPARAM(icon_id as usize),
                LPARAM(message as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                1_000,
                None,
            )
            .0 == 0
            {
                return Err("QQ 没有响应托盘唤回请求，请从系统托盘打开。".into());
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let current = existing_qq_windows(executable)?;
        if let Some(window) =
            preferred_qq_window(&current.windows, unsafe { GetForegroundWindow() })
        {
            if window.process_id == expected_process && is_interactive_window(window.handle) {
                return activate_existing_window(window.handle, "QQ");
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("QQ 未能恢复可交互的主窗口，请双击系统托盘中的 QQ 图标。".into())
}

#[tauri::command]
pub async fn open_qq() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| activate_existing_qq(&qq_executable()?))
        .await
        .map_err(|error| format!("打开 QQ 的线程失败：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;

    #[test]
    fn visible_qq_window_is_preferred_but_tray_hidden_main_can_be_reused() {
        let windows = [
            QQWindow {
                handle: HWND::default(),
                process_id: 1,
                visible: false,
                area: 2_000_000,
            },
            QQWindow {
                handle: HWND::default(),
                process_id: 1,
                visible: true,
                area: 800_000,
            },
        ];
        assert_eq!(
            preferred_qq_window(&windows, HWND::default()).unwrap().area,
            800_000
        );
        assert_eq!(
            preferred_qq_window(&windows[..1], HWND::default())
                .unwrap()
                .area,
            2_000_000
        );
    }

    #[test]
    fn foreground_main_is_preferred_over_a_larger_preloaded_window() {
        let windows = [
            QQWindow {
                handle: HWND(1usize as _),
                process_id: 1,
                visible: true,
                area: 2_000_000,
            },
            QQWindow {
                handle: HWND(2usize as _),
                process_id: 1,
                visible: true,
                area: 800_000,
            },
        ];
        assert_eq!(
            preferred_qq_window(&windows, windows[1].handle)
                .unwrap()
                .handle,
            windows[1].handle
        );
    }

    #[test]
    #[ignore = "Invokes QQ's tray double-click handler and verifies the original main process"]
    fn window_activation_reuses_the_existing_qq_process() {
        let executable = qq_executable().unwrap();
        let before = existing_qq_windows(&executable).unwrap();
        let original = preferred_qq_window(&before.windows, unsafe { GetForegroundWindow() })
            .expect("QQ main window is required");
        let mut expected_process = 0;
        unsafe {
            GetWindowThreadProcessId(original.handle, Some(&mut expected_process));
        }
        assert_ne!(expected_process, 0);
        for _ in 0..2 {
            let result = activate_existing_qq(&executable);
            assert!(
                result
                    .as_ref()
                    .err()
                    .is_none_or(|error| error.contains("未能切换到前台")),
                "{result:?}"
            );
            let after = existing_qq_windows(&executable).unwrap();
            let window = preferred_qq_window(&after.windows, unsafe { GetForegroundWindow() })
                .expect("The existing main window must be visible");
            let mut process_id = 0;
            unsafe {
                GetWindowThreadProcessId(window.handle, Some(&mut process_id));
            }
            assert_eq!(process_id, expected_process);
            assert!(window.visible);
            assert!(!unsafe { IsIconic(window.handle) }.as_bool());
            assert!(is_interactive_window(window.handle));
            // A background test runner may be denied focus. That must never launch another QQ.
            if let Err(error) = result {
                assert!(error.contains("未能切换到前台"), "{error}");
                assert_ne!(
                    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() },
                    window.handle
                );
            }
        }
    }

    #[test]
    #[ignore = "Reads Windows taskbar accessibility metadata; requires a restored QQ main window"]
    fn restored_qq_has_a_taskbar_button() {
        use windows::core::Interface;
        use windows::Win32::{
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
                COINIT_MULTITHREADED,
            },
            UI::{
                Accessibility::{
                    CUIAutomation8, IUIAutomation, IUIAutomation2, TreeScope_Descendants,
                },
                WindowsAndMessaging::{EnumChildWindows, FindWindowW},
            },
        };
        unsafe extern "system" fn collect_taskbar_children(window: HWND, context: LPARAM) -> BOOL {
            let children = &mut *(context.0 as *mut Vec<HWND>);
            let mut class = [0u16; 256];
            let length = GetClassNameW(window, &mut class) as usize;
            if [
                "MSTaskListWClass",
                "Windows.UI.Composition.DesktopWindowContentBridge",
            ]
            .contains(&String::from_utf16_lossy(&class[..length]).as_str())
            {
                children.push(window);
            }
            true.into()
        }
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let result = (|| -> windows::core::Result<usize> {
                let automation: IUIAutomation =
                    CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
                let options = automation.cast::<IUIAutomation2>()?;
                options.SetConnectionTimeout(1_000)?;
                options.SetTransactionTimeout(1_000)?;
                let taskbar_handle = FindWindowW(w!("Shell_TrayWnd"), None)?;
                let mut roots = vec![taskbar_handle];
                let _ = EnumChildWindows(
                    Some(taskbar_handle),
                    Some(collect_taskbar_children),
                    LPARAM((&mut roots as *mut Vec<HWND>) as isize),
                );
                let mut count = 0;
                for root in roots {
                    let taskbar = automation.ElementFromHandle(root)?;
                    let elements = taskbar
                        .FindAll(TreeScope_Descendants, &automation.CreateTrueCondition()?)?;
                    for index in 0..elements.Length()? {
                        let element = elements.GetElement(index)?;
                        let class = element.CurrentClassName()?.to_string();
                        if class.contains("TaskListButton")
                            && element.CurrentName()?.to_string().starts_with("QQ")
                        {
                            count += 1;
                        }
                    }
                }
                Ok(count)
            })();
            CoUninitialize();
            assert!(
                result.unwrap() > 0,
                "QQ must have an application button on the primary taskbar"
            );
        }
    }
}
