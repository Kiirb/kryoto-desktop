//! Heroic Games Launcher's own added games (heroicgameslauncher.com), read-only.
//!
//! Heroic is an Electron app; its config folder is `heroic` under the
//! platform's config folder (`~/.config`, `%APPDATA%`, Application Support;
//! the Flatpak keeps it in `~/.var/app/com.heroicgameslauncher.hgl/config`).
//! Games the player added themselves ("sideloaded") are
//! `sideload_apps/library.json`: `{"games": [{app_name, title, install:
//! {executable, platform}, folder_name, art_cover, art_square,
//! is_installed}]}`. Each game's settings are `GamesConfig/<app_name>.json`
//! (`{"<app_name>": {winePrefix, wineVersion: {bin, name, type},
//! launcherArgs, enviromentOptions}}`), falling back to `config.json`'s
//! `defaultSettings`; the time played is `store/timestamp.json`
//! (`{"<app_name>": {firstPlayed, lastPlayed, totalPlayed}}`, ISO dates and
//! minutes).
//!
//! Epic, GOG and Amazon games are left out: those need their store's own
//! client to start, which only Heroic has. So are added games that are not
//! Windows games: Kryoto starts Windows games.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One game the player added to Heroic, as Kryoto uses it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeroicGame {
    /// Heroic's `app_name`: what a remembered link is kept by.
    pub key: String,
    pub title: String,
    /// Absolute path of the exe Heroic starts.
    pub executable: Option<String>,
    /// The folder it is installed in.
    pub folder: Option<String>,
    pub playtime_seconds: u64,
    /// Unix seconds.
    pub last_played: Option<u64>,
    pub added_at: Option<u64>,
    /// Heroic's arguments and environment, as one Kryoto launch line.
    pub launch_options: Option<String>,
    /// Pictures: web addresses, or files on this PC.
    pub cover: Option<String>,
    pub custom_cover: Option<String>,
    /// What Heroic runs it with, on Linux and macOS.
    pub wine: Option<Wine>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wine {
    /// Heroic's `wineVersion.bin`: Proton's `proton` script or a `wine`.
    /// None for a kind Kryoto cannot run (CrossOver, Game Porting Toolkit).
    pub bin: Option<String>,
    /// `proton` or `wine`.
    pub kind: String,
    /// The prefix as Heroic writes it: for Proton, the folder holding `pfx`.
    pub prefix: PathBuf,
    /// Other games use this prefix too (or it is Heroic's default, which any
    /// game without settings of its own uses).
    pub shared: bool,
    /// It runs with Heroic's default Wine or Proton (none of its own, or the
    /// same one): brought over, it takes Kryoto's default, not a forced one.
    pub default_tool: bool,
}

impl Wine {
    /// The Wine prefix proper: `pfx` inside it for Proton's layout.
    pub fn wine_prefix(&self) -> PathBuf {
        let pfx = self.prefix.join("pfx");
        if pfx.join("drive_c").is_dir() { pfx } else { self.prefix.clone() }
    }
}

/// Where Heroic keeps its config on this PC, most likely first. Passed in so
/// this stays testable; the caller reads them from the environment.
pub fn candidate_dirs(appdata: Option<&Path>, home: Option<&Path>, xdg_config: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        out.extend(appdata.map(|a| a.join("heroic")));
    } else if cfg!(target_os = "macos") {
        out.extend(home.map(|h| h.join("Library/Application Support/heroic")));
    } else {
        out.extend(xdg_config.map(|x| x.join("heroic")));
        if let Some(h) = home {
            out.push(h.join(".config/heroic"));
            out.push(h.join(".var/app/com.heroicgameslauncher.hgl/config/heroic"));
        }
    }
    out.dedup();
    out
}

/// A folder is Heroic's when it has its list of added games.
pub fn is_heroic_dir(dir: &Path) -> bool {
    dir.join("sideload_apps").join("library.json").is_file()
}

fn json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// `~/x` as Heroic sometimes writes it, made whole.
fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(path),
    }
}

fn same(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    n(a) == n(b)
}

/// `2026-10-20T13:05:20.515Z` (UTC, as JavaScript writes dates) as Unix seconds.
pub fn iso_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    if s.len() < 19 || &s[4..5] != "-" || &s[7..8] != "-" || !matches!(&s[10..11], "T" | " ") {
        return None;
    }
    let (y, m, d, hh, mm, ss) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Heroic's arguments and environment as one launch line:
/// `KEY=value %command% args`. Variables whose name or value would need
/// quoting are left out rather than guessed at.
fn launch_line(settings: &Value) -> Option<String> {
    let args = text(&settings["launcherArgs"]).unwrap_or_default();
    let env: Vec<String> = settings["enviromentOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let key = text(&e["key"])?;
            let value = e["value"].as_str().unwrap_or("");
            let ok_key = key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !key.starts_with(|c: char| c.is_ascii_digit());
            let ok_value = !value.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '$'));
            (ok_key && ok_value).then(|| format!("{key}={value}"))
        })
        .collect();
    match (env.is_empty(), args.is_empty()) {
        (true, true) => None,
        (true, false) => Some(args),
        (false, _) => Some(format!("{} %command% {args}", env.join(" ")).trim_end().to_string()),
    }
}

/// Every game's settings: `app_name` → its own settings, from `GamesConfig`.
fn games_config(dir: &Path) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Ok(read) = std::fs::read_dir(dir.join("GamesConfig")) else { return out };
    for e in read.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        if let Some(v) = json(&path).and_then(|v| v.get(&name).cloned()).filter(Value::is_object) {
            out.insert(name, v);
        }
    }
    out
}

/// The games the player added to Heroic. Only reads.
pub fn read_library(dir: &Path, home: Option<&Path>) -> Result<Vec<HeroicGame>, String> {
    let lib = json(&dir.join("sideload_apps").join("library.json")).ok_or("No Heroic library in that folder.")?;
    let defaults = json(&dir.join("config.json")).map(|c| c["defaultSettings"].clone()).unwrap_or(Value::Null);
    let configs = games_config(dir);
    let times = json(&dir.join("store").join("timestamp.json")).unwrap_or(Value::Null);
    let default_prefix = text(&defaults["winePrefix"]).map(|p| expand(&p, home));
    let prefix_of = |name: &str| text(&configs.get(name).unwrap_or(&Value::Null)["winePrefix"]).map(|p| expand(&p, home)).or_else(|| default_prefix.clone());
    // How many games each prefix holds, the ones only listed in GamesConfig too.
    let mut users: Vec<PathBuf> = configs.keys().filter_map(|k| prefix_of(k)).collect();
    let games = lib["games"].as_array().cloned().unwrap_or_default();
    users.extend(games.iter().filter_map(|g| text(&g["app_name"])).filter(|k| !configs.contains_key(k)).filter_map(|k| prefix_of(&k)));

    let mut out = Vec::new();
    for g in &games {
        let Some(key) = text(&g["app_name"]) else { continue };
        let platform = text(&g["install"]["platform"]).unwrap_or_default().to_ascii_lowercase();
        if platform != "windows" {
            continue;
        }
        let settings = configs.get(&key).cloned().unwrap_or(Value::Null);
        let pick = |field: &str| if settings[field].is_null() { defaults[field].clone() } else { settings[field].clone() };
        let wine = (!cfg!(windows)).then(|| prefix_of(&key)).flatten().map(|prefix| {
            let version = pick("wineVersion");
            let default_tool = settings["wineVersion"].is_null() || same_tool(&settings["wineVersion"], &defaults["wineVersion"]);
            let kind = text(&version["type"]).unwrap_or_default().to_ascii_lowercase();
            let bin = text(&version["bin"]).filter(|_| kind == "proton" || kind == "wine").map(|b| expand(&b, home).to_string_lossy().into_owned());
            let shared = users.iter().filter(|p| same(p, &prefix)).count() > 1 || default_prefix.as_deref().is_some_and(|d| same(d, &prefix));
            Wine { bin, kind, prefix, shared, default_tool }
        });
        let time = &times[&key];
        let art = |v: &Value| text(v).map(|a| a.strip_prefix("file://").map(String::from).unwrap_or(a));
        let art = art(&g["art_square"]).or_else(|| art(&g["art_cover"]));
        let (cover, custom_cover) = match art {
            Some(a) if a.starts_with("https://") => (Some(a), None),
            Some(a) if Path::new(&a).is_absolute() => (None, Some(a)),
            _ => (None, None),
        };
        out.push(HeroicGame {
            title: text(&g["title"]).unwrap_or_else(|| key.clone()),
            executable: text(&g["install"]["executable"]),
            folder: text(&g["folder_name"]),
            playtime_seconds: time["totalPlayed"].as_u64().unwrap_or(0) * 60,
            last_played: time["lastPlayed"].as_str().and_then(iso_seconds),
            added_at: time["firstPlayed"].as_str().and_then(iso_seconds),
            launch_options: launch_line(&pick_obj(&settings, &defaults)),
            cover,
            custom_cover,
            wine,
            key,
        });
    }
    Ok(out)
}

/// Two of Heroic's `wineVersion`s are the same tool: the same `bin`, or
/// with none written, the same name.
fn same_tool(a: &Value, b: &Value) -> bool {
    match (text(&a["bin"]), text(&b["bin"])) {
        (Some(x), Some(y)) => x == y,
        _ => text(&a["name"]).is_some_and(|n| Some(n) == text(&b["name"])),
    }
}

/// The game's settings over Heroic's defaults, for the launch line.
fn pick_obj(settings: &Value, defaults: &Value) -> Value {
    let mut v = defaults.clone();
    if let (Some(into), Some(from)) = (v.as_object_mut(), settings.as_object()) {
        for (k, x) in from {
            into.insert(k.clone(), x.clone());
        }
        return v;
    }
    settings.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_seconds() {
        assert_eq!(iso_seconds("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_seconds("2026-10-20T13:05:20.515Z"), Some(1_792_501_520));
        assert_eq!(iso_seconds("2000-02-29T12:00:00Z"), Some(951_825_600));
        for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026/10/20 13:05:20"] {
            assert_eq!(iso_seconds(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_launch_line_keeps_only_plain_variables() {
        let v: Value = serde_json::json!({"launcherArgs": "-dx11", "enviromentOptions": [{"key": "DXVK_HUD", "value": "fps"}, {"key": "BAD KEY", "value": "x"}, {"key": "Q", "value": "a b"}]});
        assert_eq!(launch_line(&v).as_deref(), Some("DXVK_HUD=fps %command% -dx11"));
        assert_eq!(launch_line(&serde_json::json!({"launcherArgs": " -w "})).as_deref(), Some("-w"));
        assert_eq!(launch_line(&Value::Null), None);
    }

    #[test]
    fn reads_added_games_with_their_prefix_and_hours() {
        let dir = std::env::temp_dir().join(format!("kryoto-heroic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["sideload_apps", "GamesConfig", "store"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let w = |p: &str, v: Value| std::fs::write(dir.join(p), serde_json::to_vec(&v).unwrap()).unwrap();
        let game = |name: &str, title: &str, platform: &str| serde_json::json!({"runner": "sideload", "app_name": name, "title": title, "install": {"executable": format!("/games/{title}/{title}.exe"), "platform": platform}, "folder_name": format!("/games/{title}"), "art_square": "https://cdn.example/a.png", "is_installed": true});
        w("sideload_apps/library.json", serde_json::json!({"games": [game("aaa", "Alone", "Windows"), game("bbb", "Shared1", "Windows"), game("ccc", "Shared2", "Windows"), game("ddd", "Tux", "linux"), game("eee", "Defaulted", "Windows")]}));
        w("config.json", serde_json::json!({"defaultSettings": {"winePrefix": "~/Games/Heroic/Prefixes/default", "wineVersion": {"bin": "/usr/bin/wine", "type": "wine"}}}));
        let cfg = |name: &str, prefix: &str| serde_json::json!({ name: {"winePrefix": prefix, "wineVersion": {"bin": "/p/proton", "type": "proton"}, "launcherArgs": "-x"}, "version": "v0"});
        w("GamesConfig/aaa.json", cfg("aaa", "/pfx/Alone"));
        w("GamesConfig/bbb.json", cfg("bbb", "/pfx/sideload"));
        w("GamesConfig/ccc.json", cfg("ccc", "/pfx/sideload/"));
        // The same Wine as Heroic's default, written out: still the default.
        w("GamesConfig/bbb.json", serde_json::json!({"bbb": {"winePrefix": "/pfx/sideload", "wineVersion": {"bin": "/usr/bin/wine", "type": "wine"}}}));
        w("store/timestamp.json", serde_json::json!({"aaa": {"firstPlayed": "2026-08-16T10:48:58.986Z", "lastPlayed": "2026-10-20T13:05:20.515Z", "totalPlayed": 58}}));

        let games = read_library(&dir, Some(Path::new("/home/p"))).unwrap();
        let keys: Vec<_> = games.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, ["aaa", "bbb", "ccc", "eee"], "the Linux game is left out");
        let a = &games[0];
        assert_eq!((a.playtime_seconds, a.last_played, a.launch_options.as_deref()), (3480, Some(1_792_501_520), Some("-x")));
        assert_eq!((a.executable.as_deref(), a.cover.as_deref()), (Some("/games/Alone/Alone.exe"), Some("https://cdn.example/a.png")));
        if !cfg!(windows) {
            let wine = |i: usize| games[i].wine.clone().unwrap();
            assert_eq!((wine(0).kind.as_str(), wine(0).bin.as_deref(), wine(0).shared), ("proton", Some("/p/proton"), false));
            assert!(wine(1).shared && wine(2).shared, "one prefix, two games");
            // Heroic's default tool, by inheriting it or naming the same one: no forced tool.
            assert_eq!((wine(0).default_tool, wine(1).default_tool, wine(2).default_tool, wine(3).default_tool), (false, true, false, true));
            assert_eq!((wine(3).prefix, wine(3).shared, wine(3).kind.as_str()), (PathBuf::from("/home/p/Games/Heroic/Prefixes/default"), true, "wine"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
