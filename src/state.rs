//! State shared between the SDK callbacks (main thread, network callbacks) and the audio threads.
//! Handles like `Client` and `World` are never stored here, only plain data.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

pub static MY_ID: AtomicU64 = AtomicU64::new(0);
pub static LISTENER: Mutex<[f32; 3]> = Mutex::new([0.0; 3]);
pub static YAW: AtomicU32 = AtomicU32::new(0); // f32 bits
pub static PTT: AtomicBool = AtomicBool::new(false);
pub static SELF_SPEAKING: AtomicBool = AtomicBool::new(false);
pub static MIC_LEVEL: AtomicU32 = AtomicU32::new(0); // f32 bits, for the settings meter
pub static RECENT: Mutex<Vec<(u64, Instant)>> = Mutex::new(Vec::new());
pub static RUN_DIR: Mutex<String> = Mutex::new(String::new());

/// Encoded 20 ms frames waiting for `Client::send` on the next client frame.
pub static OUTGOING: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
pub static JOIN_REQ: Mutex<Option<String>> = Mutex::new(None);
pub static LEAVE_REQ: AtomicBool = AtomicBool::new(false);

/// `id -> (display name, persistent key)` as told by the server.
pub static ROSTER: LazyLock<Mutex<HashMap<u64, (String, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub static KEY_OF: LazyLock<Mutex<HashMap<u64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct Pref {
    pub name: String,
    pub volume: f32,
    pub muted: bool,
}
/// Persistent per-player settings, keyed by the player's public key.
pub static PREFS: LazyLock<Mutex<HashMap<String, Pref>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub static GROUP_INPUT: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
pub static MY_GROUP: AtomicU64 = AtomicU64::new(0);
pub static MY_GROUP_NAME: Mutex<String> = Mutex::new(String::new());
pub static MY_GROUP_MEMBERS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
pub static GROUP_REV: AtomicU64 = AtomicU64::new(0);
pub static PUBLIC_GROUPS: Mutex<Vec<PublicGroup>> = Mutex::new(Vec::new());
/// Group the player picked from the "Other groups" list, waiting for a password if needed.
pub static SELECTED_GROUP: Mutex<Option<(String, u32)>> = Mutex::new(None);
pub static GROUP_ERROR: Mutex<String> = Mutex::new(String::new());

#[derive(Clone)]
pub struct PublicGroup {
    pub name: String,
    pub password: bool,
    pub kind: u32,
}

pub fn parse_public_groups(text: &str) {
    let groups = text
        .split(';')
        .filter_map(|entry| {
            let mut parts = entry.split('|');
            let _id = parts.next()?.parse::<u64>().ok()?;
            let name = parts.next()?.to_string();
            let password = parts.next()? == "1";
            let kind = parts.next()?.parse::<u32>().ok()?;
            Some(PublicGroup { name, password, kind })
        })
        .collect();
    *PUBLIC_GROUPS.lock() = groups;
    GROUP_REV.fetch_add(1, Ordering::Relaxed);
}

/// Server-only group membership. Group ids are derived from name + password, so knowing the
/// password is the only way to land in the same group.
pub struct GroupInfo {
    pub name: String,
    pub password: bool,
    pub kind: u32,
}

pub struct ServerState {
    pub member_of: HashMap<u64, u64>,
    pub groups: HashMap<u64, HashSet<u64>>,
    pub group_info: HashMap<u64, GroupInfo>,
    pub seq: u64,
}
pub static SERVER: LazyLock<Mutex<ServerState>> = LazyLock::new(|| {
    Mutex::new(ServerState {
        member_of: HashMap::new(),
        groups: HashMap::new(),
        group_info: HashMap::new(),
        seq: 0,
    })
});

pub fn group_id(name: &str, password: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in name.as_bytes().iter().chain(&[0]).chain(password.as_bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Everything this mod saves lives in `Downloads/Simple Voice Chat`.
pub fn svc_dir() -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let dir = std::path::Path::new(&home).join("Downloads").join("Simple Voice Chat");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir.to_string_lossy().into_owned())
}

pub fn prefs_path() -> Option<String> {
    svc_dir().map(|dir| format!("{dir}/players.txt"))
}

/// Lines are `key<TAB>volume<TAB>muted<TAB>name`.
pub fn load_prefs() {
    let Some(path) = prefs_path() else { return };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut prefs = PREFS.lock();
    for line in text.lines() {
        let mut parts = line.splitn(4, '\t');
        let (Some(key), Some(vol), Some(muted), Some(name)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(volume), Ok(muted)) = (vol.parse::<f32>(), muted.parse::<u8>()) {
            prefs.insert(
                key.to_string(),
                Pref { name: name.to_string(), volume: volume.clamp(0.0, 2.0), muted: muted != 0 },
            );
        }
    }
}

pub fn save_prefs() {
    let Some(path) = prefs_path() else { return };
    let mut text = String::new();
    for (key, p) in PREFS.lock().iter() {
        text.push_str(&format!("{}\t{}\t{}\t{}\n", key, p.volume, p.muted as u8, p.name));
    }
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

/// Server sends `id|name|key;...`; keep the mapping and remember any newly seen player.
pub fn parse_roster(text: &str) {
    let mut roster = ROSTER.lock();
    let mut keys = KEY_OF.lock();
    roster.clear();
    keys.clear();
    for entry in text.split(';') {
        let mut parts = entry.splitn(3, '|');
        let (Some(id), Some(name), Some(key)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(id) = id.parse::<u64>() {
            roster.insert(id, (name.to_string(), key.to_string()));
            keys.insert(id, key.to_string());
            PREFS.lock().entry(key.to_string()).or_insert(Pref {
                name: name.to_string(),
                volume: 1.0,
                muted: false,
            });
        }
    }
}

pub fn volume_of(id: u64) -> f32 {
    let keys = KEY_OF.lock();
    let Some(key) = keys.get(&id) else { return 1.0 };
    PREFS.lock().get(key).map_or(1.0, |p| if p.muted { 0.0 } else { p.volume })
}

pub fn mark_speaking(id: u64) {
    let mut recent = RECENT.lock();
    recent.retain(|(who, at)| *who != id && at.elapsed() < std::time::Duration::from_secs(3));
    recent.push((id, Instant::now()));
}

/// Ids heard in the last 0.7 seconds, for the mic icons above their heads.
pub fn speaking_ids() -> Vec<u64> {
    RECENT
        .lock()
        .iter()
        .filter(|(_, at)| at.elapsed() < std::time::Duration::from_millis(700))
        .map(|(id, _)| *id)
        .collect()
}

pub fn parse_group_members(text: &str) {
    let members = text
        .split(',')
        .filter_map(|part| part.trim().parse::<u64>().ok())
        .collect();
    *MY_GROUP_MEMBERS.lock() = members;
    GROUP_REV.fetch_add(1, Ordering::Relaxed);
}

pub fn player_name(id: u64) -> String {
    ROSTER.lock().get(&id).map_or_else(|| format!("{id}"), |(name, _)| name.clone())
}

pub fn speaking_text() -> String {
    let recent = RECENT.lock();
    let mut names: Vec<String> = recent
        .iter()
        .filter_map(|(id, at)| {
            if at.elapsed() > std::time::Duration::from_millis(700) {
                return None;
            }
            let roster = ROSTER.lock();
            Some(roster.get(id).map_or_else(|| format!("{id}"), |(n, _)| n.clone()))
        })
        .collect();
    if SELF_SPEAKING.load(Ordering::Relaxed) {
        names.insert(0, "you".into());
    }
    names.join(", ")
}
