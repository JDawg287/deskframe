//! Type groups: which group each item of a frame belongs to, the order the
//! groups are shown in, and when a frame shows group headers. Pure logic;
//! the extension lists come from the type-groups design.

/// A group of items. Variant order is the order groups are shown in;
/// extension groups sort A–Z by their extension.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Folders,
    Pictures,
    Videos,
    Audio,
    Documents,
    Archives,
    Programs,
    /// Files with an extension no umbrella group covers (lowercase, no dot).
    Ext(String),
    /// Files with no extension.
    Other,
}

/// A run of items of one kind in a sorted listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub kind: Kind,
    pub len: usize,
}

const PICTURES: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "webp", "heic", "heif", "avif",
    "tif", "tiff", "ico", "svg", "svgz", "jxl", "jxr", "wdp", "hdp", "psd", "xcf", "tga", "dds",
    "exr", "hdr", "raw", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf",
    "dng", "pef", "srw", "x3f",
];
const VIDEOS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "qt", "wmv", "asf", "flv", "f4v", "mpg", "mpeg",
    "mpe", "m2v", "m2ts", "mts", "vob", "3gp", "3g2", "ogv", "hevc", "h264", "h265", "rm", "rmvb",
    "divx", "mxf",
];
const AUDIO: &[&str] = &[
    "mp3", "wav", "flac", "aac", "m4a", "m4b", "ogg", "oga", "opus", "wma", "aiff", "aif", "aifc",
    "alac", "ape", "mid", "midi", "amr", "ac3", "dts", "mka", "wv", "au", "snd", "caf",
];
const DOCUMENTS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "dot", "dotx", "odt", "ott", "rtf", "txt", "wpd", "wps", "pages",
    "epub", "mobi", "azw", "azw3", "xps", "oxps", "one", "xls", "xlsx", "xlsm", "xlsb", "ods",
    "csv", "tsv", "numbers", "ppt", "pptx", "pptm", "pps", "ppsx", "odp", "key",
];
const ARCHIVES: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "lz", "lzma", "cab",
    "iso", "img", "vhd", "vhdx", "wim", "jar", "apk", "arj", "z",
];
const PROGRAMS: &[&str] = &[
    "exe",
    "msi",
    "msix",
    "msixbundle",
    "appx",
    "appxbundle",
    "appinstaller",
    "lnk",
    "url",
    "appref-ms",
    "com",
    "scr",
    "cpl",
];

/// Longest text after the last dot that still counts as an extension.
const MAX_EXTENSION: usize = 12;

/// The extension that decides `name`'s group: the text after the last dot,
/// unless the name starts with its only dot (`.gitignore`) or the text does
/// not look like an extension (`Meeting 10.30`, `v1.2 notes`).
pub fn extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.').filter(|&d| d > 0)?;
    let ext = &name[dot + 1..];
    let fits = !ext.is_empty() && ext.chars().count() <= MAX_EXTENSION;
    let plain = !ext.chars().any(char::is_whitespace);
    (fits && plain && ext.chars().any(char::is_alphabetic)).then_some(ext)
}

/// The group an item belongs to.
pub fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folders;
    }
    let Some(ext) = extension(name) else {
        return Kind::Other;
    };
    let ext = ext.to_lowercase();
    let umbrellas = [
        (PICTURES, Kind::Pictures),
        (VIDEOS, Kind::Videos),
        (AUDIO, Kind::Audio),
        (DOCUMENTS, Kind::Documents),
        (ARCHIVES, Kind::Archives),
        (PROGRAMS, Kind::Programs),
    ];
    for (list, kind) in umbrellas {
        if list.contains(&ext.as_str()) {
            return kind;
        }
    }
    Kind::Ext(ext)
}

impl Kind {
    /// The name a header shows.
    fn label(&self) -> String {
        match self {
            Kind::Folders => "Folders".into(),
            Kind::Pictures => "Pictures".into(),
            Kind::Videos => "Videos".into(),
            Kind::Audio => "Audio".into(),
            Kind::Documents => "Documents".into(),
            Kind::Archives => "Archives".into(),
            Kind::Programs => "Programs and shortcuts".into(),
            Kind::Ext(e) => format!(".{e}"),
            Kind::Other => "Other".into(),
        }
    }

    /// True for the broad groups that cover many extensions.
    fn is_umbrella(&self) -> bool {
        !matches!(self, Kind::Folders | Kind::Ext(_) | Kind::Other)
    }

    /// True for the groups shown with a preview, like Explorer does.
    pub fn has_thumbnail(&self) -> bool {
        matches!(self, Kind::Pictures | Kind::Videos)
    }
}

/// The runs of equal kinds in already-sorted `kinds`.
fn runs<'a>(kinds: impl Iterator<Item = &'a Kind>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for kind in kinds {
        match groups.last_mut() {
            Some(g) if g.kind == *kind => g.len += 1,
            _ => groups.push(Group {
                kind: kind.clone(),
                len: 1,
            }),
        }
    }
    groups
}

/// Whether a frame with these groups shows headers: always when there are
/// several, and for a single group only if it is an umbrella group (a frame
/// of only folders, or of one other type, looks as it always did).
fn show_headers(groups: &[Group]) -> bool {
    match groups {
        [] => false,
        [only] => only.kind.is_umbrella(),
        _ => true,
    }
}

/// The groups a frame shows headers for, from its sorted items' kinds:
/// their runs, or none when `show_headers` says no.
pub fn header_groups<'a>(kinds: impl Iterator<Item = &'a Kind>) -> Vec<Group> {
    let runs = runs(kinds);
    if show_headers(&runs) {
        runs
    } else {
        Vec::new()
    }
}

/// A header's text: the group's name and how many items it holds.
pub fn title(g: &Group) -> String {
    format!("{} ({})", g.kind.label(), g.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Kind {
        kind_of(name, false)
    }

    fn ext(e: &str) -> Kind {
        Kind::Ext(e.to_string())
    }

    fn groups(kinds: &[Kind]) -> Vec<Group> {
        runs(kinds.iter())
    }

    #[test]
    fn extension_rules() {
        assert_eq!(extension("a.PNG"), Some("PNG"));
        assert_eq!(extension(".gitignore"), None);
        assert_eq!(extension("README"), None);
        assert_eq!(extension("x.tar.gz"), Some("gz"));
        assert_eq!(extension("app.appref-ms"), Some("appref-ms"));
        assert_eq!(extension("trailing."), None);
        assert_eq!(extension("x.abcdefghijklm"), None); // 13 chars
    }

    #[test]
    fn fake_extensions_are_other() {
        assert_eq!(file("Meeting 10.30"), Kind::Other);
        assert_eq!(file("v1.2 notes"), Kind::Other);
        assert_eq!(file(".gitignore"), Kind::Other);
        assert_eq!(file("README"), Kind::Other);
        assert_eq!(file("photo.JPG"), Kind::Pictures);
    }

    #[test]
    fn umbrella_samples() {
        assert_eq!(file("a.heic"), Kind::Pictures);
        assert_eq!(file("a.cr3"), Kind::Pictures);
        assert_eq!(file("a.mkv"), Kind::Videos);
        assert_eq!(file("a.m2ts"), Kind::Videos);
        assert_eq!(file("a.flac"), Kind::Audio);
        assert_eq!(file("a.docx"), Kind::Documents);
        assert_eq!(file("a.txt"), Kind::Documents);
        assert_eq!(file("a.csv"), Kind::Documents);
        assert_eq!(file("a.7z"), Kind::Archives);
        assert_eq!(file("a.lnk"), Kind::Programs);
        assert_eq!(file("a.exe"), Kind::Programs);
    }

    #[test]
    fn scripts_and_ts_get_own_groups() {
        assert_eq!(file("run.bat"), ext("bat"));
        assert_eq!(file("x.ts"), ext("ts"));
        assert_eq!(file("x.md"), ext("md"));
        assert_eq!(file("A.BAT"), file("b.bat"));
    }

    #[test]
    fn folders_are_folders() {
        assert_eq!(kind_of("photos.2024", true), Kind::Folders);
        assert_eq!(kind_of("pics.png", true), Kind::Folders);
    }

    #[test]
    fn kind_order() {
        let order = [
            Kind::Folders,
            Kind::Pictures,
            Kind::Videos,
            Kind::Audio,
            Kind::Documents,
            Kind::Archives,
            Kind::Programs,
            ext("bat"),
            ext("md"),
            Kind::Other,
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn runs_split_on_kind_change() {
        let kinds = [Kind::Folders, Kind::Folders, Kind::Pictures, ext("bat")];
        assert_eq!(
            groups(&kinds),
            [
                Group {
                    kind: Kind::Folders,
                    len: 2
                },
                Group {
                    kind: Kind::Pictures,
                    len: 1
                },
                Group {
                    kind: ext("bat"),
                    len: 1
                },
            ]
        );
        assert!(groups(&[]).is_empty());
    }

    #[test]
    fn header_rules() {
        assert!(!show_headers(&groups(&[])));
        assert!(!show_headers(&groups(&[Kind::Folders])));
        assert!(!show_headers(&groups(&[ext("bat"), ext("bat")])));
        assert!(!show_headers(&groups(&[Kind::Other])));
        assert!(show_headers(&groups(&[Kind::Pictures])));
        assert!(show_headers(&groups(&[Kind::Folders, Kind::Other])));
    }

    #[test]
    fn labels() {
        assert_eq!(Kind::Programs.label(), "Programs and shortcuts");
        assert_eq!(ext("bat").label(), ".bat");
        assert_eq!(Kind::Other.label(), "Other");
        assert!(Kind::Archives.is_umbrella() && !Kind::Folders.is_umbrella());
        assert!(!ext("bat").is_umbrella() && !Kind::Other.is_umbrella());
    }

    #[test]
    fn title_has_count() {
        let g = |kind, len| Group { kind, len };
        assert_eq!(title(&g(Kind::Pictures, 12)), "Pictures (12)");
        assert_eq!(title(&g(ext("bat"), 1)), ".bat (1)");
    }

    #[test]
    fn thumbnail_kinds() {
        assert!(file("a.png").has_thumbnail());
        assert!(file("a.HEIC").has_thumbnail());
        assert!(file("a.mp4").has_thumbnail());
        assert!(!file("a.txt").has_thumbnail());
        assert!(!file("a.pdf").has_thumbnail());
        assert!(!kind_of("pics.png", true).has_thumbnail());
    }
}
