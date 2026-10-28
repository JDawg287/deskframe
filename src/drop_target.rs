//! Dropping files onto a frame: Explorer's rules for copy or move, and the
//! frame's OLE drop target.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Component, Path, PathBuf};

use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{IUnknown, IUnknown_Vtbl, Interface, Result, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Foundation::{E_NOINTERFACE, POINT, POINTL, S_OK};
use windows::Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDataObject, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT,
    FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Vtbl, RegisterDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, IDropTargetHelper, IShellItemArray, SHCreateShellItemArrayFromDataObject,
    CFSTR_LOGICALPERFORMEDDROPEFFECT, CFSTR_PERFORMEDDROPEFFECT, SIGDN_FILESYSPATH,
};

use crate::dlog;
use crate::frame::Frame;

/// Modifier keys held during a drag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// What a drop does, by Explorer's rules: Ctrl copies, Shift moves,
/// Ctrl+Shift or Alt makes a shortcut, and with no key a drop on the same
/// drive moves while one from another drive copies. If the source does not
/// allow that, the first of copy, move, shortcut it does allow.
pub fn effect(keys: Keys, same_volume: bool, allowed: DROPEFFECT) -> DROPEFFECT {
    let wanted = if keys.alt || (keys.ctrl && keys.shift) {
        DROPEFFECT_LINK
    } else if keys.ctrl {
        DROPEFFECT_COPY
    } else if keys.shift || same_volume {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    let ok = |e: DROPEFFECT| allowed.0 & e.0 != 0;
    if ok(wanted) {
        return wanted;
    }
    [DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_LINK]
        .into_iter()
        .find(|&e| ok(e))
        .unwrap_or(DROPEFFECT(0))
}

/// True when `a` and `b` are on the same volume. `volume_of` names the
/// volume a path is on; where it cannot, the drive or share decides.
pub fn same_volume_by(a: &Path, b: &Path, volume_of: impl Fn(&Path) -> Option<String>) -> bool {
    match (volume_of(a), volume_of(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y),
        _ => same_prefix(a, b),
    }
}

/// The volume `path` is on: its volume GUID path, else the root it sits
/// under (a network share, a subst drive). `None` if Windows cannot say.
fn volume_of(path: &Path) -> Option<String> {
    let text = |buf: &[u16]| {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    };
    let mut root = [0u16; 1024];
    unsafe { GetVolumePathNameW(&HSTRING::from(path.as_os_str()), &mut root) }.ok()?;
    let mut guid = [0u16; 64];
    match unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut guid) } {
        Ok(()) => Some(text(&guid)),
        Err(_) => Some(text(&root)),
    }
}

/// True when `a` and `b` start with the same drive (or network share).
pub fn same_prefix(a: &Path, b: &Path) -> bool {
    let prefix = |p: &Path| match p.components().next() {
        Some(Component::Prefix(x)) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
        _ => None,
    };
    matches!((prefix(a), prefix(b)), (Some(x), Some(y)) if x == y)
}

/// The effect `Drop` reports back to the source for a chosen effect. The
/// frame moves files itself (an "optimized move"), so a move is reported as
/// none: the source must never delete its originals, whether the move
/// later completes, fails or is cancelled.
pub fn reported_effect(chosen: DROPEFFECT) -> DROPEFFECT {
    if chosen == DROPEFFECT_MOVE {
        DROPEFFECT_NONE
    } else {
        chosen
    }
}

/// True when dropping `sources` into `target` would do nothing useful:
/// moving files into the folder they are already in, or a folder into
/// itself or one of its own subfolders.
pub fn pointless(sources: &[PathBuf], target: &Path, effect: DROPEFFECT) -> bool {
    // Windows paths compare without regard to case.
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    let target = lower(target);
    sources.iter().any(|src| {
        let src = lower(src);
        let into_itself = target.starts_with(&src);
        let same_folder = effect == DROPEFFECT_MOVE && src.parent() == Some(target.as_path());
        into_itself || same_folder
    })
}

/// Where a drop would land, for the frame to highlight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropSpot {
    /// The frame's own folder.
    Frame,
    /// The folder item at this index.
    Cell(usize),
}

/// What the frame's drop target knows. It points at its frame, which
/// revokes the registration before it is dropped.
struct DropTarget {
    frame: *const Frame,
    /// Shows the drag image over the frame, as Explorer does.
    helper: Option<IDropTargetHelper>,
    /// Paths being dragged (read on enter); empty if they are not files.
    sources: RefCell<Vec<PathBuf>>,
    /// Volumes looked up during this drag (`DragOver` comes many times a
    /// second; a share can be slow to answer).
    volumes: RefCell<Vec<(PathBuf, Option<String>)>>,
}

/// Registers `frame`'s window as an OLE drop target.
pub fn register(frame: &Frame) -> Result<()> {
    let helper =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok();
    let object = Box::new(DropTargetObject {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        target: DropTarget {
            frame,
            helper,
            sources: RefCell::new(Vec::new()),
            volumes: RefCell::new(Vec::new()),
        },
    });
    // The interface takes the one reference; OLE adds its own.
    let target = unsafe { IDropTarget::from_raw(Box::into_raw(object) as *mut c_void) };
    unsafe { RegisterDragDrop(frame.hwnd(), &target) }
}

impl DropTarget {
    fn frame(&self) -> &Frame {
        // Safety: the frame revokes this target before it is dropped.
        unsafe { &*self.frame }
    }

    /// `volume_of`, remembered for the rest of the drag.
    fn volume(&self, path: &Path) -> Option<String> {
        if let Some((_, v)) = self.volumes.borrow().iter().find(|(p, _)| p == path) {
            return v.clone();
        }
        let v = volume_of(path);
        self.volumes
            .borrow_mut()
            .push((path.to_path_buf(), v.clone()));
        v
    }

    /// Picks the effect for the pointer at `pt`, shows the drop spot and
    /// writes the effect back for OLE.
    fn over(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: POINTL,
        effect: *mut DROPEFFECT,
    ) -> (DROPEFFECT, PathBuf) {
        let frame = self.frame();
        let (target, spot) = frame.drop_spot(POINT { x: pt.x, y: pt.y });
        // Shortcuts are not made on a drop (yet): never offer one.
        let allowed = DROPEFFECT(unsafe { *effect }.0 & !DROPEFFECT_LINK.0);
        let chosen = {
            // Released before redrawing: nothing below may re-enter with it held.
            let sources = self.sources.borrow();
            match sources.first() {
                // Nowhere to put them while the folder is not available.
                _ if !frame.folder_available() => DROPEFFECT_NONE,
                None => DROPEFFECT_NONE,
                Some(first) => {
                    let e = self::effect(
                        keys_of(keys),
                        same_volume_by(first, &target, |p| self.volume(p)),
                        allowed,
                    );
                    if pointless(&sources, &target, e) {
                        DROPEFFECT_NONE
                    } else {
                        e
                    }
                }
            }
        };
        frame.set_drop((chosen != DROPEFFECT_NONE).then_some(spot));
        unsafe { *effect = chosen };
        (chosen, target)
    }

    fn drag_enter(
        &self,
        data: Option<&IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: POINTL,
        effect: *mut DROPEFFECT,
    ) {
        *self.sources.borrow_mut() = data.map(paths_of).unwrap_or_default();
        self.volumes.borrow_mut().clear();
        if let (Some(h), Some(d)) = (&self.helper, data) {
            unsafe {
                let _ = h.DragEnter(self.frame().hwnd(), d, &POINT { x: pt.x, y: pt.y }, *effect);
            }
        }
        self.over(keys, pt, effect);
    }

    fn drag_over(&self, keys: MODIFIERKEYS_FLAGS, pt: POINTL, effect: *mut DROPEFFECT) {
        let (chosen, _) = self.over(keys, pt, effect);
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragOver(&POINT { x: pt.x, y: pt.y }, chosen);
            }
        }
    }

    fn drag_leave(&self) {
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        self.frame().set_drop(None);
        self.sources.borrow_mut().clear();
    }

    fn drop(
        &self,
        data: Option<&IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: POINTL,
        effect: *mut DROPEFFECT,
    ) {
        let (chosen, target) = self.over(keys, pt, effect);
        if let (Some(h), Some(d)) = (&self.helper, data) {
            unsafe {
                let _ = h.Drop(d, &POINT { x: pt.x, y: pt.y }, chosen);
            }
        }
        let frame = self.frame();
        frame.set_drop(None);
        self.sources.borrow_mut().clear();
        let (true, Some(d)) = (chosen != DROPEFFECT_NONE, data) else {
            return;
        };
        let items = match unsafe { SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(d) } {
            Ok(items) => items,
            Err(e) => {
                dlog!("drop into {}: items not readable: {e}", target.display());
                unsafe { *effect = DROPEFFECT_NONE };
                return;
            }
        };
        let moving = chosen == DROPEFFECT_MOVE;
        if moving {
            // The frame moves the files itself: the source must not delete
            // anything, though the user sees a move.
            tell_source(d, CFSTR_PERFORMEDDROPEFFECT, DROPEFFECT_NONE);
            tell_source(d, CFSTR_LOGICALPERFORMEDDROPEFFECT, DROPEFFECT_MOVE);
        }
        unsafe { *effect = reported_effect(chosen) };
        // Run after the drop returns, so the source app is not held up for
        // the length of the copy.
        frame.queue_transfer(items, target, moving);
    }
}

/// Stores a drop effect in the drag's data object under `format` (one of
/// the shell's "performed drop effect" formats).
fn tell_source(data: &IDataObject, format: windows::core::PCWSTR, value: DROPEFFECT) {
    unsafe {
        let cf = RegisterClipboardFormatW(format);
        if cf == 0 {
            return;
        }
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, size_of::<u32>()) else {
            return;
        };
        let p = GlobalLock(mem) as *mut u32;
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            return;
        }
        *p = value.0;
        let _ = GlobalUnlock(mem);
        let fmt = FORMATETC {
            cfFormat: cf as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: mem },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        // With release = true the data object owns the memory on success.
        if data.SetData(&fmt, &medium, true).is_err() {
            let _ = GlobalFree(Some(mem));
        }
    }
}

fn keys_of(k: MODIFIERKEYS_FLAGS) -> Keys {
    /// Alt in a drop's key state (MK_ALT).
    const MK_ALT: u32 = 0x20;
    Keys {
        ctrl: k.0 & MK_CONTROL.0 != 0,
        shift: k.0 & MK_SHIFT.0 != 0,
        alt: k.0 & MK_ALT != 0,
    }
}

/// File system paths of the items in a drag; empty if they are not files.
fn paths_of(data: &IDataObject) -> Vec<PathBuf> {
    let Ok(items) = (unsafe { SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(data) })
    else {
        return Vec::new();
    };
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    (0..count)
        .filter_map(|i| unsafe {
            let name = items
                .GetItemAt(i)
                .ok()?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .ok()?;
            let path = name.to_string();
            CoTaskMemFree(Some(name.0 as *const c_void));
            path.ok().map(PathBuf::from)
        })
        .collect()
}

// The COM object, written out by hand: the `implement` macro would need a
// direct `windows-core` dependency, which this project does not take.

#[repr(C)]
struct DropTargetObject {
    vtbl: *const IDropTarget_Vtbl,
    refs: AtomicU32,
    target: DropTarget,
}

static VTBL: IDropTarget_Vtbl = IDropTarget_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: query_interface,
        AddRef: add_ref,
        Release: release,
    },
    DragEnter: drag_enter,
    DragOver: drag_over,
    DragLeave: drag_leave,
    Drop: on_drop,
};

/// Safety (all callbacks): OLE passes `this` as the pointer `register` made.
unsafe fn object<'a>(this: *mut c_void) -> &'a DropTargetObject {
    unsafe { &*(this as *const DropTargetObject) }
}

unsafe extern "system" fn query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        if *iid == IUnknown::IID || *iid == IDropTarget::IID {
            add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    unsafe { object(this) }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let left = unsafe { object(this) }.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(unsafe { Box::from_raw(this as *mut DropTargetObject) });
    }
    left
}

unsafe extern "system" fn drag_enter(
    this: *mut c_void,
    data: *mut c_void,
    keys: MODIFIERKEYS_FLAGS,
    pt: POINTL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    let data = unsafe { IDataObject::from_raw_borrowed(&data) };
    unsafe { object(this) }
        .target
        .drag_enter(data, keys, pt, effect);
    S_OK
}

unsafe extern "system" fn drag_over(
    this: *mut c_void,
    keys: MODIFIERKEYS_FLAGS,
    pt: POINTL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    unsafe { object(this) }.target.drag_over(keys, pt, effect);
    S_OK
}

unsafe extern "system" fn drag_leave(this: *mut c_void) -> HRESULT {
    unsafe { object(this) }.target.drag_leave();
    S_OK
}

unsafe extern "system" fn on_drop(
    this: *mut c_void,
    data: *mut c_void,
    keys: MODIFIERKEYS_FLAGS,
    pt: POINTL,
    effect: *mut DROPEFFECT,
) -> HRESULT {
    let data = unsafe { IDataObject::from_raw_borrowed(&data) };
    unsafe { object(this) }.target.drop(data, keys, pt, effect);
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: DROPEFFECT = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);
    const NO_KEYS: Keys = Keys {
        ctrl: false,
        shift: false,
        alt: false,
    };

    #[test]
    fn same_volume_moves() {
        assert_eq!(effect(NO_KEYS, true, ALL), DROPEFFECT_MOVE);
    }

    #[test]
    fn other_volume_copies() {
        assert_eq!(effect(NO_KEYS, false, ALL), DROPEFFECT_COPY);
    }

    #[test]
    fn ctrl_copies() {
        let keys = Keys {
            ctrl: true,
            ..NO_KEYS
        };
        assert_eq!(effect(keys, true, ALL), DROPEFFECT_COPY);
    }

    #[test]
    fn shift_moves() {
        let keys = Keys {
            shift: true,
            ..NO_KEYS
        };
        assert_eq!(effect(keys, false, ALL), DROPEFFECT_MOVE);
    }

    #[test]
    fn alt_links() {
        let keys = Keys {
            alt: true,
            ..NO_KEYS
        };
        assert_eq!(effect(keys, true, ALL), DROPEFFECT_LINK);
        let both = Keys {
            ctrl: true,
            shift: true,
            ..NO_KEYS
        };
        assert_eq!(effect(both, true, ALL), DROPEFFECT_LINK);
    }

    #[test]
    fn falls_back_to_allowed_effect() {
        // A move the source does not allow becomes a copy.
        assert_eq!(effect(NO_KEYS, true, DROPEFFECT_COPY), DROPEFFECT_COPY);
        // A shortcut that is not allowed: copy first.
        let alt = Keys {
            alt: true,
            ..NO_KEYS
        };
        assert_eq!(effect(alt, true, DROPEFFECT_MOVE), DROPEFFECT_MOVE);
        assert_eq!(effect(NO_KEYS, true, DROPEFFECT(0)), DROPEFFECT(0));
    }

    #[test]
    fn same_prefix_compares_drives_and_shares() {
        assert!(same_prefix(Path::new(r"C:\a\b"), Path::new(r"c:\x")));
        assert!(!same_prefix(Path::new(r"C:\a"), Path::new(r"D:\a")));
        assert!(same_prefix(
            Path::new(r"\\nas\share\a"),
            Path::new(r"\\NAS\Share\b")
        ));
        assert!(!same_prefix(
            Path::new(r"\\nas\share\a"),
            Path::new(r"\\nas\other\a")
        ));
        assert!(!same_prefix(
            Path::new(r"C:\a"),
            Path::new(r"\\nas\share\a")
        ));
    }

    #[test]
    fn moving_into_the_same_folder_is_pointless() {
        let src = [PathBuf::from(r"C:\Users\me\Downloads\a.txt")];
        let folder = Path::new(r"C:\Users\me\Downloads");
        assert!(pointless(&src, folder, DROPEFFECT_MOVE));
        // A copy there is fine (Explorer makes "a - Copy.txt").
        assert!(!pointless(&src, folder, DROPEFFECT_COPY));
    }

    #[test]
    fn dropping_a_folder_into_itself_is_pointless() {
        let src = [PathBuf::from(r"C:\Work\Photos")];
        assert!(pointless(
            &src,
            Path::new(r"C:\Work\Photos"),
            DROPEFFECT_COPY
        ));
        assert!(pointless(
            &src,
            Path::new(r"C:\Work\Photos\2024"),
            DROPEFFECT_MOVE
        ));
        assert!(!pointless(
            &src,
            Path::new(r"C:\Work\Photos 2"),
            DROPEFFECT_MOVE
        ));
    }

    #[test]
    fn a_move_is_reported_as_none_so_the_source_keeps_its_files() {
        assert_eq!(reported_effect(DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(reported_effect(DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(reported_effect(DROPEFFECT_NONE), DROPEFFECT_NONE);
    }

    /// A fake volume lookup: C:\\mnt\\disk2 is a second disk mounted in a
    /// folder of C:; paths on Q: cannot be looked up.
    fn fake_volume(p: &Path) -> Option<String> {
        let s = p.to_string_lossy().to_lowercase();
        if s.starts_with(r"c:\mnt\disk2") {
            Some(r"\\?\Volume{disk2}\".into())
        } else if s.starts_with(r"c:") {
            Some(r"\\?\Volume{system}\".into())
        } else if s.starts_with(r"d:") {
            Some(r"\\?\Volume{System}\".into())
        } else {
            None
        }
    }

    #[test]
    fn a_mount_point_is_its_own_volume() {
        let v = fake_volume;
        assert!(!same_volume_by(
            Path::new(r"C:\mnt\disk2\a"),
            Path::new(r"C:\Users\b"),
            v
        ));
        assert!(same_volume_by(
            Path::new(r"C:\mnt\disk2\a"),
            Path::new(r"c:\MNT\Disk2\b"),
            v
        ));
        // A second letter for the same volume (as the lookup reports it).
        assert!(same_volume_by(Path::new(r"C:\a"), Path::new(r"D:\b"), v));
    }

    #[test]
    fn unknown_volumes_fall_back_to_the_drive() {
        let v = fake_volume;
        assert!(same_volume_by(Path::new(r"Q:\a"), Path::new(r"q:\b"), v));
        assert!(!same_volume_by(Path::new(r"Q:\a"), Path::new(r"C:\b"), v));
    }

    #[test]
    fn a_real_folder_has_a_volume_guid() {
        let here = std::env::current_dir().unwrap();
        let v = volume_of(&here).expect("volume");
        assert!(v.starts_with(r"\\?\Volume{"), "{v}");
        assert!(same_volume_by(&here, &here.join("sub"), volume_of));
    }
}
