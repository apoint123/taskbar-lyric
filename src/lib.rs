use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{
            AtomicBool,
            Ordering,
        },
        mpsc::{
            self,
            Receiver,
            Sender,
        },
    },
    thread::{
        self,
    },
};

use anyhow::{
    Result,
    anyhow,
};
use strategy::{
    LayoutParams,
    LegacyStrategy,
    TaskbarLayout,
    TaskbarStrategy,
    Win11Strategy,
};
use utils::get_windows_build_number;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle,
            HANDLE,
            HWND,
            WAIT_OBJECT_0,
        },
        System::{
            Com::{
                COINIT_MULTITHREADED,
                CoInitializeEx,
                CoUninitialize,
            },
            Registry::{
                HKEY,
                HKEY_CURRENT_USER,
                KEY_NOTIFY,
                REG_NOTIFY_CHANGE_LAST_SET,
                RegCloseKey,
                RegNotifyChangeKeyValue,
                RegOpenKeyExW,
            },
            Threading::{
                CreateEventW,
                INFINITE,
                SetEvent,
                WaitForMultipleObjects,
            },
        },
    },
    core::w,
};

/// 任务列表和歌词之间的微小间距
pub const GAP: i32 = 10;

mod logger;
mod strategy;
mod tray_watcher;
mod uia;
mod uia_watcher;
mod utils;

pub use strategy::{
    Rect,
    Win10Layout,
    Win11Layout,
};
pub use tray_watcher::TrayWatcher;
pub use uia_watcher::UiaWatcher;

pub type TaskbarLayoutCallback = Box<dyn Fn(TaskbarLayout) + Send + 'static>;
pub type RegistryChangedCallback = Box<dyn Fn() + Send + Sync + 'static>;

enum TaskbarCommand {
    Embed { hwnd_ptr: usize },
    Update { width: i32 },
    Stop,
}

pub struct TaskbarService {
    sender: Sender<TaskbarCommand>,
}

impl TaskbarService {
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(TaskbarLayout) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let callback: TaskbarLayoutCallback = Box::new(callback);
            worker_loop(&rx, &callback);
        });

        Self { sender: tx }
    }

    pub fn embed_window_by_ptr(&self, hwnd_ptr: usize) {
        let _ = self.sender.send(TaskbarCommand::Embed { hwnd_ptr });
    }

    pub fn embed_window(&self, hwnd: HWND) {
        self.embed_window_by_ptr(hwnd.0 as usize);
    }

    pub fn update(&self, lyric_width: i32) {
        let _ = self
            .sender
            .send(TaskbarCommand::Update { width: lyric_width });
    }

    pub fn stop(&self) {
        let _ = self.sender.send(TaskbarCommand::Stop);
    }
}

fn worker_loop(rx: &Receiver<TaskbarCommand>, callback: &TaskbarLayoutCallback) {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            error!("CoInitializeEx 失败: {hr:?}");
            return;
        }
    }

    let mut strategy = create_strategy();

    while let Ok(msg) = rx.recv() {
        match msg {
            TaskbarCommand::Embed { hwnd_ptr } => {
                let hwnd = HWND(hwnd_ptr as *mut c_void);
                if let Some(s) = strategy.as_ref() {
                    info!(?hwnd_ptr, "正在嵌入窗口",);
                    if !s.embed_window(hwnd) {
                        error!("嵌入窗口失败");
                    }
                }
            }

            TaskbarCommand::Update { width } => {
                let mut final_width = width;
                let mut stop_signal = false;

                while let Ok(next_msg) = rx.try_recv() {
                    match next_msg {
                        TaskbarCommand::Update { width: w } => final_width = w,
                        TaskbarCommand::Embed { hwnd_ptr } => {
                            let hwnd = HWND(hwnd_ptr as *mut c_void);
                            if let Some(s) = strategy.as_ref() {
                                s.embed_window(hwnd);
                            }
                        }
                        TaskbarCommand::Stop => {
                            stop_signal = true;
                            break;
                        }
                    }
                }

                if stop_signal {
                    break;
                }

                if let Some(s) = strategy.as_mut() {
                    let params = LayoutParams {
                        lyric_width: final_width,
                    };
                    if let Some(layout) = s.update_layout(params) {
                        callback(layout);
                    }
                }
            }

            TaskbarCommand::Stop => {
                break;
            }
        }
    }

    if let Some(s) = strategy.as_ref() {
        s.restore();
    }
    unsafe {
        CoUninitialize();
    }
}

fn create_strategy() -> Option<Box<dyn TaskbarStrategy>> {
    let build_num = get_windows_build_number();
    debug!("Windows 版本号: {build_num}");

    let (mut primary, mut secondary): (Box<dyn TaskbarStrategy>, Box<dyn TaskbarStrategy>) =
        if build_num >= 22000 {
            (
                Box::new(Win11Strategy::new()),
                Box::new(LegacyStrategy::new()),
            )
        } else {
            (
                Box::new(LegacyStrategy::new()),
                Box::new(Win11Strategy::new()),
            )
        };

    if primary.init() {
        debug!("首选策略初始化成功");
        return Some(primary);
    }

    warn!("首选策略失效，尝试备选策略");
    if secondary.init() {
        debug!("备选策略初始化成功");
        return Some(secondary);
    }

    error!("未检测到支持的任务栏结构");
    None
}

/// 用于关闭句柄的 RAII 包装器
struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// Safety: 跨线程访问句柄是安全的
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

pub struct RegistryWatcher {
    stop_event: Arc<EventHandle>,
    is_running: Arc<AtomicBool>,
}

impl RegistryWatcher {
    /// 启动注册表监听
    ///
    /// 当 `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced`
    /// 下的值发生变化时，会调用传入的 Rust 回调函数，可以用它来监听任务栏的布局更改
    ///
    /// ## Errors
    /// 创建停止事件失败时抛出错误
    pub fn new<F>(callback: F) -> Result<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let raw_event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| anyhow!("创建停止事件失败: {e}"))?;

        let stop_event = Arc::new(EventHandle(raw_event));
        let is_running = Arc::new(AtomicBool::new(true));
        let thread_event = stop_event.clone();
        let callback: Arc<RegistryChangedCallback> = Arc::new(Box::new(callback));

        thread::spawn(move || unsafe {
            Self::watch_loop(&thread_event, &callback);
        });

        Ok(Self {
            stop_event,
            is_running,
        })
    }

    pub fn stop(&self) {
        if !self.is_running.load(Ordering::SeqCst) {
            return;
        }

        unsafe {
            let _ = SetEvent(self.stop_event.0);
        }

        self.is_running.store(false, Ordering::SeqCst);
        info!("注册表监听已停止");
    }

    unsafe fn watch_loop(
        stop_event_wrapper: &Arc<EventHandle>,
        callback: &Arc<RegistryChangedCallback>,
    ) {
        let stop_event = stop_event_wrapper.0;

        let mut h_key = HKEY::default();
        let sub_key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced");

        unsafe {
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                sub_key,
                Some(0),
                KEY_NOTIFY,
                &raw mut h_key,
            )
            .is_err()
            {
                error!("打开注册表键失败");
                return;
            }

            let reg_event = match CreateEventW(None, false, false, None) {
                Ok(evt) => evt,
                Err(e) => {
                    let _ = &e;
                    error!("创建注册表事件失败: {e}");
                    let _ = RegCloseKey(h_key);
                    return;
                }
            };

            loop {
                let notify_res = RegNotifyChangeKeyValue(
                    h_key,
                    true,
                    REG_NOTIFY_CHANGE_LAST_SET,
                    Some(reg_event),
                    true,
                );

                if notify_res.is_err() {
                    error!("注册通知失败");
                    break;
                }

                let handles = [stop_event, reg_event];
                let wait_result = WaitForMultipleObjects(&handles, false, INFINITE);

                let index = wait_result.0.wrapping_sub(WAIT_OBJECT_0.0);

                match index {
                    0 => {
                        debug!("退出监听循环");
                        break;
                    }
                    1 => {
                        callback();
                    }
                    _ => {
                        error!("WaitForMultipleObjects 返回异常或超时 {wait_result:?}");
                        break;
                    }
                }
            }

            let _ = CloseHandle(reg_event);
            let _ = RegCloseKey(h_key);
        }
    }
}

impl Drop for RegistryWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}
