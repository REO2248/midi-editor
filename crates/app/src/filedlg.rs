//! Native Windows common-item dialogs (#158). gpui's `prompt_for_paths`
//! API hardcodes an "All files" filter on Windows, so Open/Save-As go
//! through `IFileOpenDialog`/`IFileSaveDialog` directly to offer the
//! conventional "MIDI files / All files" filter list, a real default
//! extension, and an explicit start folder.
//!
//! `Show` is modal: each call runs on its own COM-initialized thread (the
//! caller polls a channel from the app's tick), so a lingering dialog can
//! never wedge the event loop.

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver};
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx, CLSCTX_ALL,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
        SHCreateItemFromParsingName, FOS_FILEMUSTEXIST, FOS_OVERWRITEPROMPT, SIGDN_FILESYSPATH,
    };

    /// The dialog is modal to the app window; HWND isn't `Send` so it
    /// crosses the thread boundary as an opaque usize.
    pub(crate) fn save_path(hwnd: usize, dir: PathBuf, name: String) -> Receiver<Option<PathBuf>> {
        spawn(move || unsafe {
            com_init();
            let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_ALL).ok()?;
            dlg.SetOptions(dlg.GetOptions().ok()? | FOS_OVERWRITEPROMPT)
                .ok()?;
            dlg.SetFileTypes(&[
                COMDLG_FILTERSPEC {
                    pszName: w!("MIDI files"),
                    pszSpec: w!("*.mid;*.midi;*.smf;*.kar"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("All files"),
                    pszSpec: w!("*.*"),
                },
            ])
            .ok()?;
            // the OS appends the default extension itself when the typed
            // name has none
            dlg.SetDefaultExtension(w!("mid")).ok()?;
            dlg.SetFileName(&HSTRING::from(name)).ok()?;
            let _ = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR(HSTRING::from(dir.as_os_str()).as_ptr()),
                None::<&IBindCtx>,
            )
            .map(|folder| dlg.SetFolder(&folder));
            match dlg.Show(Some(HWND(hwnd as _))) {
                Ok(()) => result_path(dlg.GetResult().ok()),
                Err(_) => None,
            }
        })
    }

    pub(crate) fn open_path(hwnd: usize, dir: PathBuf) -> Receiver<Option<PathBuf>> {
        spawn(move || unsafe {
            com_init();
            let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
            dlg.SetOptions(dlg.GetOptions().ok()? | FOS_FILEMUSTEXIST)
                .ok()?;
            dlg.SetFileTypes(&[
                COMDLG_FILTERSPEC {
                    pszName: w!("MIDI files"),
                    pszSpec: w!("*.mid;*.midi;*.smf;*.kar"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("All files"),
                    pszSpec: w!("*.*"),
                },
            ])
            .ok()?;
            let _ = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR(HSTRING::from(dir.as_os_str()).as_ptr()),
                None::<&IBindCtx>,
            )
            .map(|folder| dlg.SetFolder(&folder));
            match dlg.Show(Some(HWND(hwnd as _))) {
                Ok(()) => result_path(dlg.GetResult().ok()),
                Err(_) => None,
            }
        })
    }

    fn spawn(f: impl FnOnce() -> Option<PathBuf> + Send + 'static) -> Receiver<Option<PathBuf>> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx
    }

    unsafe fn com_init() {
        // already-initialized is fine (RPC_E_CHANGED_MODE benign here)
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    unsafe fn result_path(item: Option<IShellItem>) -> Option<PathBuf> {
        let pw = item?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = pw.to_string().ok()?;
        if !pw.is_null() {
            CoTaskMemFree(Some(pw.as_ptr() as _));
        }
        CoUninitialize();
        Some(PathBuf::from(s))
    }
}

#[cfg(windows)]
pub(crate) use imp::{open_path, save_path};

/// Poll a dialog thread's result channel without blocking the UI —
/// the same pattern the save worker's `save_rx` tick uses, but awaited
/// from an async context (the quit guard can't reach the tick).
pub(crate) async fn result(
    rx: std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
    exec: gpui_kit::BackgroundExecutor,
) -> Option<std::path::PathBuf> {
    loop {
        match rx.try_recv() {
            Ok(p) => return p,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                exec.timer(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

/// Non-Windows fallback: the gpui prompt stays the path there.
#[cfg(not(windows))]
mod stub {
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Receiver};
    pub(crate) fn save_path(
        _hwnd: usize,
        _dir: PathBuf,
        _name: String,
    ) -> Receiver<Option<PathBuf>> {
        let (tx, rx) = channel();
        let _ = tx.send(None);
        rx
    }
    pub(crate) fn open_path(_hwnd: usize, _dir: PathBuf) -> Receiver<Option<PathBuf>> {
        let (tx, rx) = channel();
        let _ = tx.send(None);
        rx
    }
}

#[cfg(not(windows))]
pub(crate) use stub::{open_path, save_path};
