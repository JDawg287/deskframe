//! A frame's folder: listing its items the way Explorer shows them, and a
//! watcher thread that tells the frame when anything in it changes.

use std::cmp::Ordering;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, ReadDirectoryChangesW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NAME_NORMALIZED, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, RegisterDeviceNotificationW, UnregisterDeviceNotification, DBT_DEVTYP_HANDLE,
    DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_HANDLE, HDEVNOTIFY, WM_APP,
};

use crate::{dlog, groups};

/// `ReadDirectoryChangesW` buffer size (64 KB is also the limit over the network).
const CHANGE_BUFFER_BYTES: usize = 64 * 1024;
/// Longest `Watcher::start` waits for the first watch to be armed.
const START_WAIT_MS: u64 = 500;
/// Longest `Watcher::release` waits for the folder handle to be closed.
const RELEASE_WAIT_MS: u32 = 1_000;
/// How often a missing folder is checked for again.
const MISSING_POLL_MS: u32 = 2_000;

/// Posted to the frame when its folder changed (wparam = frame id).
pub const WM_FOLDER_CHANGED: u32 = WM_APP + 1;
/// Posted to the frame when a background listing is done; lparam owns a
/// `Listing` (take it with `take_listing`).
pub const WM_FOLDER_LISTED: u32 = WM_APP + 6;

/// The result of `list_in_background`.
pub struct Listing {
    /// Which request this answers; older ones are dropped by the frame.
    pub generation: u64,
    pub items: Result<Vec<Entry>, FolderError>,
    /// The type groups the frame shows headers for; empty when it shows
    /// none (worked out here, off the UI thread).
    pub groups: Vec<groups::Group>,
}

/// Lists `dir` on a worker thread (a network folder can take seconds) and
/// posts `WM_FOLDER_LISTED` to `notify` with the result.
pub fn list_in_background(dir: PathBuf, notify: HWND, generation: u64) {
    let target = Shared(notify.0 as isize);
    let for_thread = dir.clone();
    let spawned = std::thread::Builder::new()
        .name("list".to_string())
        .spawn(move || post_listing(&for_thread, target, generation));
    if let Err(e) = spawned {
        // Better a short wait on this thread than a frame that stays blank.
        dlog!("listing thread not started ({e}); listing here");
        post_listing(&dir, target, generation);
    }
}

fn post_listing(dir: &Path, target: Shared, generation: u64) {
    let items = list(dir);
    let groups = match &items {
        Ok(v) => groups::header_groups(v.iter().map(|e| &e.kind)),
        Err(_) => Vec::new(),
    };
    let listing = Box::new(Listing {
        generation,
        items,
        groups,
    });
    let raw = Box::into_raw(listing);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(target.0 as *mut c_void)),
            WM_FOLDER_LISTED,
            WPARAM(0),
            LPARAM(raw as isize),
        )
    };
    if posted.is_err() {
        // The frame is gone: nobody will take it.
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// Takes the `Listing` a `WM_FOLDER_LISTED` message carries.
///
/// # Safety
/// `lp` must be the lparam of a `WM_FOLDER_LISTED` message, taken once.
pub unsafe fn take_listing(lp: LPARAM) -> Box<Listing> {
    unsafe { Box::from_raw(lp.0 as *mut Listing) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// The type group it is shown in.
    pub kind: groups::Kind,
    /// Last write time (FILETIME ticks); a new one means new contents.
    pub modified: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderError {
    /// The folder (or its drive or share) is not there right now.
    NotFound,
    Other(String),
}

/// The folder's visible items in their type groups (folders first, see
/// `groups::Kind`), each group in Explorer's name order. Hidden items are skipped, as in Explorer's default view
/// (protected system files such as `desktop.ini` are hidden too).
pub fn list(dir: &Path) -> Result<Vec<Entry>, FolderError> {
    let mut items: Vec<(Entry, Vec<u16>)> = Vec::new();
    for e in fs::read_dir(dir).map_err(classify)?.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let attrs = md.file_attributes();
        if attrs & FILE_ATTRIBUTE_HIDDEN.0 != 0 {
            continue;
        }
        let name = e.file_name();
        let key = name.encode_wide().chain([0]).collect();
        let name = name.to_string_lossy().into_owned();
        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
        let entry = Entry {
            kind: groups::kind_of(&name, is_dir),
            name,
            path: e.path(),
            is_dir,
            modified: md.last_write_time(),
        };
        items.push((entry, key));
    }
    items.sort_by(|(a, ka), (b, kb)| a.kind.cmp(&b.kind).then_with(|| explorer_order(ka, kb)));
    Ok(items.into_iter().map(|(e, _)| e).collect())
}

/// The name a frame shows: without its extension when `hide_extension` is
/// on (files only). What counts as an extension is the type groups' rule,
/// so `.gitignore` and `Meeting 10.30` stay whole.
pub fn display_name(name: &str, is_dir: bool, hide_extension: bool) -> &str {
    if !hide_extension || is_dir {
        return name;
    }
    match groups::extension(name) {
        Some(ext) => &name[..name.len() - ext.len() - 1],
        None => name,
    }
}

/// Explorer's name order: case-insensitive, digits compared as numbers.
/// Both keys are NUL-terminated UTF-16.
fn explorer_order(a: &[u16], b: &[u16]) -> Ordering {
    unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) }.cmp(&0)
}

/// Missing folders, drives that are not ready and offline shares are all
/// "not available"; anything else is reported as is.
fn classify(e: io::Error) -> FolderError {
    const NOT_THERE: [i32; 5] = [
        2,  // ERROR_FILE_NOT_FOUND
        3,  // ERROR_PATH_NOT_FOUND
        21, // ERROR_NOT_READY (removed USB drive)
        53, // ERROR_BAD_NETPATH
        67, // ERROR_BAD_NET_NAME
    ];
    if e.kind() == io::ErrorKind::NotFound
        || e.raw_os_error().is_some_and(|c| NOT_THERE.contains(&c))
    {
        FolderError::NotFound
    } else {
        FolderError::Other(e.to_string())
    }
}

/// Watches one folder on its own thread. Every change, buffer overflow,
/// disappearance or reappearance of the folder posts `WM_FOLDER_CHANGED` to
/// the frame, which then re-lists the folder. Dropping it tells the thread
/// to stop without waiting for it: on an unreachable network folder the
/// thread can be stuck in a file call for many seconds.
pub struct Watcher {
    /// (stop, released) shared with the thread; `None` if Windows could not
    /// make the events, in which case nothing is watched.
    events: Option<(Arc<Event>, Arc<Event>)>,
}

/// A manual-reset Win32 event, closed when the last owner drops it (the
/// watcher or its thread, whichever is later).
struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new(set: bool) -> Option<Arc<Event>> {
        let h = unsafe { CreateEventW(None, true, set, None) }.ok()?;
        Some(Arc::new(Event(h)))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A window handle passed to the watcher thread (valid from any thread).
#[derive(Clone, Copy)]
struct Shared(isize);
unsafe impl Send for Shared {}

impl Watcher {
    /// Starts watching. Waits up to `START_WAIT_MS` for the first watch to
    /// be armed (or the folder to be found missing), so a change made right
    /// after `start` is normally not missed.
    pub fn start(dir: PathBuf, notify: HWND, frame_id: u32) -> Watcher {
        // "released" starts set: the thread holds no folder handle yet.
        let (Some(stop), Some(released)) = (Event::new(false), Event::new(true)) else {
            dlog!("frame {frame_id}: folder watcher not started (no events)");
            return Watcher { events: None };
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let target = Shared(notify.0 as isize);
        let (stop_t, released_t) = (stop.clone(), released.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("watch {frame_id}"))
            .spawn(move || {
                let notify = HWND(target.0 as *mut c_void);
                let post = move || unsafe {
                    let _ = PostMessageW(
                        Some(notify),
                        WM_FOLDER_CHANGED,
                        WPARAM(frame_id as usize),
                        LPARAM(0),
                    );
                };
                watch(&dir, &stop_t, &released_t, notify, ready_tx, post);
            });
        if let Err(e) = spawned {
            dlog!("frame {frame_id}: folder watcher not started: {e}");
            return Watcher { events: None };
        }
        // An unreachable network folder can block the first open for a long
        // time; don't hold the UI thread for more than a moment.
        let _ = ready_rx.recv_timeout(Duration::from_millis(START_WAIT_MS));
        Watcher {
            events: Some((stop, released)),
        }
    }

    /// Stops watching and returns once the folder is no longer held open, or
    /// after `RELEASE_WAIT_MS`: its drive is about to be removed.
    pub fn release(&self) {
        if let Some((stop, released)) = &self.events {
            unsafe {
                let _ = SetEvent(stop.0);
                WaitForSingleObject(released.0, RELEASE_WAIT_MS);
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some((stop, _)) = &self.events {
            unsafe {
                let _ = SetEvent(stop.0);
            }
        }
    }
}

/// The watcher thread: watch while the folder exists, poll while it does
/// not. `released` is set whenever the thread holds no handle to the folder.
fn watch(
    dir: &Path,
    stop: &Event,
    released: &Event,
    notify: HWND,
    ready: mpsc::Sender<()>,
    post: impl Fn(),
) {
    let stop = stop.0;
    let mut ready = Some(ready);
    let mut missing = false;
    loop {
        if let Some(h) = open_dir(dir) {
            unsafe {
                let _ = ResetEvent(released.0);
            }
            if missing {
                post(); // it came back
            }
            let removal = removal_warning(notify, h);
            let stopped = watch_handle(h, stop, &mut ready, &post);
            unsafe {
                // Close first: a drive removal waits for this handle, while
                // unregistering may wait for that removal notice to finish.
                let _ = CloseHandle(h);
                let _ = SetEvent(released.0);
                if let Some(r) = removal {
                    let _ = UnregisterDeviceNotification(r);
                }
            }
            if stopped {
                return;
            }
            // The folder went away or can no longer be watched.
            post();
        }
        missing = true;
        if let Some(r) = ready.take() {
            let _ = r.send(());
        }
        if unsafe { WaitForSingleObject(stop, MISSING_POLL_MS) } == WAIT_OBJECT_0 {
            return;
        }
    }
}

/// Asks Windows to warn `notify` (`WM_DEVICECHANGE` with
/// `DBT_DEVICEQUERYREMOVE`) before the drive holding `h` is removed: an
/// open handle blocks "Safely Remove", so the frame drops its watcher then.
fn removal_warning(notify: HWND, h: HANDLE) -> Option<HDEVNOTIFY> {
    let filter = DEV_BROADCAST_HANDLE {
        dbch_size: size_of::<DEV_BROADCAST_HANDLE>() as u32,
        dbch_devicetype: DBT_DEVTYP_HANDLE.0,
        dbch_handle: h,
        ..Default::default()
    };
    unsafe {
        RegisterDeviceNotificationW(
            HANDLE(notify.0),
            &filter as *const DEV_BROADCAST_HANDLE as *const c_void,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    }
    .ok()
}

fn open_dir(dir: &Path) -> Option<HANDLE> {
    unsafe {
        CreateFileW(
            &HSTRING::from(dir.as_os_str()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
        .ok()
    }
}

/// Waits for changes on the open folder `h` until `stop` is set (returns
/// true) or the watch ends (false): the folder is gone, or no longer at the
/// path it was opened at (moved to the Recycle Bin or renamed).
fn watch_handle(
    h: HANDLE,
    stop: HANDLE,
    ready: &mut Option<mpsc::Sender<()>>,
    post: &impl Fn(),
) -> bool {
    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        return false;
    };
    let home = final_path(h);
    // DWORD-aligned, as ReadDirectoryChangesW requires.
    let mut buf = vec![0u32; CHANGE_BUFFER_BYTES / 4];
    // Only what the frame shows: names and attributes (hidden). Writes to a
    // file (a download in progress) do not change the grid.
    let filter =
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_ATTRIBUTES;
    let stopped = 'watch: loop {
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        unsafe {
            let _ = ResetEvent(event);
            let armed = ReadDirectoryChangesW(
                h,
                buf.as_mut_ptr() as *mut c_void,
                CHANGE_BUFFER_BYTES as u32,
                false,
                filter,
                None,
                Some(&mut ov),
                None,
            );
            if armed.is_err() {
                break false;
            }
        }
        if let Some(r) = ready.take() {
            let _ = r.send(());
        }
        let mut bytes = 0u32;
        // Wait for a change, checking every poll interval that the folder
        // is still where it was.
        loop {
            let woke = unsafe { WaitForMultipleObjects(&[event, stop], false, MISSING_POLL_MS) };
            if woke == WAIT_OBJECT_0 {
                break;
            }
            let stopping = woke.0 == WAIT_OBJECT_0.0 + 1;
            if stopping || final_path(h) != home {
                unsafe {
                    let _ = CancelIoEx(h, Some(&ov));
                    let _ = GetOverlappedResult(h, &ov, &mut bytes, true);
                }
                break 'watch stopping;
            }
        }
        match unsafe { GetOverlappedResult(h, &ov, &mut bytes, false) } {
            // Zero bytes means the buffer overflowed: still a change.
            Ok(()) => post(),
            Err(e) if e.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => post(),
            Err(_) => break false,
        }
        if final_path(h) != home {
            break false;
        }
    };
    unsafe {
        let _ = CloseHandle(event);
    }
    stopped
}

/// The full path an open handle currently leads to.
fn final_path(h: HANDLE) -> Option<Vec<u16>> {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) } as usize;
    (n > 0 && n < buf.len()).then(|| {
        buf.truncate(n);
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;

    use windows::core::{w, HSTRING};
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, PeekMessageW, HWND_MESSAGE, MSG, PM_REMOVE,
        WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    /// A fresh, empty directory under %TEMP% for one test.
    fn temp_dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskframe-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn hides_last_extension() {
        assert_eq!(
            display_name("report.final.pdf", false, true),
            "report.final"
        );
        assert_eq!(
            display_name("Ünïcödé 日本.txt", false, true),
            "Ünïcödé 日本"
        );
    }

    #[test]
    fn keeps_folders_whole() {
        assert_eq!(display_name("photos.2024", true, true), "photos.2024");
    }

    #[test]
    fn keeps_dot_files() {
        assert_eq!(display_name(".gitignore", false, true), ".gitignore");
        assert_eq!(display_name("README", false, true), "README");
    }

    #[test]
    fn keeps_dots_that_are_not_extensions() {
        // Same rule as the type groups: these have no extension.
        assert_eq!(display_name("Meeting 10.30", false, true), "Meeting 10.30");
        assert_eq!(display_name("v1.2 notes", false, true), "v1.2 notes");
        assert_eq!(display_name("trailing.", false, true), "trailing.");
        assert_eq!(display_name("photo.JPG", false, true), "photo");
    }

    #[test]
    fn shown_when_off() {
        assert_eq!(display_name("report.pdf", false, false), "report.pdf");
    }

    #[test]
    fn lists_dirs_first_then_names() {
        let d = temp_dir("order");
        fs::write(d.join("b.txt"), "").unwrap();
        fs::write(d.join("A.txt"), "").unwrap();
        fs::create_dir(d.join("zeta")).unwrap();
        fs::create_dir(d.join("Alpha")).unwrap();
        let got = list(&d).unwrap();
        assert_eq!(names(&got), ["Alpha", "zeta", "A.txt", "b.txt"]);
        assert!(got[0].is_dir && !got[2].is_dir);
        assert_eq!(got[2].path, d.join("A.txt"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn groups_types_in_order() {
        let d = temp_dir("groups");
        fs::create_dir(d.join("b")).unwrap();
        for n in [
            "README",
            "notes.md",
            "run.bat",
            "doc.pdf",
            "song.mp3",
            "clip.mp4",
            "a.png",
            "Meeting 10.30",
        ] {
            fs::write(d.join(n), "").unwrap();
        }
        let got = list(&d).unwrap();
        assert_eq!(
            names(&got),
            [
                "b",
                "a.png",
                "clip.mp4",
                "song.mp3",
                "doc.pdf",
                "run.bat",
                "notes.md",
                "Meeting 10.30",
                "README"
            ]
        );
        assert_eq!(got[1].kind, groups::Kind::Pictures);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn numbers_sort_like_explorer() {
        let d = temp_dir("numbers");
        for n in ["file10.txt", "file2.txt", "file1.txt"] {
            fs::write(d.join(n), "").unwrap();
        }
        assert_eq!(
            names(&list(&d).unwrap()),
            ["file1.txt", "file2.txt", "file10.txt"]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn skips_hidden_files() {
        let d = temp_dir("hidden");
        fs::write(d.join("shown.txt"), "").unwrap();
        let hidden = d.join("desktop.ini");
        fs::write(&hidden, "").unwrap();
        unsafe {
            SetFileAttributesW(&HSTRING::from(hidden.as_os_str()), FILE_ATTRIBUTE_HIDDEN).unwrap();
        }
        assert_eq!(names(&list(&d).unwrap()), ["shown.txt"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unicode_names_enumerate() {
        let d = temp_dir("unicode");
        fs::write(d.join("Ünïcödé 日本.txt"), "").unwrap();
        assert_eq!(names(&list(&d).unwrap()), ["Ünïcödé 日本.txt"]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn missing_folder_is_reported_not_panicking() {
        let d = std::env::temp_dir().join("deskframe-surely-missing-folder");
        assert_eq!(list(&d), Err(FolderError::NotFound));
    }

    /// A message-only window on this thread to receive the watcher's posts.
    fn message_window() -> HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .unwrap()
        }
    }

    /// Pumps this thread's queue until `WM_FOLDER_CHANGED` for `frame_id`
    /// arrives or `timeout` passes.
    fn wait_for_change(frame_id: u32, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut msg = MSG::default();
        while Instant::now() < end {
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_FOLDER_CHANGED && msg.wParam.0 == frame_id as usize {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Opens `dir` allowing no other handle to it; `None` if one is open.
    fn exclusive_open(dir: &Path) -> Option<HANDLE> {
        unsafe {
            CreateFileW(
                &HSTRING::from(dir.as_os_str()),
                FILE_LIST_DIRECTORY.0,
                windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
            .ok()
        }
    }

    #[test]
    fn release_lets_go_of_the_folder() {
        let d = temp_dir("release");
        let hwnd = message_window();
        let watcher = Watcher::start(d.clone(), hwnd, 6);
        assert!(
            exclusive_open(&d).is_none(),
            "the watcher should hold the folder"
        );
        watcher.release();
        let h = exclusive_open(&d).expect("folder still held after release");
        unsafe {
            CloseHandle(h).unwrap();
            DestroyWindow(hwnd).unwrap();
        }
        drop(watcher);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn removal_warning_registers_for_a_folder() {
        let d = temp_dir("removal");
        let hwnd = message_window();
        let h = open_dir(&d).expect("open folder");
        let r = removal_warning(hwnd, h).expect("registered");
        unsafe {
            UnregisterDeviceNotification(r).unwrap();
            CloseHandle(h).unwrap();
            DestroyWindow(hwnd).unwrap();
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn background_listing_arrives() {
        let d = temp_dir("background");
        fs::write(d.join("one.txt"), "").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        let hwnd = message_window();
        list_in_background(d.clone(), hwnd, 9);
        let end = Instant::now() + Duration::from_secs(2);
        let mut msg = MSG::default();
        let mut got = None;
        while got.is_none() && Instant::now() < end {
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_FOLDER_LISTED {
                    got = Some(unsafe { take_listing(msg.lParam) });
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let listing = got.expect("no WM_FOLDER_LISTED within 2 s");
        assert_eq!(listing.generation, 9);
        assert_eq!(names(listing.items.as_ref().unwrap()), ["sub", "one.txt"]);
        let kinds: Vec<_> = listing
            .groups
            .iter()
            .map(|g| (g.kind.clone(), g.len))
            .collect();
        assert_eq!(
            kinds,
            [(groups::Kind::Folders, 1), (groups::Kind::Documents, 1)]
        );
        unsafe { DestroyWindow(hwnd).unwrap() };
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn watcher_reports_bulk_delete() {
        let d = temp_dir("bulk");
        for i in 0..1_000 {
            fs::write(d.join(format!("f{i}.txt")), "").unwrap();
        }
        let hwnd = message_window();
        let watcher = Watcher::start(d.clone(), hwnd, 7);
        for i in 0..1_000 {
            fs::remove_file(d.join(format!("f{i}.txt"))).unwrap();
        }
        assert!(wait_for_change(7, Duration::from_secs(2)));
        assert_eq!(list(&d).unwrap(), Vec::new());
        drop(watcher);
        unsafe { DestroyWindow(hwnd).unwrap() };
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn watcher_reports_folder_renamed_away_and_back() {
        // Deleting to the Recycle Bin is a rename: the open handle follows
        // the folder, so the watcher must notice the path no longer leads to it.
        let d = temp_dir("renamed");
        let moved = d.with_file_name(format!(
            "{}-moved",
            d.file_name().unwrap().to_string_lossy()
        ));
        let _ = fs::remove_dir_all(&moved);
        let hwnd = message_window();
        let watcher = Watcher::start(d.clone(), hwnd, 4);
        fs::rename(&d, &moved).unwrap();
        assert!(
            wait_for_change(4, Duration::from_secs(4)),
            "rename away not reported"
        );
        fs::rename(&moved, &d).unwrap();
        assert!(
            wait_for_change(4, Duration::from_secs(4)),
            "return not reported"
        );
        drop(watcher);
        unsafe { DestroyWindow(hwnd).unwrap() };
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn watcher_ignores_writes_to_existing_files() {
        // A download or log being written must not keep re-listing the folder.
        let d = temp_dir("writes");
        let f = d.join("growing.log");
        fs::write(&f, "start").unwrap();
        let hwnd = message_window();
        let watcher = Watcher::start(d.clone(), hwnd, 5);
        for _ in 0..5 {
            let mut file = fs::OpenOptions::new().append(true).open(&f).unwrap();
            std::io::Write::write_all(&mut file, b" more").unwrap();
        }
        assert!(!wait_for_change(5, Duration::from_millis(600)));
        drop(watcher);
        unsafe { DestroyWindow(hwnd).unwrap() };
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn watcher_reports_folder_vanishing_and_returning() {
        let d = temp_dir("vanish");
        let hwnd = message_window();
        let watcher = Watcher::start(d.clone(), hwnd, 3);
        fs::remove_dir_all(&d).unwrap();
        assert!(
            wait_for_change(3, Duration::from_secs(2)),
            "vanish not reported"
        );
        fs::create_dir_all(&d).unwrap();
        assert!(
            wait_for_change(3, Duration::from_secs(4)),
            "return not reported"
        );
        drop(watcher);
        unsafe { DestroyWindow(hwnd).unwrap() };
        fs::remove_dir_all(&d).unwrap();
    }
}
