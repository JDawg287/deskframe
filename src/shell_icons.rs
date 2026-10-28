//! Shell icons, exactly as Explorer shows them: the system image list gives
//! each file an index (files of a type share one), and each index becomes a
//! Direct2D bitmap once, made by the shell at the exact pixel size the frame
//! draws (no 256 px copies are kept around).
//!
//! Use it from one thread (the UI thread): calls made from several threads
//! at once while the system image list starts up can fail (seen in tests).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, HSTRING, PCWSTR};

use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ, HPALETTE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapIgnoreAlpha, WICBitmapInterpolationModeHighQualityCubic,
    WICBitmapPaletteTypeMedianCut, WICBitmapUseAlpha, WICBitmapUsePremultipliedAlpha,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IBindCtx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHDefExtractIconW, SHGetFileInfoW,
    SHGetStockIconInfo, SHFILEINFOW, SHGFI_SYSICONINDEX, SHGSI_ICONLOCATION, SHSTOCKICONINFO,
    SIID_DOCNOASSOC, SIIGBF_ICONONLY, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, PostMessageW, HICON, WM_APP};

use crate::dlog;
use crate::folder::Entry;

/// Icon bitmaps kept at most (about 20 KB each at 72 px); the least
/// recently used go first. All frames share one renderer and so one cache,
/// so frames drawn at different sizes must fit in it together.
const MAX_BITMAPS: usize = 256;
/// Previews kept at most (each its own file, up to about 20 KB at 72 px).
/// Enough for what frames show at once; older ones reload quickly from
/// Windows' thumbnail cache.
const MAX_THUMBNAILS: usize = 128;

/// Where one preview stands.
enum Thumb {
    /// Asked for at this time; the thread has not answered yet.
    Asked(Instant),
    /// The file has no thumbnail: it keeps its icon.
    Failed,
    /// Made by the thread, not yet a Direct2D bitmap (owned here).
    Raw(HBITMAP),
    Ready(ID2D1Bitmap),
}

impl Thumb {
    fn free(self) {
        if let Thumb::Raw(h) = self {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(h.0));
            }
        }
    }
}

/// Message a frame gets when a thumbnail it asked for is done; lparam owns
/// it (take it with `take_thumbnail`).
pub const WM_THUMBNAIL: u32 = WM_APP + 9;

/// One file's preview at one size. A new modified time is a new key, so an
/// edited picture gets a new preview.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    pub path: PathBuf,
    pub modified: u64,
    pub px: u32,
}

impl ThumbKey {
    /// The preview key for `entry` at `px`, if its kind gets previews.
    pub fn of(entry: &Entry, px: u32) -> Option<ThumbKey> {
        entry.kind.has_thumbnail().then(|| ThumbKey {
            path: entry.path.clone(),
            modified: entry.modified,
            px,
        })
    }
}

/// The shell's thumbnail of `path` at most `px` square (Explorer's own,
/// from its cache when it has one). Fails when the file has no thumbnail.
pub fn make_thumbnail(path: &Path, px: u32) -> Result<HBITMAP> {
    let side = px as i32;
    unsafe {
        let item: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None::<&IBindCtx>)?;
        item.GetImage(SIZE { cx: side, cy: side }, SIIGBF_THUMBNAILONLY)
    }
}

/// A finished thumbnail on its way to the frame that asked. It owns the
/// bitmap until `take_image`; one nobody took is deleted with it.
pub struct ThumbDone {
    pub key: ThumbKey,
    image: Option<isize>,
}

impl ThumbDone {
    pub fn take_image(&mut self) -> Option<HBITMAP> {
        self.image.take().map(|h| HBITMAP(h as *mut c_void))
    }
}

impl Drop for ThumbDone {
    fn drop(&mut self) {
        if let Some(h) = self.take_image() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(h.0));
            }
        }
    }
}

/// Takes the `ThumbDone` a `WM_THUMBNAIL` message carries.
///
/// # Safety
/// `lp` must be the lparam of a `WM_THUMBNAIL` message, taken once.
pub unsafe fn take_thumbnail(lp: LPARAM) -> Box<ThumbDone> {
    unsafe { Box::from_raw(lp.0 as *mut ThumbDone) }
}

/// Sends a `ThumbDone` to `target` again (it could not be taken yet); if
/// that fails, it is dropped and its bitmap freed.
pub fn repost_thumbnail(target: HWND, done: Box<ThumbDone>) {
    let raw = Box::into_raw(done);
    let posted =
        unsafe { PostMessageW(Some(target), WM_THUMBNAIL, WPARAM(0), LPARAM(raw as isize)) };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// One request: the preview wanted and the window to tell.
struct Request(ThumbKey, isize);

/// How long an unanswered request waits before it may be asked again (its
/// answer can be lost: the frame that asked was closed, say).
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(30);

/// Requests waiting for the thumbnail thread. The newest goes first: it is
/// what a frame shows now, older ones may have scrolled away.
struct Pending<T>(Vec<T>);

impl<T> Pending<T> {
    fn push(&mut self, r: T) {
        self.0.push(r);
    }

    fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }
}

/// The thumbnail thread's waiting requests and the signal that wakes it.
type Queue = Arc<(Mutex<Pending<Request>>, Condvar)>;

/// Asks the thumbnail thread for `key`; `notify` gets `WM_THUMBNAIL`.
/// One shared thread makes them, newest first (the shell's thumbnail work
/// can take a while; the UI thread never waits for it).
pub fn request_thumbnail(key: ThumbKey, notify: HWND) {
    static WORKER: OnceLock<Option<Queue>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let queue: Queue = Arc::new((Mutex::new(Pending(Vec::new())), Condvar::new()));
        let for_thread = queue.clone();
        let spawned = std::thread::Builder::new()
            .name("thumbnails".to_string())
            .spawn(move || make_thumbnails(&for_thread));
        match spawned {
            Ok(_) => Some(queue),
            Err(e) => {
                dlog!("thumbnail thread not started ({e}); files keep their icons");
                None
            }
        }
    });
    let target = notify.0 as isize;
    match worker {
        Some(queue) => {
            let (pending, wake) = &**queue;
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Request(key, target));
            wake.notify_one();
        }
        None => post_thumbnail(target, key, None),
    }
}

fn make_thumbnails(queue: &Queue) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let (pending, wake) = &**queue;
    loop {
        let next = {
            let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match p.pop() {
                    Some(r) => break r,
                    None => p = wake.wait(p).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        let Request(key, target) = next;
        let image = make_thumbnail(&key.path, key.px).ok();
        post_thumbnail(target, key, image);
    }
}

fn post_thumbnail(target: isize, key: ThumbKey, image: Option<HBITMAP>) {
    let done = Box::new(ThumbDone {
        key,
        image: image.map(|h| h.0 as isize),
    });
    let raw = Box::into_raw(done);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(target as *mut c_void)),
            WM_THUMBNAIL,
            WPARAM(0),
            LPARAM(raw as isize),
        )
    };
    if posted.is_err() {
        // The frame is gone: drop it here, bitmap and all.
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// An index into the system image list. Files of the same type share one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IconKey(pub i32);

/// The shell could not say which icon a file has; it shows the stock icon.
pub const UNKNOWN: IconKey = IconKey(-1);

/// The icon Explorer shows for `path`, or `UNKNOWN`.
pub fn icon_key(path: &Path) -> IconKey {
    let mut info = SHFILEINFOW::default();
    let found = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(path.as_os_str()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        )
    };
    if found != 0 {
        IconKey(info.iIcon)
    } else {
        UNKNOWN
    }
}

/// Direct2D bitmaps of shell icons for one render target, made on first use
/// and kept per icon and pixel size.
pub struct IconCache {
    wic: Option<IWICImagingFactory>,
    capacity: usize,
    /// Bitmap per (icon, pixel size) with the tick it was last used.
    bitmaps: HashMap<(IconKey, u32), (ID2D1Bitmap, u64)>,
    /// Windows' stock document icon per pixel size: the fallback, kept apart
    /// so it never stands in for a real icon key.
    stock: HashMap<u32, Option<ID2D1Bitmap>>,
    /// Items whose icon could not be made (locked, gone, network blip):
    /// they show the stock icon without retrying on every paint, while other
    /// files of the same type still get the real icon.
    failed: HashSet<PathBuf>,
    /// Previews with the tick they were last used.
    thumbs: HashMap<ThumbKey, (Thumb, u64)>,
    tick: u64,
}

impl Drop for IconCache {
    fn drop(&mut self) {
        for (_, (t, _)) in self.thumbs.drain() {
            t.free();
        }
    }
}

impl IconCache {
    pub fn new() -> IconCache {
        IconCache::with_capacity(MAX_BITMAPS)
    }

    fn with_capacity(capacity: usize) -> IconCache {
        IconCache {
            wic: None,
            capacity,
            bitmaps: HashMap::new(),
            stock: HashMap::new(),
            failed: HashSet::new(),
            thumbs: HashMap::new(),
            tick: 0,
        }
    }

    /// The icon `key` as a `px`×`px` bitmap for `rt`, made from `path` (any
    /// item with that key looks the same). An icon the shell cannot supply
    /// shows the stock document icon; `None` only if even that fails.
    pub fn get(
        &mut self,
        key: IconKey,
        path: &Path,
        px: u32,
        rt: &ID2D1RenderTarget,
    ) -> Option<ID2D1Bitmap> {
        self.tick += 1;
        if key == UNKNOWN || self.failed.contains(path) {
            return self.stock(px, rt);
        }
        if let Some((bitmap, used)) = self.bitmaps.get_mut(&(key, px)) {
            *used = self.tick;
            return Some(bitmap.clone());
        }
        match self.load_item(path, px, rt) {
            Ok(bitmap) => {
                self.keep((key, px), bitmap.clone());
                Some(bitmap)
            }
            Err(e) => {
                dlog!("icon for {} at {px} px: {e}", path.display());
                if self.failed.len() >= self.capacity {
                    self.failed.clear();
                }
                self.failed.insert(path.to_path_buf());
                self.stock(px, rt)
            }
        }
    }

    /// Stores a bitmap, first dropping the least recently used one if full.
    fn keep(&mut self, slot: (IconKey, u32), bitmap: ID2D1Bitmap) {
        if self.bitmaps.len() >= self.capacity {
            let oldest = self
                .bitmaps
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                self.bitmaps.remove(&k);
            }
        }
        self.bitmaps.insert(slot, (bitmap, self.tick));
    }

    /// True the first time `key` is seen (the caller then asks for it);
    /// later calls say it is already asked for, ready or failed.
    pub fn wants_thumbnail(&mut self, key: &ThumbKey) -> bool {
        self.wants_thumbnail_at(key, Instant::now())
    }

    /// `wants_thumbnail` at time `now`.
    fn wants_thumbnail_at(&mut self, key: &ThumbKey, now: Instant) -> bool {
        self.tick += 1;
        if let Some((state, used)) = self.thumbs.get_mut(key) {
            *used = self.tick;
            return match state {
                Thumb::Asked(at) if now.duration_since(*at) >= ASK_AGAIN_AFTER => {
                    *at = now;
                    true
                }
                _ => false,
            };
        }
        self.keep_thumb(key.clone(), Thumb::Asked(now));
        true
    }

    /// Takes the worker's answer for `key`: its bitmap, or `None` if the
    /// file has no thumbnail (it keeps its icon).
    pub fn thumbnail_ready(&mut self, key: ThumbKey, image: Option<HBITMAP>) {
        self.tick += 1;
        if let Some((old, _)) = self.thumbs.remove(&key) {
            old.free();
        }
        self.keep_thumb(key, image.map_or(Thumb::Failed, Thumb::Raw));
    }

    /// The preview for `key` at its own pixel size, once it is ready.
    pub fn thumbnail(&mut self, key: &ThumbKey, rt: &ID2D1RenderTarget) -> Option<ID2D1Bitmap> {
        self.tick += 1;
        let tick = self.tick;
        let (state, used) = self.thumbs.get_mut(key)?;
        *used = tick;
        let raw = match state {
            Thumb::Ready(b) => return Some(b.clone()),
            Thumb::Raw(h) => *h,
            Thumb::Asked(_) | Thumb::Failed => return None,
        };
        let made = self
            .thumbnail_source(raw)
            .and_then(|source| unsafe { rt.CreateBitmapFromWicBitmap(&source, None) });
        unsafe {
            let _ = DeleteObject(HGDIOBJ(raw.0));
        }
        let (next, bitmap) = match made {
            Ok(b) => (Thumb::Ready(b.clone()), Some(b)),
            Err(e) => {
                dlog!("preview of {}: {e}", key.path.display());
                (Thumb::Failed, None)
            }
        };
        self.thumbs.insert(key.clone(), (next, tick));
        bitmap
    }

    /// Stores a preview, first dropping the least recently used if full.
    fn keep_thumb(&mut self, key: ThumbKey, t: Thumb) {
        if self.thumbs.len() >= MAX_THUMBNAILS {
            let oldest = self
                .thumbs
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            if let Some((old, _)) = oldest.and_then(|k| self.thumbs.remove(&k)) {
                old.free();
            }
        }
        self.thumbs.insert(key, (t, self.tick));
    }

    /// A shell thumbnail as premultiplied BGRA. Thumbnails of pictures
    /// without transparency can come with every alpha at zero: those are
    /// read as opaque, or they would not show at all.
    fn thumbnail_source(&mut self, h: HBITMAP) -> Result<IWICBitmapSource> {
        let wic = self.wic()?;
        unsafe {
            let mut bitmap = wic.CreateBitmapFromHBITMAP(
                h,
                HPALETTE::default(),
                WICBitmapUsePremultipliedAlpha,
            )?;
            let (mut w, mut hgt) = (0, 0);
            bitmap.GetSize(&mut w, &mut hgt)?;
            let mut pixels = vec![0u8; (w * hgt * 4) as usize];
            bitmap.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
            if pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
                bitmap =
                    wic.CreateBitmapFromHBITMAP(h, HPALETTE::default(), WICBitmapIgnoreAlpha)?;
            }
            let converter = wic.CreateFormatConverter()?;
            converter.Initialize(
                &bitmap,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;
            converter.cast()
        }
    }

    /// The stock document icon at `px`.
    fn stock(&mut self, px: u32, rt: &ID2D1RenderTarget) -> Option<ID2D1Bitmap> {
        if let Some(b) = self.stock.get(&px) {
            return b.clone();
        }
        let bitmap = self
            .load_stock(px, rt)
            .inspect_err(|e| dlog!("stock icon at {px} px: {e}"))
            .ok();
        self.stock.insert(px, bitmap.clone());
        bitmap
    }

    /// The shell's icon for `path`, rendered by the shell at `px`.
    fn load_item(&mut self, path: &Path, px: u32, rt: &ID2D1RenderTarget) -> Result<ID2D1Bitmap> {
        let side = px as i32;
        unsafe {
            let item: IShellItemImageFactory =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None::<&IBindCtx>)?;
            let hbitmap = item.GetImage(SIZE { cx: side, cy: side }, SIIGBF_ICONONLY)?;
            let wic_bitmap = self.wic()?.CreateBitmapFromHBITMAP(
                hbitmap,
                HPALETTE::default(),
                WICBitmapUseAlpha,
            );
            let _ = DeleteObject(HGDIOBJ(hbitmap.0));
            self.make_bitmap(&wic_bitmap?.cast()?, px, rt)
        }
    }

    /// Windows' "no associated app" document icon, extracted at `px`.
    fn load_stock(&mut self, px: u32, rt: &ID2D1RenderTarget) -> Result<ID2D1Bitmap> {
        let mut info = SHSTOCKICONINFO {
            cbSize: size_of::<SHSTOCKICONINFO>() as u32,
            ..Default::default()
        };
        let mut hicon = HICON::default();
        unsafe {
            SHGetStockIconInfo(SIID_DOCNOASSOC, SHGSI_ICONLOCATION, &mut info)?;
            SHDefExtractIconW(
                PCWSTR(info.szPath.as_ptr()),
                info.iIcon,
                0,
                Some(&mut hicon),
                None,
                px,
            )
            .ok()?;
            let wic_bitmap = self.wic()?.CreateBitmapFromHICON(hicon);
            let _ = DestroyIcon(hicon);
            self.make_bitmap(&wic_bitmap?.cast()?, px, rt)
        }
    }

    /// Premultiplied BGRA at exactly `px`×`px`, as a bitmap for `rt`.
    fn make_bitmap(
        &mut self,
        source: &IWICBitmapSource,
        px: u32,
        rt: &ID2D1RenderTarget,
    ) -> Result<ID2D1Bitmap> {
        let wic = self.wic()?;
        unsafe {
            let converter = wic.CreateFormatConverter()?;
            converter.Initialize(
                source,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;
            let (mut w, mut h) = (0, 0);
            converter.GetSize(&mut w, &mut h)?;
            let sized: IWICBitmapSource = if (w, h) == (px, px) {
                converter.cast()?
            } else {
                let scaler = wic.CreateBitmapScaler()?;
                scaler.Initialize(
                    &converter,
                    px,
                    px,
                    WICBitmapInterpolationModeHighQualityCubic,
                )?;
                scaler.cast()?
            };
            rt.CreateBitmapFromWicBitmap(&sized, None)
        }
    }

    fn wic(&mut self) -> Result<IWICImagingFactory> {
        if self.wic.is_none() {
            self.wic = Some(unsafe {
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?
            });
        }
        Ok(self.wic.clone().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    /// COM for this test thread, and a turn: icon tests run one at a time,
    /// like the app's single UI thread (see the module note).
    fn shell() -> std::sync::MutexGuard<'static, ()> {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp_file(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskframe-icons-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        fs::write(&p, "").unwrap();
        p
    }

    /// A software DC render target, as the app uses (bound to a memory DC).
    fn render_target() -> ID2D1RenderTarget {
        unsafe {
            let f: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let t = f.CreateDCRenderTarget(&props).unwrap();
            let dc = CreateCompatibleDC(None);
            t.BindDC(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
            )
            .unwrap();
            let _ = DeleteDC(dc);
            t.into()
        }
    }

    #[test]
    fn icon_key_for_txt_is_stable() {
        let _turn = shell();
        let a = icon_key(&temp_file("a.txt"));
        let b = icon_key(&temp_file("b.txt"));
        assert_eq!(a, b);
        assert!(a.0 >= 0);
    }

    #[test]
    fn folder_and_file_keys_differ() {
        let _turn = shell();
        let file = temp_file("c.txt");
        assert_ne!(icon_key(&file), icon_key(file.parent().unwrap()));
    }

    #[test]
    fn unknown_icon_shows_the_stock_icon() {
        let _turn = shell();
        let rt = render_target();
        let mut cache = IconCache::new();
        let missing = Path::new(r"Z:\no-such-folder\x.unknownext");
        assert_eq!(icon_key(missing), UNKNOWN);
        let bmp = cache.get(UNKNOWN, missing, 48, &rt).expect("stock icon");
        let size = unsafe { bmp.GetPixelSize() };
        assert_eq!((size.width, size.height), (48, 48));
        assert!(
            cache.bitmaps.is_empty(),
            "the stock icon is not cached under a key"
        );
    }

    #[test]
    fn failed_item_does_not_poison_its_file_type() {
        let _turn = shell();
        let rt = render_target();
        let mut cache = IconCache::new();
        let good = temp_file("f.txt");
        let key = icon_key(&good);
        // First file of the type cannot be read (gone): stock icon, no entry.
        let gone = good.with_file_name("gone.txt");
        cache.get(key, &gone, 48, &rt).expect("stock icon");
        assert!(!cache.bitmaps.contains_key(&(key, 48)));
        // The next file of the same type gets the real icon.
        cache.get(key, &good, 48, &rt).expect("real icon");
        assert!(cache.bitmaps.contains_key(&(key, 48)));
    }

    #[test]
    fn bitmap_has_requested_pixel_size() {
        let _turn = shell();
        let rt = render_target();
        let mut cache = IconCache::new();
        let path = temp_file("d.txt");
        for px in [48, 72, 96] {
            // The shell's own image, not the stock fallback.
            let bmp = cache
                .load_item(&path, px, &rt)
                .expect("icon from the shell");
            let size = unsafe { bmp.GetPixelSize() };
            assert_eq!((size.width, size.height), (px, px));
        }
    }

    #[test]
    fn cache_drops_the_least_recently_used_when_full() {
        let _turn = shell();
        let rt = render_target();
        let mut cache = IconCache::with_capacity(2);
        let path = temp_file("e.txt");
        let key = icon_key(&path);
        cache.get(key, &path, 72, &rt).expect("72");
        cache.get(key, &path, 36, &rt).expect("36");
        cache.get(key, &path, 72, &rt).expect("72 again"); // 36 is now the oldest
        cache.get(key, &path, 54, &rt).expect("54");
        let mut sizes: Vec<u32> = cache.bitmaps.keys().map(|&(_, px)| px).collect();
        sizes.sort();
        assert_eq!(sizes, [54, 72]);
    }

    /// A `w`×`h` 24-bit bitmap file filled with one colour.
    fn bmp_file(name: &str, w: u32, h: u32) -> PathBuf {
        let p = temp_file(name);
        let row = (w * 3).div_ceil(4) * 4;
        let size = 54 + row * h;
        let mut b = Vec::with_capacity(size as usize);
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&(w as i32).to_le_bytes());
        b.extend_from_slice(&(h as i32).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(&[0u8; 24]);
        for _ in 0..h {
            for _ in 0..w {
                b.extend_from_slice(&[0x30, 0x40, 0xD0]);
            }
            b.resize(b.len() + (row - w * 3) as usize, 0);
        }
        fs::write(&p, b).unwrap();
        p
    }

    fn entry(name: &str, modified: u64) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(format!(r"C:\pics\{name}")),
            is_dir: false,
            kind: crate::groups::kind_of(name, false),
            modified,
        }
    }

    fn bitmap_size(h: HBITMAP) -> (i32, i32) {
        use windows::Win32::Graphics::Gdi::{GetObjectW, BITMAP};
        let mut bm = BITMAP::default();
        unsafe {
            GetObjectW(
                HGDIOBJ(h.0),
                size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut _),
            )
        };
        (bm.bmWidth, bm.bmHeight.abs())
    }

    #[test]
    fn thumb_key_changes_with_modified_time() {
        let a = ThumbKey::of(&entry("a.png", 1), 48).expect("pictures get previews");
        let b = ThumbKey::of(&entry("a.png", 2), 48).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.px, 48);
        assert_eq!(ThumbKey::of(&entry("a.txt", 1), 48), None);
    }

    #[test]
    fn text_file_has_no_thumbnail() {
        let _turn = shell();
        assert!(make_thumbnail(&temp_file("t.txt"), 48).is_err());
    }

    #[test]
    fn bitmap_file_thumbnail_keeps_its_shape() {
        let _turn = shell();
        let h = make_thumbnail(&bmp_file("wide.bmp", 200, 100), 48).expect("a thumbnail");
        let size = bitmap_size(h);
        unsafe {
            let _ = DeleteObject(HGDIOBJ(h.0));
        }
        assert_eq!(size, (48, 24));
    }

    #[test]
    fn opaque_bitmap_thumbnail_is_opaque() {
        let _turn = shell();
        let rt = render_target();
        let mut cache = IconCache::new();
        let h = make_thumbnail(&bmp_file("opaque.bmp", 32, 32), 32).expect("a thumbnail");
        let source = cache.thumbnail_source(h).expect("a source");
        unsafe {
            let _ = DeleteObject(HGDIOBJ(h.0));
        }
        let mut px = [0u8; 4];
        let rect = windows::Win32::Graphics::Imaging::WICRect {
            X: 16,
            Y: 16,
            Width: 1,
            Height: 1,
        };
        unsafe { source.CopyPixels(&rect, 4, &mut px).unwrap() };
        assert_eq!(px[3], 255, "BGRA {px:?}");
        let _ = rt;
    }

    #[test]
    fn thumbnail_asked_once() {
        let mut cache = IconCache::new();
        let key = ThumbKey::of(&entry("b.png", 1), 48).unwrap();
        assert!(cache.wants_thumbnail(&key));
        assert!(!cache.wants_thumbnail(&key));
    }

    #[test]
    fn newest_request_goes_first() {
        let mut p = Pending(Vec::new());
        p.push(1);
        p.push(2);
        p.push(3);
        assert_eq!(p.pop(), Some(3));
        p.push(4);
        assert_eq!(p.pop(), Some(4));
        assert_eq!(p.pop(), Some(2));
        assert_eq!(p.pop(), Some(1));
        assert_eq!(p.pop(), None);
    }

    #[test]
    fn lost_answer_is_asked_again() {
        let mut cache = IconCache::new();
        let key = ThumbKey::of(&entry("c.png", 1), 48).unwrap();
        let t0 = Instant::now();
        assert!(cache.wants_thumbnail_at(&key, t0));
        assert!(!cache.wants_thumbnail_at(&key, t0 + Duration::from_secs(5)));
        let later = t0 + ASK_AGAIN_AFTER + Duration::from_secs(1);
        assert!(
            cache.wants_thumbnail_at(&key, later),
            "no answer: ask again"
        );
        // An answered key is never asked again.
        cache.thumbnail_ready(key.clone(), None);
        assert!(!cache.wants_thumbnail_at(&key, later + ASK_AGAIN_AFTER * 2));
    }

    #[test]
    fn at_most_128_previews_are_kept() {
        let mut cache = IconCache::new();
        for n in 0..129 {
            let key = ThumbKey::of(&entry(&format!("p{n}.png"), 1), 48).unwrap();
            cache.wants_thumbnail(&key);
        }
        assert_eq!(cache.thumbs.len(), 128);
        // The oldest went first.
        let first = ThumbKey::of(&entry("p0.png", 1), 48).unwrap();
        assert!(!cache.thumbs.contains_key(&first));
    }
}
