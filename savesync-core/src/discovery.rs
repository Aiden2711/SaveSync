//! Save game discovery module.
//!
//! Supports Ludusavi manifest fetching/caching, Lutris game scanning,
//! and Steam/Proton library scanning.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::models::AdapterType;

const LUDUSAVI_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml";

/// Source of a discovered save game candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoverySource {
    Ludusavi { title: String },
    Lutris { slug: String },
    Steam { appid: u32 },
    Proton { appid: u32 },
}

/// A discovered save game candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredGame {
    pub title: String,
    pub adapter_type: AdapterType,
    pub save_paths: Vec<PathBuf>,
    pub source: DiscoverySource,
}

/// Serde struct for Ludusavi manifest game entry.
#[derive(Debug, Deserialize)]
struct LudusaviGameEntry {
    files: Option<HashMap<String, serde_yaml::Value>>,
    // Part of the manifest schema, but not needed for save-path discovery.
    #[allow(dead_code)]
    steam: Option<LudusaviSteam>,
}

#[derive(Debug, Deserialize)]
struct LudusaviSteam {
    #[allow(dead_code)]
    id: Option<u32>,
}

/// Synchronously fetch or retrieve cached Ludusavi manifest YAML.
pub fn get_ludusavi_manifest_yaml(db: &mut Db) -> Result<String> {
    let conn = db.connection();
    let cached: Option<(String, String)> = conn
        .query_row(
            "SELECT etag, yaml FROM ludusavi_manifest_cache WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Network(e.to_string()))?;

    let mut req = client.get(LUDUSAVI_MANIFEST_URL);
    if let Some((ref etag, _)) = cached {
        req = req.header("If-None-Match", etag);
    }

    match req.send() {
        Ok(resp) if resp.status().as_u16() == 304 => {
            if let Some((_, yaml)) = cached {
                return Ok(yaml);
            }
        }
        Ok(resp) if resp.status().is_success() => {
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if let Ok(yaml) = resp.text() {
                let now = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "INSERT INTO ludusavi_manifest_cache (id, etag, yaml, fetched_at)
                     VALUES (1, ?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET
                        etag = excluded.etag,
                        yaml = excluded.yaml,
                        fetched_at = excluded.fetched_at",
                    params![etag, yaml, now],
                );
                return Ok(yaml);
            }
        }
        _ => {}
    }

    if let Some((_, yaml)) = cached {
        Ok(yaml)
    } else {
        Err(Error::NotFound(
            "Ludusavi manifest could not be retrieved".into(),
        ))
    }
}

/// System paths helper for 16 Ludusavi placeholders.
struct SystemPaths {
    home: PathBuf,
    user_name: String,
    win_app_data: PathBuf,
    win_local_app_data: PathBuf,
    win_local_app_data_low: PathBuf,
    win_documents: PathBuf,
    win_public: PathBuf,
    win_program_data: PathBuf,
    win_dir: PathBuf,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
}

impl SystemPaths {
    fn detect() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Users/Default"));

        let user_name = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "steamuser".to_string());

        let win_app_data = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"));

        let win_local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));

        let win_local_app_data_low = home.join("AppData").join("LocalLow");

        let win_documents = home.join("Documents");

        let win_public = std::env::var_os("PUBLIC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Users/Public"));

        let win_program_data = std::env::var_os("ALLUSERSPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/ProgramData"));

        let win_dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows"));

        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));

        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));

        Self {
            home,
            user_name,
            win_app_data,
            win_local_app_data,
            win_local_app_data_low,
            win_documents,
            win_public,
            win_program_data,
            win_dir,
            xdg_data,
            xdg_config,
        }
    }

    fn replace_placeholders(&self, path: &str, game_title: &str) -> String {
        let mut res = path.to_string();
        // Standardize separators in placeholder strings
        let to_s = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");

        res = res.replace("<home>", &to_s(&self.home));
        res = res.replace("<osUserName>", &self.user_name);
        res = res.replace("<winAppData>", &to_s(&self.win_app_data));
        res = res.replace("<winLocalAppData>", &to_s(&self.win_local_app_data));
        res = res.replace("<winLocalAppDataLow>", &to_s(&self.win_local_app_data_low));
        res = res.replace("<winDocuments>", &to_s(&self.win_documents));
        res = res.replace("<winPublic>", &to_s(&self.win_public));
        res = res.replace("<winProgramData>", &to_s(&self.win_program_data));
        res = res.replace("<winDir>", &to_s(&self.win_dir));
        res = res.replace("<xdgData>", &to_s(&self.xdg_data));
        res = res.replace("<xdgConfig>", &to_s(&self.xdg_config));
        res = res.replace("<game>", game_title);
        res = res.replace("<root>", "");
        res = res.replace("<base>", "");
        res = res.replace("<storeGameId>", "*");
        res = res.replace("<storeUserId>", "*");

        res
    }
}

/// Simple glob path expansion recursively matching directories and files.
fn expand_glob_pattern(pattern: &str) -> Vec<PathBuf> {
    let normalized = pattern.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let mut current_paths = if normalized.starts_with('/') {
        vec![PathBuf::from("/")]
    } else if parts[0].ends_with(':') {
        // Windows drive prefix e.g. "C:"
        vec![PathBuf::from(format!("{}/", parts[0]))]
    } else {
        vec![PathBuf::from(".")]
    };

    let start_idx = if parts[0].ends_with(':') { 1 } else { 0 };

    for &part in &parts[start_idx..] {
        let mut next_paths = Vec::new();

        if part == "*" || part == "**" || part.contains('*') || part.contains('?') {
            for base in &current_paths {
                if let Ok(entries) = fs::read_dir(base) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if match_pattern(part, &name) {
                            next_paths.push(entry.path());
                        }
                    }
                }
            }
        } else {
            for base in &current_paths {
                let candidate = base.join(part);
                if candidate.exists() {
                    next_paths.push(candidate);
                }
            }
        }

        current_paths = next_paths;
        if current_paths.is_empty() {
            break;
        }
    }

    // Filter to paths that actually exist
    current_paths.into_iter().filter(|p| p.exists()).collect()
}

fn match_pattern(pattern: &str, text: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern.eq_ignore_ascii_case(text);
    }

    // Simple wildcard matcher
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let (mut px, mut tx) = (0, 0);
    let (mut next_px, mut next_tx) = (0, 0);

    while px < p_bytes.len() || tx < t_bytes.len() {
        if px < p_bytes.len() {
            let c = p_bytes[px];
            if c == b'?' {
                if tx < t_bytes.len() {
                    px += 1;
                    tx += 1;
                    continue;
                }
            } else if c == b'*' {
                next_px = px + 1;
                next_tx = tx + 1;
                px += 1;
                continue;
            } else if tx < t_bytes.len() && c.eq_ignore_ascii_case(&t_bytes[tx]) {
                px += 1;
                tx += 1;
                continue;
            }
        }
        if next_tx > 0 && next_tx <= t_bytes.len() {
            px = next_px;
            tx = next_tx;
            next_tx += 1;
            continue;
        }
        return false;
    }
    true
}

/// Discover games from Ludusavi manifest.
pub fn discover_ludusavi(db: &mut Db) -> Vec<DiscoveredGame> {
    let mut results = Vec::new();
    let yaml = match get_ludusavi_manifest_yaml(db) {
        Ok(y) => y,
        Err(_) => return results,
    };

    let entries: HashMap<String, LudusaviGameEntry> = match serde_yaml::from_str(&yaml) {
        Ok(e) => e,
        Err(_) => return results,
    };

    let sys = SystemPaths::detect();

    for (title, entry) in entries {
        let mut save_paths = Vec::new();
        if let Some(files) = entry.files {
            for (pattern, _) in files {
                let resolved = sys.replace_placeholders(&pattern, &title);
                let expanded = expand_glob_pattern(&resolved);
                save_paths.extend(expanded);
            }
        }

        if !save_paths.is_empty() {
            save_paths.sort();
            save_paths.dedup();
            results.push(DiscoveredGame {
                title: title.clone(),
                adapter_type: AdapterType::Ludusavi,
                save_paths,
                source: DiscoverySource::Ludusavi { title },
            });
        }
    }

    results
}

/// Discover games from Lutris (Linux configuration files).
pub fn discover_lutris() -> Vec<DiscoveredGame> {
    let mut results = Vec::new();

    let sys = SystemPaths::detect();
    let lutris_dirs = vec![
        sys.xdg_config.join("lutris").join("games"),
        sys.xdg_data.join("lutris").join("games"),
        sys.home.join(".config").join("lutris").join("games"),
        sys.home
            .join(".local")
            .join("share")
            .join("lutris")
            .join("games"),
    ];

    for dir in lutris_dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let extension = path.extension().and_then(|e| e.to_str());
                if matches!(extension, Some("yml" | "yaml"))
                    && let Ok(content) = fs::read_to_string(&path)
                    && let Ok(val) = serde_yaml::from_str::<serde_yaml::Value>(&content)
                {
                    let name = val
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let slug = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&name)
                        .to_string();

                    if name.is_empty() {
                        continue;
                    }

                    let prefix = val
                        .get("game")
                        .and_then(|g| g.get("prefix"))
                        .and_then(|p| p.as_str());

                    let mut save_paths = Vec::new();
                    if let Some(p) = prefix {
                        let prefix_path = PathBuf::from(p);
                        let pfx_user = prefix_path.join("drive_c").join("users").join("steamuser");
                        if pfx_user.exists() {
                            save_paths.push(pfx_user);
                        } else {
                            let users_dir = prefix_path.join("drive_c").join("users");
                            if users_dir.exists() {
                                save_paths.push(users_dir);
                            }
                        }
                    }

                    if !save_paths.is_empty() {
                        results.push(DiscoveredGame {
                            title: name,
                            adapter_type: AdapterType::Emulator,
                            save_paths,
                            source: DiscoverySource::Lutris { slug },
                        });
                    }
                }
            }
        }
    }

    results
}

/// Look up a key in a VDF object.
///
/// VDF keys are case-insensitive, but the parser preserves the source casing,
/// so an exact lookup is tried first and case-insensitive ASCII matching is
/// used as a fallback.
fn vdf_get_ci<'a, 'text>(
    obj: &'a steam_vdf_parser::Obj<'text>,
    key: &str,
) -> Option<&'a steam_vdf_parser::Value<'text>> {
    if let Some(value) = obj.get(key) {
        return Some(value);
    }
    obj.iter()
        .find(|(k, _)| k.as_ref().eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}

/// Parse `libraryfolders.vdf` text into library paths, in document order.
///
/// Supports the v2 format (`"libraryfolders" { "0" { "path" "..." } }`) and the
/// legacy format where a numeric entry maps directly to a path string. Paths are
/// deduplicated; no filesystem access or existence checks are performed here.
fn parse_steam_library_folders(text: &str) -> Vec<PathBuf> {
    let Ok(vdf) = steam_vdf_parser::parse_text(text) else {
        return Vec::new();
    };

    if !vdf.key().eq_ignore_ascii_case("libraryfolders") {
        return Vec::new();
    }

    let Some(root) = vdf.as_obj() else {
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = Vec::new();

    for (key, value) in root.iter() {
        let candidate = if let Some(entry) = value.as_obj() {
            vdf_get_ci(entry, "path").and_then(|v| v.as_str())
        } else if key.as_ref().parse::<u32>().is_ok() {
            // Legacy format: numeric keys map directly to the library path.
            value.as_str()
        } else {
            None
        };

        if let Some(candidate) = candidate {
            let path = PathBuf::from(candidate);
            if !path.as_os_str().is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    paths
}

/// Parsed Steam `appmanifest_*.acf` application state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SteamAppManifest {
    appid: u32,
    name: String,
    installdir: String,
}

/// Parse an `appmanifest_*.acf` text document into its `AppState` fields.
///
/// Returns `None` when the document is malformed, the root key is not
/// `AppState`, `appid` is missing/invalid, or `name` is missing/empty.
/// `installdir` is optional and defaults to an empty string.
fn parse_app_manifest(text: &str) -> Option<SteamAppManifest> {
    let vdf = steam_vdf_parser::parse_text(text).ok()?;

    if !vdf.key().eq_ignore_ascii_case("AppState") {
        return None;
    }

    let state = vdf.as_obj()?;
    let appid = vdf_get_ci(state, "appid")?.as_str()?.parse::<u32>().ok()?;
    let name = vdf_get_ci(state, "name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let installdir = vdf_get_ci(state, "installdir")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    Some(SteamAppManifest {
        appid,
        name: name.to_string(),
        installdir: installdir.to_string(),
    })
}

/// Locate Steam library roots, including extra libraries from `libraryfolders.vdf`.
fn find_steam_libraries() -> Vec<PathBuf> {
    let sys = SystemPaths::detect();
    let candidates = vec![
        // Standard Steam install paths
        PathBuf::from("C:/Program Files (x86)/Steam"),
        PathBuf::from("C:/Program Files/Steam"),
        sys.home.join(".local/share/Steam"),
        sys.home.join(".steam/steam"),
        sys.home.join(".steam/root"),
        sys.home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
        sys.home.join("Library/Application Support/Steam"),
    ];

    let mut libraries = Vec::new();

    for steam_dir in candidates {
        if !steam_dir.exists() {
            continue;
        }
        libraries.push(steam_dir.clone());

        let vdf_path = steam_dir.join("steamapps").join("libraryfolders.vdf");
        if vdf_path.exists()
            && let Ok(content) = fs::read_to_string(&vdf_path)
        {
            for path in parse_steam_library_folders(&content) {
                if path.exists() && !libraries.contains(&path) {
                    libraries.push(path);
                }
            }
        }
    }

    libraries
}

/// Discover games from Steam and Proton prefixes.
pub fn discover_steam() -> Vec<DiscoveredGame> {
    let mut results = Vec::new();
    let libraries = find_steam_libraries();

    for lib in &libraries {
        let steamapps = lib.join("steamapps");
        if !steamapps.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(&steamapps) {
            for entry in entries.flatten() {
                let path = entry.path();
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if file_name.starts_with("appmanifest_")
                    && file_name.ends_with(".acf")
                    && let Ok(content) = fs::read_to_string(&path)
                    && let Some(manifest) = parse_app_manifest(&content)
                {
                    let appid = manifest.appid;
                    let appid_str = appid.to_string();
                    let name = manifest.name;
                    let install_dir_str = manifest.installdir;

                    let mut save_paths = Vec::new();

                    // 1. Proton compatdata prefix
                    let proton_pfx = steamapps
                        .join("compatdata")
                        .join(&appid_str)
                        .join("pfx")
                        .join("drive_c")
                        .join("users")
                        .join("steamuser");
                    if proton_pfx.exists() {
                        save_paths.push(proton_pfx.clone());
                        results.push(DiscoveredGame {
                            title: format!("{} (Proton)", name),
                            adapter_type: AdapterType::Emulator,
                            save_paths: vec![proton_pfx],
                            source: DiscoverySource::Proton { appid },
                        });
                    }

                    // 2. Steam Cloud userdata
                    let userdata_dir = lib.join("userdata");
                    if userdata_dir.exists()
                        && let Ok(user_entries) = fs::read_dir(&userdata_dir)
                    {
                        for u_entry in user_entries.flatten() {
                            let remote_path = u_entry.path().join(&appid_str).join("remote");
                            if remote_path.exists() {
                                save_paths.push(remote_path);
                            }
                        }
                    }

                    // 3. Game install dir saves if existing
                    if !install_dir_str.is_empty() {
                        let common_install = steamapps.join("common").join(install_dir_str);
                        if common_install.exists() {
                            let saves_dir = common_install.join("saves");
                            if saves_dir.exists() {
                                save_paths.push(saves_dir);
                            }
                        }
                    }

                    if !save_paths.is_empty() {
                        results.push(DiscoveredGame {
                            title: name,
                            adapter_type: AdapterType::Ludusavi,
                            save_paths,
                            source: DiscoverySource::Steam { appid },
                        });
                    }
                }
            }
        }
    }

    results
}

/// Main entry point for discovering games across all sources with deduplication.
pub fn discover_all_games(db: &mut Db) -> Vec<DiscoveredGame> {
    let mut all = Vec::new();

    all.extend(discover_ludusavi(db));
    all.extend(discover_lutris());
    all.extend(discover_steam());

    // Deduplicate by normalized title (case-insensitive)
    let mut map: HashMap<String, DiscoveredGame> = HashMap::new();

    for game in all {
        let norm_key = game.title.trim().to_lowercase();
        if let Some(existing) = map.get_mut(&norm_key) {
            for path in game.save_paths {
                if !existing.save_paths.contains(&path) {
                    existing.save_paths.push(path);
                }
            }
        } else {
            map.insert(norm_key, game);
        }
    }

    let mut result: Vec<DiscoveredGame> = map.into_values().collect();
    result.sort_by(|a, b| a.title.cmp(&b.title));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_paths_placeholders() {
        let sys = SystemPaths::detect();
        let replaced = sys.replace_placeholders("<winDocuments>/MyGame/saves", "MyGame");
        assert!(!replaced.contains("<winDocuments>"));
        assert!(replaced.contains("MyGame/saves"));
    }

    #[test]
    fn test_pattern_matching() {
        assert!(match_pattern("*.sav", "slot1.sav"));
        assert!(match_pattern("appmanifest_*.acf", "appmanifest_123.acf"));
        assert!(!match_pattern("*.sav", "slot1.txt"));
    }

    #[test]
    fn test_parse_steam_library_folders_v2() {
        let text = r#"
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "contentid"   "1234567890"
        "apps"
        {
            "228980"  "9876543210"
        }
    }
    "1"
    {
        "path"        "/home/steam/.steam/steam"
        "label"       ""
        "apps"
        {
        }
    }
}
"#;

        let paths = parse_steam_library_folders(text);
        assert_eq!(
            paths,
            vec![
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from("/home/steam/.steam/steam"),
            ]
        );
    }

    #[test]
    fn test_parse_steam_library_folders_legacy() {
        // Legacy documents also carry non-path entries such as stats IDs; only
        // the numeric entries are library paths.
        let text = r#"
"LibraryFolders"
{
    "TimeNextStatsReport"  "1234567890"
    "ContentStatsID"       "9876543210123456789"
    "1"                    "D:\\SteamLibrary"
    "2"                    "/mnt/games/SteamLibrary"
}
"#;

        let paths = parse_steam_library_folders(text);
        assert_eq!(
            paths,
            vec![
                PathBuf::from(r"D:\SteamLibrary"),
                PathBuf::from("/mnt/games/SteamLibrary"),
            ]
        );
    }

    #[test]
    fn test_parse_steam_library_folders_deduplicates() {
        let text = r#"
"libraryfolders"
{
    "0"
    {
        "path" "D:\\SteamLibrary"
    }
    "1"
    {
        "path" "D:\\SteamLibrary"
    }
}
"#;

        assert_eq!(
            parse_steam_library_folders(text),
            vec![PathBuf::from(r"D:\SteamLibrary")]
        );
    }

    #[test]
    fn test_parse_steam_library_folders_rejects_invalid_input() {
        // Malformed document (missing value / closing brace).
        assert!(parse_steam_library_folders(r#""libraryfolders" { "0" { "path" }"#).is_empty());
        // Root value is not an object.
        assert!(parse_steam_library_folders(r#""libraryfolders" "not an object""#).is_empty());
        // Wrong root key.
        assert!(
            parse_steam_library_folders(r#""AppState" { "0" { "path" "C:\\Steam" } }"#).is_empty()
        );
        // Not VDF at all.
        assert!(parse_steam_library_folders("this is not vdf").is_empty());
    }

    #[test]
    fn test_parse_app_manifest() {
        let text = r#"
"AppState"
{
    "appid"        "12345"
    "Universe"     "1"
    "name"         "Test Game"
    "StateFlags"   "4"
    "installdir"   "TestGame"
    "UserConfig"
    {
        "language" "english"
    }
}
"#;

        let manifest = parse_app_manifest(text).expect("manifest should parse");
        assert_eq!(
            manifest,
            SteamAppManifest {
                appid: 12345,
                name: "Test Game".to_string(),
                installdir: "TestGame".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_app_manifest_case_insensitive_keys_and_missing_installdir() {
        let text = r#"
"APPSTATE"
{
    "AppId"  "999"
    "Name"   "Case Game"
}
"#;

        let manifest = parse_app_manifest(text).expect("manifest should parse");
        assert_eq!(manifest.appid, 999);
        assert_eq!(manifest.name, "Case Game");
        assert_eq!(manifest.installdir, "");
    }

    #[test]
    fn test_parse_app_manifest_rejects_invalid_documents() {
        // Missing AppState root.
        assert_eq!(
            parse_app_manifest(r#""appmanifest" { "appid" "12345" "name" "Test Game" }"#),
            None
        );
        // Non-numeric appid.
        assert_eq!(
            parse_app_manifest(r#""AppState" { "appid" "abc" "name" "Test Game" }"#),
            None
        );
        // appid out of range for u32.
        assert_eq!(
            parse_app_manifest(r#""AppState" { "appid" "4294967296" "name" "Test Game" }"#),
            None
        );
        // Empty name.
        assert_eq!(
            parse_app_manifest(r#""AppState" { "appid" "12345" "name" "" }"#),
            None
        );
        // Malformed document.
        assert_eq!(parse_app_manifest(r#""AppState" { "appid" "12345" "#), None);
    }
}
