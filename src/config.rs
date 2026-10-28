//! `config.json` beside the .exe: which frames exist, where each sits on
//! each monitor, and the app's toggles. Loading never fails: a missing file
//! gives the defaults, a broken one is moved aside first so it is never
//! overwritten. Saving is atomic (write a temp file, then replace).

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dlog;

/// Format version written into new files.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub frames: Vec<FrameConfig>,
    pub lock_all: bool,
    /// Set when a broken config could not be moved aside: nothing may be
    /// written over it this session.
    #[serde(skip)]
    pub read_only: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            version: CONFIG_VERSION,
            frames: Vec::new(),
            lock_all: false,
            read_only: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct FrameConfig {
    pub id: u32,
    pub folder: PathBuf,
    pub collapsed: bool,
    pub locked: bool,
    /// Position per monitor, keyed by the monitor's device ID.
    pub placements: BTreeMap<String, Placement>,
    /// The monitor the user last left the frame on.
    pub last_monitor: Option<String>,
    /// How much the tint covers what is behind (0 = the theme default).
    pub opacity: f32,
    /// Blur behind the frame (off = solid tint).
    pub blur: bool,
    /// Show file names without their extension.
    pub hide_extensions: bool,
}

impl Default for FrameConfig {
    fn default() -> FrameConfig {
        FrameConfig {
            id: 0,
            folder: PathBuf::new(),
            collapsed: false,
            locked: false,
            placements: BTreeMap::new(),
            last_monitor: None,
            opacity: 0.0,
            blur: true,
            hide_extensions: false,
        }
    }
}

/// Physical px relative to the monitor's top-left corner; `h` is the
/// expanded height even while the frame is collapsed. `mon_w`/`mon_h` are
/// the monitor's size and `scale` the frame's drawing scale when saved, so
/// the frame can be zoomed to keep its look at another resolution
/// (0 = unknown: an older config, used as is).
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub mon_w: i32,
    pub mon_h: i32,
    pub scale: f32,
}

/// `config.json` next to the running .exe.
pub fn config_path() -> PathBuf {
    crate::log::exe_dir().join("config.json")
}

/// The config at `path`, or the defaults when it is missing. A file that
/// cannot be read or parsed is renamed to `config.json.bad` (then `.bad2`,
/// `.bad3`, …) so the next save cannot overwrite it.
pub fn load(path: &Path) -> Config {
    let parsed = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Config>(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Config::default(),
        Err(e) => Err(e.to_string()),
    };
    match parsed {
        Ok(mut cfg) => {
            tidy(&mut cfg);
            cfg
        }
        Err(why) => {
            dlog!("config: {} unusable ({why})", path.display());
            Config {
                read_only: !move_aside(path),
                ..Config::default()
            }
        }
    }
}

/// The smallest frame id (from 1) not in `used`.
pub fn next_id(used: impl Iterator<Item = u32>) -> u32 {
    let used: HashSet<u32> = used.collect();
    (1..).find(|id| !used.contains(id)).unwrap_or(1)
}

/// Repairs what a hand edit can break without making the JSON invalid:
/// missing or repeated frame ids (Remove frame works by id), `/` in folder
/// paths (Windows' icon lookup needs `\`), and placements with no size.
fn tidy(cfg: &mut Config) {
    let mut taken = HashSet::new();
    let keeps: Vec<bool> = cfg
        .frames
        .iter()
        .map(|f| f.id != 0 && taken.insert(f.id))
        .collect();
    for (f, keep) in cfg.frames.iter_mut().zip(keeps) {
        if !keep {
            f.id = next_id(taken.iter().copied());
            taken.insert(f.id);
        }
        let wide: Vec<u16> = f
            .folder
            .as_os_str()
            .encode_wide()
            .map(|c| {
                if c == u16::from(b'/') {
                    u16::from(b'\\')
                } else {
                    c
                }
            })
            .collect();
        f.folder = PathBuf::from(OsString::from_wide(&wide));
        f.placements.retain(|_, p| p.w > 0 && p.h > 0);
    }
}

/// Renames a broken config out of the way; false if it could not be moved.
fn move_aside(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let free = (1..)
        .map(|n| {
            let suffix = if n == 1 { String::new() } else { n.to_string() };
            path.with_file_name(format!("{name}.bad{suffix}"))
        })
        .find(|p| !p.exists());
    let Some(bad) = free else { return false };
    match fs::rename(path, &bad) {
        Ok(()) => {
            dlog!("config: kept the old file as {}", bad.display());
            true
        }
        Err(e) => {
            dlog!("config: could not move it aside, so it will not be saved over: {e}");
            false
        }
    }
}

/// Writes `cfg` to a temp file, flushes it, then replaces `path` with it, so
/// a crash or power loss leaves either the old or the new file.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    if cfg.read_only {
        return Err(io::Error::other(
            "the config on disk is broken and could not be moved aside",
        ));
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(cfg).map_err(io::Error::other)?;
    let written = File::create(&tmp).and_then(|mut f| {
        f.write_all(&json)?;
        f.sync_all()
    });
    match written.and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(test: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("deskframe-config-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> Config {
        let mut placements = BTreeMap::new();
        placements.insert(
            "\\\\?\\DISPLAY#ABC1234#1&0000000&0&UID100#{00000000}".to_string(),
            Placement {
                x: 72,
                y: 72,
                w: 480,
                h: 396,
                mon_w: 5120,
                mon_h: 2160,
                scale: 1.5,
            },
        );
        Config {
            version: CONFIG_VERSION,
            frames: vec![FrameConfig {
                id: 1,
                folder: PathBuf::from("C:\\Users\\me\\Ünïcödé 日本"),
                collapsed: true,
                locked: false,
                placements,
                last_monitor: Some("\\\\?\\DISPLAY#ABC1234#1&0000000&0&UID100#{00000000}".into()),
                opacity: 0.7,
                blur: false,
                hide_extensions: true,
            }],
            lock_all: true,
            read_only: false,
        }
    }

    #[test]
    fn round_trip() {
        let d = temp_dir("round");
        let p = d.join("config.json");
        save(&p, &sample()).unwrap();
        assert_eq!(load(&p), sample());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn missing_file_gives_default() {
        let d = temp_dir("missing");
        assert_eq!(load(&d.join("config.json")), Config::default());
        assert!(!d.join("config.json.bad").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let d = temp_dir("corrupt");
        let p = d.join("config.json");
        fs::write(&p, "{not json").unwrap();
        assert_eq!(load(&p), Config::default());
        assert_eq!(
            fs::read_to_string(d.join("config.json.bad")).unwrap(),
            "{not json"
        );
        assert!(!p.exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn second_corrupt_file_keeps_the_first() {
        let d = temp_dir("corrupt2");
        let p = d.join("config.json");
        fs::write(&p, "first").unwrap();
        load(&p);
        fs::write(&p, "second").unwrap();
        load(&p);
        assert_eq!(
            fs::read_to_string(d.join("config.json.bad")).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(d.join("config.json.bad2")).unwrap(),
            "second"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let d = temp_dir("unknown");
        let p = d.join("config.json");
        fs::write(
            &p,
            r#"{"version":1,"lock_all":true,"future_setting":42,"frames":[{"id":3,"shiny":true}]}"#,
        )
        .unwrap();
        let cfg = load(&p);
        assert!(cfg.lock_all);
        assert_eq!(cfg.frames[0].id, 3);
        assert!(!cfg.frames[0].collapsed);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn save_is_atomic() {
        let d = temp_dir("atomic");
        let p = d.join("config.json");
        save(&p, &Config::default()).unwrap();
        save(&p, &sample()).unwrap();
        assert_eq!(load(&p), sample());
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"]);
        fs::remove_dir_all(&d).unwrap();
    }

    fn load_json(test: &str, json: &str) -> Config {
        let d = temp_dir(test);
        let p = d.join("config.json");
        fs::write(&p, json).unwrap();
        let cfg = load(&p);
        fs::remove_dir_all(&d).unwrap();
        cfg
    }

    #[test]
    fn duplicate_ids_are_renumbered() {
        let cfg = load_json("ids", r#"{"frames":[{"id":1},{"id":1},{}]}"#);
        let ids: Vec<u32> = cfg.frames.iter().map(|f| f.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn forward_slashes_become_backslashes() {
        let cfg = load_json(
            "slashes",
            r#"{"frames":[{"id":1,"folder":"C:/Users/me/Downloads"}]}"#,
        );
        // Compare the text: Rust paths treat / and \ alike, Windows' icon
        // lookup does not.
        assert_eq!(cfg.frames[0].folder.as_os_str(), r"C:\Users\me\Downloads");
    }

    #[test]
    fn empty_placements_are_dropped() {
        let cfg = load_json(
            "placements",
            r#"{"frames":[{"id":1,"placements":{"A":{"x":5,"y":5,"w":0,"h":300},"B":{"x":5,"y":5,"w":400,"h":300}}}]}"#,
        );
        let ids: Vec<&String> = cfg.frames[0].placements.keys().collect();
        assert_eq!(ids, ["B"]);
    }

    #[test]
    fn unmovable_bad_file_disables_saving() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = temp_dir("unmovable");
        let p = d.join("config.json");
        fs::write(&p, "{bad").unwrap();
        // Another program holds the file open without sharing: it can be
        // neither read nor moved aside.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&p)
            .unwrap();
        let cfg = load(&p);
        assert!(cfg.read_only);
        assert!(save(&p, &cfg).is_err());
        drop(held);
        assert_eq!(fs::read_to_string(&p).unwrap(), "{bad");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn next_id_is_the_smallest_free_one() {
        assert_eq!(next_id([].into_iter()), 1);
        assert_eq!(next_id([1, 2, 4].into_iter()), 3);
        assert_eq!(next_id([3, 1, 2].into_iter()), 4);
        // A hand-edited huge id does not wrap around to 0.
        assert_eq!(next_id([u32::MAX, 1].into_iter()), 2);
    }

    #[test]
    fn old_config_gets_default_appearance() {
        let cfg = load_json("appearance", r#"{"frames":[{"id":1,"folder":"C:\\x"}]}"#);
        let f = &cfg.frames[0];
        assert_eq!((f.opacity, f.blur, f.hide_extensions), (0.0, true, false));
    }
}
