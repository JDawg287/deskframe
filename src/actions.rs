//! What the user can do through the shell: open an item, show it in
//! Explorer, copy its path, delete it, drag it out, pick a folder for a
//! new frame. Thin
//! wrappers; they report failures to the log and otherwise do nothing, as
//! Explorer would.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IBindCtx, IDataObject, CLSCTX_ALL, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    IDropSource, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
};
use windows::Win32::UI::Shell::{
    BHID_DataObject, FileOpenDialog, FileOperation, IFileOpenDialog, IFileOperation,
    IFileOperationProgressSink, IShellItem, IShellItemArray, SHCreateItemFromParsingName,
    SHDoDragDrop, ShellExecuteExW, FOFX_ADDUNDORECORD, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO,
    FOF_WANTNUKEWARNING, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SEE_MASK_INVOKEIDLIST,
    SHELLEXECUTEINFOW, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::dlog;

/// Standard clipboard format for UTF-16 text.
const CF_UNICODETEXT: u32 = 13;

/// Opens `path` with its default verb, as a double-click in Explorer does.
pub fn open(owner: HWND, path: &Path) {
    shell_execute(owner, &HSTRING::from(path.as_os_str()), None);
}

/// Opens an Explorer window on the item's folder with the item selected.
pub fn show_in_folder(owner: HWND, path: &Path) {
    let args = HSTRING::from(format!("/select,\"{}\"", path.display()));
    shell_execute(owner, &HSTRING::from("explorer.exe"), Some(&args));
}

fn shell_execute(owner: HWND, file: &HSTRING, args: Option<&HSTRING>) {
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        hwnd: owner,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: args.map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        dlog!("open {} failed: {e}", file);
    }
}

/// Puts `text` on the clipboard.
pub fn copy_text(owner: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let bytes = wide.len() * 2;
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            dlog!("copy: clipboard busy");
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let dst = GlobalLock(mem) as *mut u16;
            let mut owned_by_clipboard = false;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                let _ = GlobalUnlock(mem);
                owned_by_clipboard = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).is_ok();
            }
            if !owned_by_clipboard {
                let _ = GlobalFree(Some(mem));
            }
        }
        let _ = CloseClipboard();
    }
}

/// Asks the user to choose a folder; `None` if they cancel.
pub fn pick_folder(owner: HWND) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        let _ = dialog.SetTitle(w!("Choose a folder for the new frame"));
        dialog.Show(Some(owner)).ok()?; // fails when cancelled
        let name = dialog
            .GetResult()
            .ok()?
            .GetDisplayName(SIGDN_FILESYSPATH)
            .ok()?;
        let path = name.to_string();
        CoTaskMemFree(Some(name.0 as *const c_void));
        path.ok().map(PathBuf::from)
    }
}

/// Lets the user drag `path` out of the frame, as from Explorer: copy,
/// move or shortcut, whatever the drop target allows. Returns once the item
/// is dropped or the drag is cancelled.
pub fn drag_out(owner: HWND, path: &Path) {
    let run = || -> windows::core::Result<DROPEFFECT> {
        unsafe {
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None::<&IBindCtx>)?;
            let data: IDataObject = item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject)?;
            SHDoDragDrop(
                Some(owner),
                &data,
                None::<&IDropSource>,
                DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            )
        }
    };
    match run() {
        Ok(effect) => dlog!("dragged {} out (effect {})", path.display(), effect.0),
        Err(e) => dlog!("drag of {} failed: {e}", path.display()),
    }
}

/// Copies or moves the dragged items in `data` into `dest`, the way
/// Explorer does: Windows' own progress, conflict prompts and undo.
pub fn transfer(owner: HWND, items: &IShellItemArray, dest: &Path, move_items: bool) {
    let run = || -> windows::core::Result<()> {
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOwnerWindow(owner)?;
            op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_ADDUNDORECORD)?;
            let target: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(dest.as_os_str()), None::<&IBindCtx>)?;
            if move_items {
                op.MoveItems(items, &target)?;
            } else {
                op.CopyItems(items, &target)?;
            }
            op.PerformOperations()
        }
    };
    let what = if move_items { "move" } else { "copy" };
    match run() {
        Ok(()) => dlog!("{what} into {} done", dest.display()),
        Err(e) => dlog!("{what} into {}: {e}", dest.display()),
    }
}

/// Deletes `paths` the way Explorer does: to the Recycle Bin with an undo
/// record, or `permanently`; Windows shows its own confirmation and
/// progress. Cancelling is not an error worth more than a log line.
pub fn delete(owner: HWND, paths: &[PathBuf], permanently: bool) {
    let flags = if permanently {
        FOF_WANTNUKEWARNING
    } else {
        FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_WANTNUKEWARNING
    };
    let run = || -> windows::core::Result<()> {
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOwnerWindow(owner)?;
            op.SetOperationFlags(flags)?;
            for p in paths {
                let item: IShellItem =
                    SHCreateItemFromParsingName(&HSTRING::from(p.as_os_str()), None::<&IBindCtx>)?;
                op.DeleteItem(&item, None::<&IFileOperationProgressSink>)?;
            }
            op.PerformOperations()
        }
    };
    if let Err(e) = run() {
        dlog!("delete {} item(s): {e}", paths.len());
    }
}
