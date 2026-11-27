//! Heroic Games Launcher on Bridge: the games the player added to it
//! themselves. Its files are read by `kryoto-import`, which never writes one
//! of Heroic's. Heroic hands its games over: a game Kryoto has becomes a
//! Kryoto game, one it does not stays the player's own, and either way it
//! starts with the same Wine or Proton Heroic used.
//!
//! On Linux each game's Wine prefix comes along too, so it starts where it
//! left off: copied whole when it is the game's own, and when other games
//! share it (Heroic's default, or one the player pointed several games at),
//! only the game's save files, found the way save versions find them. A
//! prefix Kryoto already has for the game is never written over. Heroic's
//! prefix is only read.

use super::{env_path, BroughtOver, Following, Incoming, Provider, ReadProblem};
use kryoto_import::heroic::{self as he, HeroicGame};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

pub(super) struct Heroic;

fn home() -> Option<PathBuf> {
    env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

/// Heroic's config folder on this PC, when there is one.
fn default_dir() -> Option<PathBuf> {
    // Debug builds only: a test library in Heroic's format instead of the PC's own.
    #[cfg(debug_assertions)]
    if let Some(dir) = env_path("KRYOTO_DEV_HEROIC_DIR").filter(|d| he::is_heroic_dir(d)) {
        return Some(dir);
    }
    he::candidate_dirs(env_path("APPDATA").as_deref(), home().as_deref(), env_path("XDG_CONFIG_HOME").as_deref()).into_iter().find(|d| he::is_heroic_dir(d))
}

impl From<HeroicGame> for Incoming {
    fn from(h: HeroicGame) -> Incoming {
        Incoming {
            key: h.key,
            title: h.title,
            executable: h.executable,
            folder: h.folder,
            playtime_seconds: h.playtime_seconds,
            last_played: h.last_played,
            added_at: h.added_at,
            launch_options: h.launch_options,
            // Heroic's default tool becomes Kryoto's default: no tool forced.
            proton_path: h.wine.filter(|w| !w.default_tool).and_then(|w| w.bin),
            cover: h.cover,
            custom_cover: h.custom_cover,
            ..Default::default()
        }
    }
}

impl Provider for Heroic {
    fn id(&self) -> &'static str {
        "heroic"
    }

    fn name(&self) -> &'static str {
        "Heroic"
    }

    /// The folder the player picked (Heroic's config folder), else Heroic's own.
    fn locate(&self, picked: Option<&str>) -> Option<PathBuf> {
        let Some(dir) = picked.map(str::trim).filter(|d| !d.is_empty()) else { return default_dir() };
        let p = PathBuf::from(dir);
        if he::is_heroic_dir(&p) {
            return Some(p);
        }
        let inner = p.join("heroic");
        he::is_heroic_dir(&inner).then_some(inner)
    }

    /// Whether Heroic is open. Its files could be read anyway, but a game it
    /// started may be running in the prefix that is about to be copied.
    fn running(&self) -> bool {
        super::process_running(&["heroic", "heroic.exe", "heroic games launcher.exe"], |_, exe| {
            exe.contains("/heroic/heroic") || exe.contains("heroicgameslauncher") || exe.contains("heroic games launcher")
        })
    }

    fn read(&self, dir: &Path) -> Result<Vec<Incoming>, ReadProblem> {
        he::read_library(dir, home().as_deref()).map(|games| games.into_iter().map(Incoming::from).collect()).map_err(ReadProblem::Other)
    }

    /// Their prefixes (or saves), then their saves as versions.
    fn brought_over(&self, app: &AppHandle, dir: &Path, games: Vec<BroughtOver>) -> Following {
        let saves = !games.is_empty() && crate::saves::enabled(app);
        let wine: Vec<(BroughtOver, he::Wine)> = if cfg!(windows) {
            Vec::new()
        } else {
            let heroic = he::read_library(dir, home().as_deref()).unwrap_or_default();
            games.iter().filter_map(|g| Some((BroughtOver { id: g.id.clone(), title: g.title.clone(), key: g.key.clone() }, heroic.iter().find(|h| h.key == g.key)?.wine.clone()?))).collect()
        };
        let moving = !wine.is_empty();
        if !moving && !saves {
            return Following::default();
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut done: Vec<String> = Vec::new();
            for (g, w) in &wine {
                match bring_prefix(&app, g, w).await {
                    Ok(Some(what)) => done.push(format!("{} ({what})", g.title)),
                    Ok(None) => {}
                    Err(e) => crate::logging::warn("heroic", &format!("{}: prefix not brought over: {e}", g.title)),
                }
            }
            if saves {
                for g in &games {
                    if let Err(e) = crate::saves::bring_over(&app, &g.id, None).await {
                        crate::logging::warn("heroic", &format!("{}: saves not kept: {e}", g.title));
                    }
                }
            }
            if !done.is_empty() {
                use tauri::Emitter;
                let body = format!("From Heroic: {}.", done.join(", "));
                let _ = app.emit("notify", crate::downloads::Notice::new("Wine prefixes brought over", &body, None));
            }
        });
        Following {
            saves,
            note: moving.then(|| "Their Wine prefixes are being copied from Heroic, or just their saves where other games share the prefix; a notice says when.".into()),
        }
    }
}

/// One game's prefix from Heroic into Kryoto's place for it. Says what came,
/// or nothing when there was nothing to bring.
async fn bring_prefix(app: &AppHandle, g: &BroughtOver, w: &he::Wine) -> Result<Option<&'static str>, String> {
    let from = w.wine_prefix();
    if !from.join("drive_c").is_dir() {
        return Ok(None);
    }
    let to = crate::library::prefix_root(app, &g.id)?;
    if to.exists() {
        crate::logging::info("heroic", &format!("{}: Kryoto has a prefix for it already, left as it is", g.title));
        return Ok(None);
    }
    // Where Kryoto will start it: `pfx` inside, like Heroic, for Proton's layout.
    let proton_layout = from != w.prefix;
    if !w.shared {
        let (src, dst) = (w.prefix.clone(), to.clone());
        let whole = tauri::async_runtime::spawn_blocking(move || copy_whole(&src, &dst)).await.map_err(|e| e.to_string())?;
        match whole {
            Ok(bytes) => {
                crate::logging::info("heroic", &format!("{}: prefix copied whole, {bytes} bytes, from {}", g.title, w.prefix.display()));
                return Ok(Some("whole prefix"));
            }
            // Not enough room for all of it: the saves still fit.
            Err(e) => crate::logging::warn("heroic", &format!("{}: {e}; bringing just its saves", g.title)),
        }
    }
    let game = crate::library::load(app)?.into_iter().find(|x| x.id == g.id).ok_or("It is no longer in the library.")?;
    let roots = kryoto_saves::rules::SaveRoots::for_prefix(&from, Some(PathBuf::from(&game.install_dir)));
    let (rules, _) = crate::saves::locations(app, &game, &roots);
    // In the game's own folder they are where they were; the registry is
    // the prefix's own, and other games' keys are in it too.
    let rules: Vec<_> = rules.into_iter().filter(|r| r.kind == kryoto_saves::rules::Kind::Files && r.root != kryoto_saves::rules::Root::GameDir).collect();
    let into = if proton_layout { to.join("pfx") } else { to.clone() };
    let copied = tauri::async_runtime::spawn_blocking(move || {
        let found = kryoto_saves::scan::collect(&rules, &roots, kryoto_saves::scan::MAX_FILES);
        copy_saves(&from, &into, found.files.iter().map(|f| f.abs.as_path()))
    })
    .await
    .map_err(|e| e.to_string())??;
    crate::logging::info("heroic", &format!("{}: {copied} save files copied from the shared prefix {}", g.title, w.prefix.display()));
    Ok((copied > 0).then_some("its saves"))
}

/// Room kept free on the drive besides the prefix itself.
#[cfg(unix)]
const MARGIN: u64 = 1 << 30;

/// The whole prefix, into a folder beside `to` and then renamed, so a copy
/// that stops halfway never looks like a prefix. Links stay links (a prefix
/// links `z:` to `/`), and one into the old prefix points into the new one.
#[cfg(unix)]
fn copy_whole(from: &Path, to: &Path) -> Result<u64, String> {
    let parent = to.parent().ok_or("No place for prefixes.")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let size = tree_size(from);
    if let Some((_, free)) = crate::storage::disk_space(parent) {
        if free < size.saturating_add(MARGIN) {
            return Err(format!("the prefix takes {size} bytes and only {free} are free"));
        }
    }
    let name = to.file_name().ok_or("No name for the prefix.")?.to_string_lossy();
    let part = parent.join(format!(".part-{name}"));
    let _ = std::fs::remove_dir_all(&part);
    match copy_tree(from, &part, from, to) {
        Ok(()) => std::fs::rename(&part, to).map(|_| size).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&part);
            Err(format!("the prefix could not be copied: {e}"))
        }
    }
}

#[cfg(not(unix))]
fn copy_whole(_: &Path, _: &Path) -> Result<u64, String> {
    Err("there are no prefixes on this system".into())
}

/// Bytes of the files under `dir`, not following links.
#[cfg(unix)]
fn tree_size(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else { return 0 };
    read.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// `from` into `to`, links as links. A link to somewhere inside `old_root`
/// is pointed at the same place under `new_root`.
#[cfg(unix)]
fn copy_tree(from: &Path, to: &Path, old_root: &Path, new_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let kind = e.file_type()?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&src)?;
            let target = match target.strip_prefix(old_root) {
                Ok(rest) if target.is_absolute() => new_root.join(rest),
                _ => target,
            };
            std::os::unix::fs::symlink(target, &dst)?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst, old_root, new_root)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst)?;
        }
        // Sockets and pipes are Wine's while it runs: nothing to keep.
    }
    Ok(())
}

/// Each save file at the same place inside the new prefix. Returns how many.
fn copy_saves<'a>(from: &Path, into: &Path, files: impl Iterator<Item = &'a Path>) -> Result<usize, String> {
    let mut n = 0;
    for abs in files {
        let Ok(rel) = abs.strip_prefix(from) else { continue };
        let dst = into.join(rel);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(abs, &dst).map_err(|e| format!("{}: {e}", rel.display()))?;
        n += 1;
    }
    Ok(n)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kryoto-heroic-bridge-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_whole_prefix_keeps_its_links_without_following_them() {
        let root = temp("whole");
        let (from, to) = (root.join("Heroic/Prefixes/Alone"), root.join("kryoto/prefixes/alone"));
        let user = from.join("pfx/drive_c/users/steamuser/AppData/Roaming/Game");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(from.join("pfx/dosdevices")).unwrap();
        std::fs::write(user.join("save.dat"), b"progress").unwrap();
        std::fs::write(from.join("version"), b"9.0").unwrap();
        std::os::unix::fs::symlink("/", from.join("pfx/dosdevices/z:")).unwrap();
        std::os::unix::fs::symlink("../drive_c", from.join("pfx/dosdevices/c:")).unwrap();
        std::os::unix::fs::symlink(from.join("pfx/drive_c"), from.join("pfx/inside")).unwrap();
        copy_whole(&from, &to).unwrap();
        assert_eq!(std::fs::read(to.join("pfx/drive_c/users/steamuser/AppData/Roaming/Game/save.dat")).unwrap(), b"progress");
        assert_eq!(std::fs::read_link(to.join("pfx/dosdevices/z:")).unwrap(), Path::new("/"));
        assert_eq!(std::fs::read_link(to.join("pfx/dosdevices/c:")).unwrap(), Path::new("../drive_c"));
        assert_eq!(std::fs::read_link(to.join("pfx/inside")).unwrap(), to.join("pfx/drive_c"));
        assert!(!root.join("kryoto/prefixes/.part-alone").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saves_land_at_the_same_place_and_never_over_a_file() {
        let root = temp("saves");
        let (from, into) = (root.join("shared/pfx"), root.join("kryoto/pfx"));
        let a = from.join("drive_c/users/steamuser/Documents/My Games/X/a.sav");
        let b = from.join("drive_c/users/steamuser/Documents/My Games/X/b.sav");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let kept = into.join("drive_c/users/steamuser/Documents/My Games/X/b.sav");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"mine").unwrap();
        let outside = root.join("elsewhere.sav");
        std::fs::write(&outside, b"x").unwrap();
        let n = copy_saves(&from, &into, [a.as_path(), b.as_path(), outside.as_path()].into_iter()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(into.join("drive_c/users/steamuser/Documents/My Games/X/a.sav")).unwrap(), b"a");
        assert_eq!(std::fs::read(&kept).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(root);
    }
}
