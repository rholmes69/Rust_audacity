//! Windows: a plain top-level window that hosts a plugin's own editor (VST3 IPlugView or
//! an embedded CLAP GUI). The window lives on the UI thread, whose message loop (run by
//! winit) also dispatches its messages.

use std::{
    collections::HashMap,
    ffi::c_void,
    ptr,
    sync::{Mutex, OnceLock},
};

use anyhow::{Result, anyhow, bail};
use vst3::{
    Class, ComPtr, ComRef, ComWrapper,
    Steinberg::{
        IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect, kPlatformTypeHWND,
        kResultOk, tresult,
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
        DestroyWindow, GWL_STYLE, GetClientRect, GetWindowLongW, IDC_ARROW, LoadCursorW,
        RegisterClassW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
        ShowWindow, WM_CLOSE, WM_SIZE, WNDCLASSW, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
        WS_OVERLAPPED, WS_SYSMENU, WS_THICKFRAME,
    },
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows the user closed, and new client sizes after the user resized (by HWND).
#[derive(Default)]
struct Events {
    closed: Vec<isize>,
    resized: HashMap<isize, (i32, i32)>,
}

static EVENTS: Mutex<Option<Events>> = Mutex::new(None);

fn with_events<R>(f: impl FnOnce(&mut Events) -> R) -> R {
    let mut g = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Events::default))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            // Hide now; the owner tears the editor down on its next idle tick.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            with_events(|e| e.closed.push(hwnd as isize));
            0
        }
        WM_SIZE => {
            let w = (lparam & 0xFFFF) as i32;
            let h = ((lparam >> 16) & 0xFFFF) as i32;
            if w > 0 && h > 0 {
                with_events(|e| {
                    e.resized.insert(hwnd as isize, (w, h));
                });
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn class_name() -> &'static [u16] {
    static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    CLASS.get_or_init(|| {
        let name = wide("RustAudacityPluginEditor");
        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(ptr::null()),
                hIcon: ptr::null_mut(),
                hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            RegisterClassW(&wc);
        }
        name
    })
}

/// Lets a VST3 editor ask for a new size.
struct PlugFrame {
    hwnd: isize,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        unsafe {
            if new_size.is_null() {
                return kResultOk;
            }
            let r = *new_size;
            resize_client(self.hwnd as HWND, r.right - r.left, r.bottom - r.top);
            // We caused this size change; don't report it back as a user resize.
            with_events(|e| {
                e.resized.remove(&self.hwnd);
            });
            if let Some(v) = ComRef::from_raw(view) {
                v.onSize(new_size);
            }
        }
        kResultOk
    }
}

unsafe fn resize_client(hwnd: HWND, w: i32, h: i32) {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w.max(50),
            bottom: h.max(50),
        };
        AdjustWindowRectEx(&mut r, style, 0, 0);
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            0,
            0,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub struct Editor {
    hwnd: HWND,
    vst3: Option<(ComPtr<IPlugView>, ComWrapper<PlugFrame>)>,
}

impl Editor {
    /// An empty window with a client area of `w`×`h` for a plugin to draw into.
    pub fn open_window(title: &str, w: i32, h: i32, resizable: bool) -> Result<Self> {
        let class = class_name();
        let mut style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        if resizable {
            style |= WS_THICKFRAME | WS_MAXIMIZEBOX;
        }
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w.max(50),
            bottom: h.max(50),
        };
        let title = wide(title);
        let hwnd = unsafe {
            AdjustWindowRectEx(&mut r, style, 0, 0);
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                r.right - r.left,
                r.bottom - r.top,
                ptr::null_mut(),
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            bail!("couldn't create a window for the plugin");
        }
        unsafe { ShowWindow(hwnd, SW_SHOW) };
        Ok(Self { hwnd, vst3: None })
    }

    pub fn open_vst3(view: ComPtr<IPlugView>, title: &str) -> Result<Self> {
        unsafe {
            if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultOk {
                bail!("this plugin's window isn't supported on Windows");
            }
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 640,
                bottom: 400,
            };
            view.getSize(&mut rect);
            let resizable = view.canResize() == kResultOk;
            let mut ed = Self::open_window(
                title,
                rect.right - rect.left,
                rect.bottom - rect.top,
                resizable,
            )?;
            let frame = ComWrapper::new(PlugFrame {
                hwnd: ed.hwnd as isize,
            });
            if let Some(f) = frame.as_com_ref::<IPlugFrame>() {
                view.setFrame(f.as_ptr());
            }
            if view.attached(ed.hwnd, kPlatformTypeHWND) != kResultOk {
                view.setFrame(ptr::null_mut());
                DestroyWindow(ed.hwnd);
                return Err(anyhow!("the plugin couldn't open its window"));
            }
            // Some editors only know their size once attached.
            if view.getSize(&mut rect) == kResultOk {
                resize_client(ed.hwnd, rect.right - rect.left, rect.bottom - rect.top);
            }
            with_events(|e| {
                e.resized.remove(&(ed.hwnd as isize));
            });
            ed.vst3 = Some((view, frame));
            Ok(ed)
        }
    }

    pub fn hwnd(&self) -> *mut c_void {
        self.hwnd
    }

    pub fn resize_client(&self, w: i32, h: i32) {
        unsafe { resize_client(self.hwnd, w, h) };
        with_events(|e| {
            e.resized.remove(&(self.hwnd as isize));
        });
    }

    /// The user closed the window.
    pub fn closed(&self) -> bool {
        with_events(|e| e.closed.contains(&(self.hwnd as isize)))
    }

    /// A new client size after the user resized the window.
    pub fn take_user_resize(&self) -> Option<(i32, i32)> {
        with_events(|e| e.resized.remove(&(self.hwnd as isize)))
    }

    /// Keep a VST3 editor in step with the window (call on idle).
    pub fn poll(&self) {
        if let Some((view, _)) = &self.vst3
            && let Some((w, h)) = self.take_user_resize()
        {
            unsafe {
                let mut r = ViewRect {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                if view.checkSizeConstraint(&mut r) == kResultOk {
                    view.onSize(&mut r);
                    let (cw, ch) = (r.right - r.left, r.bottom - r.top);
                    let mut cur = RECT {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    };
                    GetClientRect(self.hwnd, &mut cur);
                    if cur.right != cw || cur.bottom != ch {
                        resize_client(self.hwnd, cw, ch);
                    }
                }
            }
        }
    }

    pub fn close(mut self) {
        unsafe {
            if let Some((view, _frame)) = self.vst3.take() {
                view.removed();
                view.setFrame(ptr::null_mut());
            }
            DestroyWindow(self.hwnd);
        }
        let h = self.hwnd as isize;
        with_events(|e| {
            e.closed.retain(|x| *x != h);
            e.resized.remove(&h);
        });
    }
}
