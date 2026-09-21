//! Generic HWND-tree diagnostics for plugin editor windows (spec Part 1/4).
//! No plugin/vendor hardcoding — diagnostics inspect structure, visibility, and
//! parent-chain only.

#[derive(Debug, Clone, Default)]
pub struct GpuEditorDiagnostics {
    pub child_count: u32,
    pub gpu_editor_detected: bool,
    pub parent_chain_ok: bool,
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::c_void;

    use super::GpuEditorDiagnostics;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, GetClientRect, GetParent, GetWindowLongPtrW,
        GetWindowThreadProcessId, IsWindow, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE,
    };

    fn hwnd_from(handle: u64) -> HWND {
        HWND(handle as *mut c_void)
    }

    struct EnumCtx {
        children: Vec<u64>,
    }

    unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut EnumCtx);
        if IsWindow(Some(hwnd)).as_bool() {
            ctx.children.push(hwnd.0 as u64);
        }
        BOOL(1)
    }

    fn parent_chain_ok(content_hwnd: u64, shell_hwnd: u64, host_hwnd: u64) -> bool {
        if content_hwnd == 0 || shell_hwnd == 0 {
            return false;
        }
        let content = hwnd_from(content_hwnd);
        let shell = hwnd_from(shell_hwnd);
        unsafe {
            if !IsWindow(Some(content)).as_bool() || !IsWindow(Some(shell)).as_bool() {
                return false;
            }
            if GetParent(content).ok() != Some(shell) {
                return false;
            }
            if host_hwnd != 0 {
                let host = hwnd_from(host_hwnd);
                if IsWindow(Some(host)).as_bool() && GetParent(host).ok() != Some(content) {
                    return false;
                }
            }
        }
        true
    }

    fn log_child(hwnd: u64, paint_count: u32, erase_count: u32, size_count: u32) {
        let h = hwnd_from(hwnd);
        unsafe {
            if !IsWindow(Some(h)).as_bool() {
                return;
            }
            let mut class_buf = [0u16; 256];
            let class_len = GetClassNameW(h, &mut class_buf);
            let class_name = if class_len > 0 {
                String::from_utf16_lossy(&class_buf[..class_len as usize])
            } else {
                String::from("<unknown>")
            };
            let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
            let ex_style = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            let mut pid: u32 = 0;
            let tid = GetWindowThreadProcessId(h, Some(&mut pid));
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            let w = rc.right - rc.left;
            let h_px = rc.bottom - rc.top;
            let visible = IsWindowVisible(h).as_bool() && w > 0 && h_px > 0;
            eprintln!(
                "[gpu-editor-diagnostics] child_hwnd=0x{hwnd:x} class={class_name} style=0x{style:08x} ex_style=0x{ex_style:08x} pid={pid} tid={tid}"
            );
            eprintln!(
                "[gpu-editor-diagnostics] paint_count={paint_count} erase_count={erase_count} size_count={size_count}"
            );
            eprintln!("[gpu-editor-diagnostics] visible={visible} rect={w}x{h_px}");
        }
    }

    pub fn log_gpu_editor_diagnostics(
        plugin_instance_id: &str,
        plugin_path: &str,
        shell_hwnd: u64,
        content_hwnd: u64,
        host_hwnd: u64,
        paint_count: u32,
        erase_count: u32,
        size_count: u32,
    ) -> GpuEditorDiagnostics {
        eprintln!("[gpu-editor-diagnostics] plugin_instance_id={plugin_instance_id}");
        eprintln!("[gpu-editor-diagnostics] plugin_path={plugin_path}");
        eprintln!("[gpu-editor-diagnostics] editor_ownership=main_owned");
        eprintln!(
            "[gpu-editor-diagnostics] shell_hwnd=0x{shell_hwnd:x} content_hwnd=0x{content_hwnd:x} host_hwnd=0x{host_hwnd:x}"
        );

        let mut ctx = EnumCtx {
            children: Vec::new(),
        };
        let root = if host_hwnd != 0 {
            host_hwnd
        } else {
            content_hwnd
        };
        unsafe {
            let _ = EnumChildWindows(
                Some(hwnd_from(root)),
                Some(enum_child),
                LPARAM(&mut ctx as *mut EnumCtx as isize),
            );
        }

        eprintln!(
            "[gpu-editor-diagnostics] child_count={}",
            ctx.children.len()
        );
        for child in &ctx.children {
            log_child(*child, paint_count, erase_count, size_count);
        }

        let chain_ok = parent_chain_ok(content_hwnd, shell_hwnd, host_hwnd);
        eprintln!("[gpu-editor-diagnostics] parent_chain_ok={chain_ok}");

        let self_pid = unsafe { GetCurrentProcessId() };
        eprintln!("[gpu-editor-diagnostics] main_process_pid={self_pid}");

        GpuEditorDiagnostics {
            child_count: ctx.children.len() as u32,
            gpu_editor_detected: false,
            parent_chain_ok: chain_ok,
        }
    }

    pub fn log_window_style_audit(shell_hwnd: u64, content_hwnd: u64, host_hwnd: u64) {
        const WS_EX_LAYERED: u32 = 0x0008_0000;
        fn ex_style(hwnd: u64) -> u32 {
            if hwnd == 0 {
                return 0;
            }
            unsafe { GetWindowLongPtrW(hwnd_from(hwnd), GWL_EXSTYLE) as u32 }
        }
        let shell_ex = ex_style(shell_hwnd);
        let content_ex = ex_style(content_hwnd);
        let host_ex = ex_style(host_hwnd);
        let layered = (shell_ex & WS_EX_LAYERED) != 0
            || (content_ex & WS_EX_LAYERED) != 0
            || (host_ex & WS_EX_LAYERED) != 0;
        eprintln!("[gpu-editor-window-style] shell_ex_style=0x{shell_ex:08x}");
        eprintln!("[gpu-editor-window-style] content_ex_style=0x{content_ex:08x}");
        eprintln!("[gpu-editor-window-style] host_ex_style=0x{host_ex:08x}");
        eprintln!("[gpu-editor-window-style] layered={layered}");
    }
}

#[cfg(target_os = "windows")]
pub use imp::{log_gpu_editor_diagnostics, log_window_style_audit};

#[cfg(not(target_os = "windows"))]
pub fn log_gpu_editor_diagnostics(
    _plugin_instance_id: &str,
    _plugin_path: &str,
    _shell_hwnd: u64,
    _content_hwnd: u64,
    _host_hwnd: u64,
    _paint_count: u32,
    _erase_count: u32,
    _size_count: u32,
) -> GpuEditorDiagnostics {
    GpuEditorDiagnostics::default()
}

#[cfg(not(target_os = "windows"))]
pub fn log_window_style_audit(_shell_hwnd: u64, _content_hwnd: u64, _host_hwnd: u64) {}
