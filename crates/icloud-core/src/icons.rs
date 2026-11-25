//! Finding an icon the way the file manager does.
//!
//! A notification can name its icon (`folder-remote`) and leave the lookup to
//! the notification server, but servers do not all use the desktop's icon
//! theme: a Qt-based shell resolves names through Qt, mako through its own
//! setting. The notification then shows a different folder from the one Files
//! draws. So the name is looked up here, in the GTK icon theme Files uses,
//! following the theme's `Inherits` chain as the freedesktop icon theme
//! specification describes, and the server gets the file.

use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

/// Directories icon themes live in, most specific first.
fn search_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());

    let mut roots = Vec::new();
    roots.extend(home.map(|h| h.join(".icons")));
    roots.extend(data_home.map(|d| d.join("icons")));
    roots.extend(data_dirs.split(':').filter(|d| d.starts_with('/')).map(|d| Path::new(d).join("icons")));
    roots
}

/// The icon theme GTK applications use: `gtk-icon-theme-name` in GTK's
/// `settings.ini`, as written by the appearance settings.
fn gtk_icon_theme() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|version| {
        let text = fs::read_to_string(config.join(version).join("settings.ini")).ok()?;
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "gtk-icon-theme-name").then(|| value.trim().trim_matches('"').to_owned())
        })
    })
}

/// The file for icon `name` at about `size` pixels in the desktop's theme.
pub fn lookup(name: &str, size: u32) -> Option<PathBuf> {
    let theme = gtk_icon_theme().unwrap_or_else(|| "hicolor".to_owned());
    lookup_in(&theme, &search_roots(), name, size)
}

/// One directory of a theme, as its `index.theme` describes it.
#[derive(Debug)]
struct ThemeDir {
    path: String,
    size: u32,
    scale: u32,
    scalable: bool,
    min: u32,
    max: u32,
}

impl ThemeDir {
    /// How far this directory is from `size` (0 = a perfect match).
    fn distance(&self, size: u32) -> u32 {
        if self.scalable {
            if (self.min..=self.max).contains(&size) { 0 } else { self.min.abs_diff(size).min(self.max.abs_diff(size)) }
        } else {
            (self.size * self.scale).abs_diff(size)
        }
    }
}

/// A theme's directories and the themes it inherits from.
struct Theme {
    roots: Vec<PathBuf>,
    dirs: Vec<ThemeDir>,
    inherits: Vec<String>,
}

fn load_theme(name: &str, roots: &[PathBuf]) -> Option<Theme> {
    let theme_roots: Vec<PathBuf> = roots.iter().map(|r| r.join(name)).filter(|p| p.is_dir()).collect();
    let index = theme_roots.iter().find_map(|r| fs::read_to_string(r.join("index.theme")).ok())?;

    let mut inherits = Vec::new();
    let mut directories = Vec::new();
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in index.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((section.to_owned(), Vec::new()));
        } else if let (Some((key, value)), Some((_, entries))) = (line.split_once('='), sections.last_mut()) {
            entries.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let list = |value: &str| value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    for (section, entries) in &sections {
        if section == "Icon Theme" {
            for (key, value) in entries {
                match key.as_str() {
                    "Inherits" => inherits = list(value),
                    "Directories" | "ScaledDirectories" => directories.extend(list(value)),
                    _ => {}
                }
            }
        }
    }
    let dirs = directories
        .iter()
        .filter_map(|dir| {
            let (_, entries) = sections.iter().find(|(s, _)| s == dir)?;
            let get = |key: &str| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
            let number = |key: &str| get(key).and_then(|v| v.parse::<u32>().ok());
            let size = number("Size")?;
            let scalable = get("Type") == Some("Scalable");
            Some(ThemeDir {
                path: dir.clone(),
                size,
                scale: number("Scale").unwrap_or(1),
                scalable,
                min: number("MinSize").unwrap_or(size),
                max: number("MaxSize").unwrap_or(size),
            })
        })
        .collect();
    Some(Theme { roots: theme_roots, dirs, inherits })
}

/// [`lookup`] with the theme and the search roots given.
fn lookup_in(theme: &str, roots: &[PathBuf], name: &str, size: u32) -> Option<PathBuf> {
    // Breadth first along `Inherits`, then hicolor, every theme once.
    let mut queue = VecDeque::from([theme.to_owned()]);
    let mut seen = HashSet::new();
    let mut hicolor_queued = false;
    loop {
        let Some(current) = queue.pop_front() else {
            if hicolor_queued {
                return None;
            }
            hicolor_queued = true;
            queue.push_back("hicolor".to_owned());
            continue;
        };
        if !seen.insert(current.clone()) {
            continue;
        }
        hicolor_queued |= current == "hicolor";
        let Some(loaded) = load_theme(&current, roots) else { continue };
        if let Some(found) = best_in(&loaded, name, size) {
            return Some(found);
        }
        queue.extend(loaded.inherits);
    }
}

fn best_in(theme: &Theme, name: &str, size: u32) -> Option<PathBuf> {
    // A fixed size that matches beats a scalable directory that merely covers it.
    let mut best: Option<((u32, bool), PathBuf)> = None;
    for dir in &theme.dirs {
        for root in &theme.roots {
            for extension in ["svg", "png"] {
                let candidate = root.join(&dir.path).join(format!("{name}.{extension}"));
                if !candidate.is_file() {
                    continue;
                }
                let distance = (dir.distance(size), dir.scalable);
                if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                    best = Some((distance, candidate));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn theme(root: &Path, name: &str, inherits: &str, dirs: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let names: Vec<&str> = dirs.iter().map(|(d, _)| *d).collect();
        let mut index = format!("[Icon Theme]\nName={name}\nInherits={inherits}\nDirectories={}\n", names.join(","));
        for (path, section) in dirs {
            let _ = write!(index, "\n[{path}]\n{section}\n");
        }
        fs::write(dir.join("index.theme"), index).unwrap();
    }

    fn icon(root: &Path, theme: &str, dir: &str, file: &str) -> PathBuf {
        let path = root.join(theme).join(dir).join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"x").unwrap();
        path
    }

    /// Like the desktop this was written for: a theme of folder colours that
    /// inherits the folder icons it lacks from Yaru, then Adwaita.
    fn desktop() -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let (home, system) = (dir.path().join("home"), dir.path().join("system"));
        theme(&home, "Filo-orange", "Filo,Yaru-dark,Adwaita,hicolor", &[("48x48/places", "Size=48\nType=Fixed")]);
        icon(&home, "Filo-orange", "48x48/places", "folder-documents.png");
        theme(&home, "Filo", "hicolor", &[("scalable/places", "Size=16\nMinSize=8\nMaxSize=512\nType=Scalable")]);
        icon(&home, "Filo", "scalable/places", "folder-remote-symbolic.svg");
        theme(
            &system,
            "Yaru-dark",
            "Yaru",
            &[("16x16/places", "Size=16\nType=Fixed"), ("48x48/places", "Size=48\nType=Fixed")],
        );
        icon(&system, "Yaru-dark", "16x16/places", "folder-remote.png");
        icon(&system, "Yaru-dark", "48x48/places", "folder-remote.png");
        theme(&system, "Adwaita", "hicolor", &[("scalable/places", "Size=16\nMinSize=8\nMaxSize=512\nType=Scalable")]);
        icon(&system, "Adwaita", "scalable/places", "folder-remote.svg");
        icon(&system, "Adwaita", "scalable/places", "network-server.svg");
        theme(&system, "hicolor", "", &[("48x48/apps", "Size=48\nType=Fixed")]);
        icon(&system, "hicolor", "48x48/apps", "some-app.png");
        (dir, vec![home, system])
    }

    #[test]
    fn an_icon_the_theme_lacks_comes_from_the_first_theme_it_inherits_that_has_it() {
        let (dir, roots) = desktop();
        let found = lookup_in("Filo-orange", &roots, "folder-remote", 48).unwrap();
        assert_eq!(
            found,
            dir.path().join("system/Yaru-dark/48x48/places/folder-remote.png"),
            "Yaru, as Files shows it, not Adwaita"
        );
    }

    #[test]
    fn an_exact_fixed_size_beats_a_scalable_directory_that_covers_it() {
        let dir = tempfile::tempdir().unwrap();
        theme(
            dir.path(),
            "Y",
            "",
            &[
                ("256x256/places", "Size=256\nMinSize=64\nMaxSize=256\nType=Scalable"),
                ("32x32@2x/places", "Size=32\nScale=2\nType=Fixed"),
            ],
        );
        icon(dir.path(), "Y", "256x256/places", "folder.png");
        let crisp = icon(dir.path(), "Y", "32x32@2x/places", "folder.png");
        assert_eq!(lookup_in("Y", &[dir.path().to_owned()], "folder", 64).unwrap(), crisp);
    }

    #[test]
    fn the_size_closest_to_the_one_asked_for_wins() {
        let (dir, roots) = desktop();
        let small = lookup_in("Filo-orange", &roots, "folder-remote", 16).unwrap();
        assert_eq!(small, dir.path().join("system/Yaru-dark/16x16/places/folder-remote.png"));
    }

    #[test]
    fn the_theme_s_own_icons_come_first_and_hicolor_is_the_last_resort() {
        let (dir, roots) = desktop();
        assert_eq!(
            lookup_in("Filo-orange", &roots, "folder-documents", 48).unwrap(),
            dir.path().join("home/Filo-orange/48x48/places/folder-documents.png")
        );
        assert_eq!(
            lookup_in("Filo-orange", &roots, "network-server", 48).unwrap(),
            dir.path().join("system/Adwaita/scalable/places/network-server.svg")
        );
        assert_eq!(
            lookup_in("Yaru-dark", &roots, "some-app", 48).unwrap(),
            dir.path().join("system/hicolor/48x48/apps/some-app.png"),
            "hicolor is searched even when no theme names it"
        );
        assert!(lookup_in("Filo-orange", &roots, "no-such-icon", 48).is_none());
        assert!(lookup_in("Missing-theme", &roots, "some-app", 48).is_some(), "an unknown theme falls back to hicolor");
    }

    #[test]
    fn a_theme_that_inherits_itself_does_not_loop() {
        let dir = tempfile::tempdir().unwrap();
        theme(dir.path(), "A", "B", &[]);
        theme(dir.path(), "B", "A", &[]);
        assert!(lookup_in("A", &[dir.path().to_owned()], "x", 48).is_none());
    }
}
