use super::{
    write_atomic, AtomicWriteError, Bincode, CompressedBincode, Encoder, JSONPretty, JSON,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SLOT_ID_LEN: usize = 32;
const REPLAY_SUFFIX: &str = "_replay";

/// A save slot's filename stem. The city's display name is stored separately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(String);

/// A name cannot be used as a save slot filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotIdError;

impl fmt::Display for SlotIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "slot id must have 1 to {MAX_SLOT_ID_LEN} ASCII letters, digits, underscores, or hyphens and cannot end in {REPLAY_SUFFIX}"
        )
    }
}

impl std::error::Error for SlotIdError {}

impl SlotId {
    /// Validate the stem syntax. Use `slot_paths` to check a specific save root.
    pub fn new(name: &str) -> Result<Self, SlotIdError> {
        if name.is_empty()
            || name.len() > MAX_SLOT_ID_LEN
            || !name.chars().all(is_slot_char)
            || name.ends_with(REPLAY_SUFFIX)
        {
            return Err(SlotIdError);
        }
        Ok(Self(name.to_owned()))
    }

    /// Generate a valid stem distinct from every stem in `existing`.
    pub fn from_display_name(name: &str, existing: &[SlotId]) -> Self {
        let mut base = name
            .split(|ch| !is_slot_char(ch))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        if base.is_empty() {
            base.push_str("city");
        }
        base.truncate(MAX_SLOT_ID_LEN);

        if !base.ends_with(REPLAY_SUFFIX) && !existing.iter().any(|id| id.as_str() == base) {
            return Self(base);
        }
        for number in 2usize.. {
            let suffix = format!("_{number}");
            let prefix_len = base.len().min(MAX_SLOT_ID_LEN - suffix.len());
            let candidate = format!("{}{suffix}", &base[..prefix_len]);
            if !existing.iter().any(|id| id.as_str() == candidate) {
                return Self(candidate);
            }
        }
        unreachable!()
    }

    pub fn legacy() -> Self {
        Self("world".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_slot_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMeta {
    pub display_name: String,
    pub game_tick: u64,
    pub population: u32,
    pub money_cents: i64,
    pub saved_at_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Zip,
    Bc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotEntry {
    pub id: SlotId,
    pub meta: Option<SlotMeta>,
    pub snapshot: SnapshotKind,
    pub replay_available: bool,
    pub modified_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotPaths {
    pub zip: PathBuf,
    pub bc: PathBuf,
    pub replay: PathBuf,
    pub meta: PathBuf,
}

impl SlotPaths {
    fn new(root: &Path, id: &SlotId) -> Self {
        Self {
            zip: root.join(format!("{id}.{}", CompressedBincode::EXTENSION)),
            bc: root.join(format!("{id}.{}", Bincode::EXTENSION)),
            replay: root.join(format!("{id}{REPLAY_SUFFIX}.{}", JSON::EXTENSION)),
            meta: root.join(format!("{id}.meta.{}", JSON::EXTENSION)),
        }
    }
}

/// Build paths under `root`, rejecting stems already used by standalone JSON files.
pub fn slot_paths(root: &Path, id: &SlotId) -> io::Result<SlotPaths> {
    if is_reserved(root, id)? {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("slot id {id} is reserved by an existing JSON file"),
        ));
    }
    Ok(SlotPaths::new(root, id))
}

fn is_reserved(root: &Path, id: &SlotId) -> io::Result<bool> {
    match fs::symlink_metadata(root.join(format!("{id}.{}", JSON::EXTENSION))) {
        Ok(metadata) => Ok(metadata.is_file() || metadata.is_symlink()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Replace the metadata sidecar. Write it after committing the snapshot.
pub fn write_slot_meta(root: &Path, id: &SlotId, meta: &SlotMeta) -> Result<(), AtomicWriteError> {
    let paths = slot_paths(root, id).map_err(AtomicWriteError::PreCommit)?;
    let bytes = JSONPretty::encode(meta).map_err(AtomicWriteError::PreCommit)?;
    write_atomic(&paths.meta, &bytes)
}

/// Read the sidecar directly. Use `list_slots` to get metadata checked against the snapshot.
pub fn read_slot_meta(root: &Path, id: &SlotId) -> io::Result<SlotMeta> {
    let file = File::open(slot_paths(root, id)?.meta)?;
    JSON::decode_reader(BufReader::new(file))
}

/// List snapshot slots in descending saved/modified time, then ascending slot ID.
/// A root that has yet to be created contains no slots.
pub fn list_slots(root: &Path) -> io::Result<Vec<SlotEntry>> {
    let dir = match fs::read_dir(root) {
        Ok(dir) => dir,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut snapshots = BTreeMap::new();
    for entry in dir {
        let path = entry?.path();
        let kind = match path.extension().and_then(|ext| ext.to_str()) {
            Some(CompressedBincode::EXTENSION) => SnapshotKind::Zip,
            Some(Bincode::EXTENSION) => SnapshotKind::Bc,
            _ => continue,
        };
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(id) = SlotId::new(stem) else {
            continue;
        };
        let Some(metadata) = file_metadata(&path)? else {
            continue;
        };
        if kind == SnapshotKind::Zip {
            snapshots.insert(id, (kind, metadata));
        } else {
            snapshots.entry(id).or_insert((kind, metadata));
        }
    }

    let mut slots = Vec::with_capacity(snapshots.len());
    for (id, (snapshot, metadata)) in snapshots {
        if is_reserved(root, &id)? {
            continue;
        }
        let paths = SlotPaths::new(root, &id);
        let modified = metadata.modified()?;
        slots.push(SlotEntry {
            id,
            meta: fresh_slot_meta(&paths.meta, modified),
            snapshot,
            replay_available: file_metadata(&paths.replay)?.is_some(),
            modified_unix: modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
    }
    slots.sort_by(|a, b| {
        let saved_at = |entry: &SlotEntry| {
            entry
                .meta
                .as_ref()
                .map_or(entry.modified_unix, |meta| meta.saved_at_unix)
        };
        saved_at(b).cmp(&saved_at(a)).then_with(|| a.id.cmp(&b.id))
    });
    Ok(slots)
}

fn file_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn fresh_slot_meta(path: &Path, snapshot_modified: SystemTime) -> Option<SlotMeta> {
    file_metadata(path).ok()??;
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.modified().ok()? < snapshot_modified {
        return None;
    }
    JSON::decode_reader(BufReader::new(file)).ok()
}
