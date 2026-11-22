use super::{
    write_atomic, AtomicWriteError, Bincode, CompressedBincode, Encoder, JSONPretty, JSON,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SLOT_ID_LEN: usize = 32;
const REPLAY_SUFFIX: &str = "_replay";
const AUTOSAVE_PREFIX: &str = "autosave_";

/// A save slot's lowercase filename stem. The city's display name is stored separately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(String);

/// A name cannot be used as a save slot filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotIdError;

impl fmt::Display for SlotIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "slot id must be an unreserved name with 1 to {MAX_SLOT_ID_LEN} ASCII letters, digits, underscores, or hyphens"
        )
    }
}

impl std::error::Error for SlotIdError {}

impl SlotId {
    /// Validate a user stem and normalize its case. Use `slot_paths` to check a save root.
    pub fn new(name: &str) -> Result<Self, SlotIdError> {
        if name.is_empty() || name.len() > MAX_SLOT_ID_LEN || !name.chars().all(is_slot_char) {
            return Err(SlotIdError);
        }
        let name = name.to_ascii_lowercase();
        if name.ends_with(REPLAY_SUFFIX)
            || name.starts_with(AUTOSAVE_PREFIX)
            || matches!(
                name.as_str(),
                "settings" | "bindings" | "lot_brush" | "netinfo" | "camera3d" | "camera"
            )
            || is_device_stem(&name)
        {
            return Err(SlotIdError);
        }
        Ok(Self(name))
    }

    /// Generate a valid user stem distinct from every stem in `existing`.
    pub fn from_display_name(name: &str, existing: &[SlotId]) -> Self {
        let mut base = name
            .split(|ch| !is_slot_char(ch))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        base.make_ascii_lowercase();
        if base.is_empty() {
            base.push_str("city");
        }
        if base.starts_with(AUTOSAVE_PREFIX) {
            base.insert_str(0, "city_");
        }
        base.truncate(MAX_SLOT_ID_LEN);

        let available = |name: &str| Self::new(name).ok().filter(|id| !existing.contains(id));
        if let Some(id) = available(&base) {
            return id;
        }
        for number in 2usize.. {
            let suffix = format!("_{number}");
            let prefix_len = base.len().min(MAX_SLOT_ID_LEN - suffix.len());
            if let Some(id) = available(&format!("{}{suffix}", &base[..prefix_len])) {
                return id;
            }
        }
        unreachable!()
    }

    pub fn legacy() -> Self {
        Self("world".to_owned())
    }

    /// Construct a reserved slot for the autosave rotation.
    pub fn autosave(n: u8) -> Self {
        Self(format!("{AUTOSAVE_PREFIX}{n}"))
    }

    pub fn is_autosave(&self) -> bool {
        self.0.starts_with(AUTOSAVE_PREFIX)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_disk(stem: &str) -> Result<Self, SlotIdError> {
        if let Some(number) = stem.strip_prefix(AUTOSAVE_PREFIX) {
            let id = Self::autosave(number.parse().map_err(|_| SlotIdError)?);
            return (id.as_str() == stem).then_some(id).ok_or(SlotIdError);
        }
        Self::new(stem)
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

fn is_device_stem(stem: &str) -> bool {
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || matches!(
            stem.as_bytes(),
            [b'c', b'o', b'm', b'1'..=b'9'] | [b'l', b'p', b't', b'1'..=b'9']
        )
}

/// Identifies one snapshot publication independently of its game tick or file timestamps.
///
/// The simulation save wrapper should put a fresh random revision in its versioned header
/// and store the same value in the replay JSON's top-level revision and in `SlotMeta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotRevision(u128);

impl SnapshotRevision {
    pub const fn new(revision: u128) -> Self {
        Self(revision)
    }

    pub const fn get(self) -> u128 {
        self.0
    }

    pub fn matches_sidecar(self, revision: Option<u128>) -> bool {
        revision == Some(self.get())
    }
}

/// Orbit pose stored with a city snapshot. The viewport is local to the player.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotCamera {
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotMeta {
    pub display_name: String,
    pub game_tick: u64,
    pub population: u32,
    pub money_cents: i64,
    pub saved_at_unix: u64,
    #[serde(default)]
    pub revision: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<SlotCamera>,
}

#[cfg(test)]
#[test]
fn af2_slot_camera_metadata_round_trips() {
    let mut sidecar = serde_json::json!({
        "display_name": "Port",
        "game_tick": 8,
        "population": 0,
        "money_cents": 20,
        "saved_at_unix": 42
    });
    let legacy: SlotMeta = serde_json::from_value(sidecar.clone()).unwrap();
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("camera")
        .is_none());

    let camera = serde_json::json!({
        "position": [42.0, 150.0, 10.0],
        "yaw": -0.25,
        "pitch": 0.75,
        "distance": 410.0
    });
    sidecar["camera"] = camera.clone();
    let meta: SlotMeta = serde_json::from_value(sidecar).unwrap();
    assert_eq!(serde_json::to_value(meta).unwrap()["camera"], camera);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Zip,
    Bc,
    /// A ZIP path exists but is a symlink or another nonregular entry.
    Obstructed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlotEntry {
    pub id: SlotId,
    pub meta: Option<SlotMeta>,
    pub snapshot: SnapshotKind,
    pub replay_available: bool,
    pub modified_unix: u64,
    pub modified: SystemTime,
    pub revision: Option<SnapshotRevision>,
    /// Existing members retain their exact spelling. Absent members use canonical paths.
    pub paths: SlotPaths,
    file: SnapshotFile,
}

impl SlotEntry {
    pub fn snapshot_path(&self) -> &Path {
        match self.snapshot {
            SnapshotKind::Bc => &self.paths.bc,
            _ => &self.paths.zip,
        }
    }

    pub fn same_snapshot_as(&self, other: &Self) -> bool {
        self.id == other.id
            && self.snapshot == other.snapshot
            && self.snapshot_path() == other.snapshot_path()
            && self.revision == other.revision
            && self.file == other.file
    }

    /// Compare the listing to the handle that will be used for the read.
    pub fn matches_snapshot_file(
        &self,
        path: &Path,
        kind: SnapshotKind,
        file: &File,
    ) -> io::Result<bool> {
        Ok(self.snapshot == kind
            && self.snapshot_path() == path
            && self.file == SnapshotFile::from_metadata(&file.metadata()?)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotFile {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    device_inode: (u64, u64),
}

impl SnapshotFile {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
            #[cfg(unix)]
            device_inode: (metadata.dev(), metadata.ino()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotPaths {
    pub zip: PathBuf,
    pub bc: PathBuf,
    pub replay: PathBuf,
    pub meta: PathBuf,
}

// Ordering keeps the ZIP in place until any BC and sidecars have been removed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MemberKind {
    Replay,
    Meta,
    Bc,
    Zip,
    StandaloneJson,
}

impl MemberKind {
    fn split_name(name: &str) -> Option<(&str, Self)> {
        [
            (".meta.json", Self::Meta),
            ("_replay.json", Self::Replay),
            (".zip", Self::Zip),
            (".bc", Self::Bc),
            (".json", Self::StandaloneJson),
        ]
        .into_iter()
        .find_map(|(suffix, kind)| name.strip_suffix(suffix).map(|stem| (stem, kind)))
    }
}

struct Member {
    kind: MemberKind,
    path: PathBuf,
    metadata: Metadata,
    canonical: bool,
}

#[derive(Default)]
struct SlotFamily {
    reserved: bool,
    members: Vec<Member>,
}

impl SlotFamily {
    fn member(&self, kind: MemberKind) -> Option<&Member> {
        self.members.iter().find(|member| member.kind == kind)
    }

    fn paths(&self, root: &Path, id: &SlotId) -> SlotPaths {
        let resolve = |kind, name: String| {
            self.member(kind)
                .map_or_else(|| root.join(name), |member| member.path.clone())
        };
        SlotPaths {
            zip: resolve(
                MemberKind::Zip,
                format!("{id}.{}", CompressedBincode::EXTENSION),
            ),
            bc: resolve(MemberKind::Bc, format!("{id}.{}", Bincode::EXTENSION)),
            replay: resolve(
                MemberKind::Replay,
                format!("{id}{REPLAY_SUFFIX}.{}", JSON::EXTENSION),
            ),
            meta: resolve(MemberKind::Meta, format!("{id}.meta.{}", JSON::EXTENSION)),
        }
    }

    fn require_unreserved(&self, id: &SlotId) -> io::Result<()> {
        if self.reserved {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("slot id {id} is reserved by an existing JSON file"),
            ));
        }
        Ok(())
    }
}

/// Resolve paths under `root`, rejecting stems already used by standalone JSON files.
pub fn slot_paths(root: &Path, id: &SlotId) -> io::Result<SlotPaths> {
    let family = read_families(root, Some(id))?
        .remove(id)
        .unwrap_or_default();
    family.require_unreserved(id)?;
    Ok(family.paths(root, id))
}

/// Delete every spelling of the slot's replay, metadata, legacy BC and ZIP files.
/// Missing files are accepted so a partially completed deletion can be retried.
/// A standalone JSON reservation prevents deletion, as it does for `slot_paths`.
/// A directory at any spelling of the ZIP path returns `ErrorKind::IsADirectory`
/// before any member of the slot is removed.
pub fn delete_slot(root: &Path, id: &SlotId) -> io::Result<()> {
    let family = read_families(root, Some(id))?
        .remove(id)
        .unwrap_or_default();
    family.require_unreserved(id)?;
    if let Some(zip) = family
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Zip && member.metadata.is_dir())
    {
        return Err(io::Error::new(
            ErrorKind::IsADirectory,
            format!("slot ZIP path {} is a directory", zip.path.display()),
        ));
    }
    for member in family.members {
        #[cfg(windows)]
        let result = {
            use std::os::windows::fs::FileTypeExt;
            if member.metadata.file_type().is_symlink_dir() {
                fs::remove_dir(member.path)
            } else {
                fs::remove_file(member.path)
            }
        };
        #[cfg(not(windows))]
        let result = fs::remove_file(member.path);

        match result {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Replace the metadata sidecar. Write it after committing the snapshot.
pub fn write_slot_meta(root: &Path, id: &SlotId, meta: &SlotMeta) -> Result<(), AtomicWriteError> {
    let paths = slot_paths(root, id).map_err(AtomicWriteError::PreCommit)?;
    let bytes = JSONPretty::encode(meta).map_err(AtomicWriteError::PreCommit)?;
    write_atomic(&paths.meta, &bytes)
}

/// Read the sidecar directly. Use `list_slots` to get metadata checked against the snapshot.
pub fn read_slot_meta(root: &Path, id: &SlotId) -> io::Result<SlotMeta> {
    let file = open_regular(&slot_paths(root, id)?.meta)?;
    JSON::decode_reader(BufReader::new(file))
}

/// List slots by descending saved/modified time, then ascending slot ID.
/// A missing root contains no slots. Sidecar freshness uses snapshot mtime.
pub fn list_slots(root: &Path) -> io::Result<Vec<SlotEntry>> {
    list_slots_with_revisions(root, |_, _| None)
}

/// List slots with revisions read by the caller from the selected snapshot's header.
///
/// A known snapshot revision determines metadata freshness. Without a revision,
/// metadata must be at least as recent as the snapshot. Ordering uses fresh
/// metadata's `saved_at_unix` or falls back to the snapshot's modification time.
///
/// Use the supplied path as-is for I/O. Obstructed snapshots are never passed to the
/// callback. Return `None` for legacy snapshots or when no revision could be read.
pub fn list_slots_with_revisions(
    root: &Path,
    mut snapshot_revision: impl FnMut(&Path, SnapshotKind) -> Option<SnapshotRevision>,
) -> io::Result<Vec<SlotEntry>> {
    let families = read_families(root, None)?;
    let mut slots = Vec::with_capacity(families.len());
    for (id, family) in families {
        if family.reserved {
            continue;
        }
        let paths = family.paths(root, &id);
        let (snapshot, member) = if let Some(zip) = family.member(MemberKind::Zip) {
            let kind = if zip.metadata.is_file() {
                SnapshotKind::Zip
            } else {
                SnapshotKind::Obstructed
            };
            (kind, zip)
        } else if let Some(bc) = family
            .member(MemberKind::Bc)
            .filter(|member| member.metadata.is_file())
        {
            (SnapshotKind::Bc, bc)
        } else {
            continue;
        };

        let Ok(file) =
            report_entry_error(SnapshotFile::from_metadata(&member.metadata), &member.path)
        else {
            continue;
        };
        let modified = file.modified;
        let (meta, replay_available, revision) = if snapshot == SnapshotKind::Obstructed {
            (None, false, None)
        } else {
            let revision = snapshot_revision(&member.path, snapshot);
            let meta = fresh_slot_meta(&paths.meta, modified, revision);
            let replay_available = available_replay(&paths.replay, revision);
            (meta, replay_available, revision)
        };
        slots.push(SlotEntry {
            id,
            meta,
            snapshot,
            replay_available,
            modified_unix: modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            modified,
            revision,
            paths,
            file,
        });
    }

    let saved_at = |entry: &SlotEntry| {
        entry
            .meta
            .as_ref()
            .map_or(entry.modified_unix, |meta| meta.saved_at_unix)
    };
    slots.sort_by(|a, b| saved_at(b).cmp(&saved_at(a)).then_with(|| a.id.cmp(&b.id)));
    Ok(slots)
}

fn read_families(root: &Path, only: Option<&SlotId>) -> io::Result<BTreeMap<SlotId, SlotFamily>> {
    let mut families = BTreeMap::<_, SlotFamily>::new();
    let dir = match fs::read_dir(root) {
        Ok(dir) => dir,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(families),
        Err(error) => return Err(error),
    };

    for entry in dir {
        let entry = match report_entry_error(entry, root) {
            Ok(entry) => entry,
            Err(error) if only.is_some() => return Err(error),
            Err(_) => continue,
        };
        let filename = entry.file_name();
        let Some(name) = filename.to_str() else {
            continue;
        };
        let lowercase = name.to_ascii_lowercase();
        let Some((stem, kind)) = MemberKind::split_name(&lowercase) else {
            continue;
        };
        let Ok(id) = SlotId::from_disk(stem) else {
            continue;
        };
        if only.is_some_and(|only| only != &id) {
            continue;
        }
        let path = entry.path();
        let metadata = match report_entry_error(path.symlink_metadata(), &path) {
            Ok(metadata) => metadata,
            Err(error) if only.is_some() && error.kind() != ErrorKind::NotFound => {
                return Err(error)
            }
            Err(_) => continue,
        };
        let family = families.entry(id).or_default();
        if kind == MemberKind::StandaloneJson {
            family.reserved |= metadata.is_file() || metadata.is_symlink();
        } else {
            family.members.push(Member {
                kind,
                path,
                metadata,
                canonical: name == lowercase,
            });
        }
    }
    for family in families.values_mut() {
        // Prefer regular sidecars and BC files. A canonical ZIP takes precedence even
        // when obstructed, matching the loader's behavior at that path.
        family.members.sort_by(|a, b| {
            let priority = |member: &Member| {
                (
                    member.kind,
                    member.kind != MemberKind::Zip && !member.metadata.is_file(),
                    !member.canonical,
                )
            };
            priority(a)
                .cmp(&priority(b))
                .then_with(|| a.path.cmp(&b.path))
        });
    }
    Ok(families)
}

fn report_entry_error<T>(result: io::Result<T>, path: &Path) -> io::Result<T> {
    result.inspect_err(|error| {
        if error.kind() != ErrorKind::NotFound {
            log::warn!(
                "could not inspect save slot path {}: {error}",
                path.display()
            );
        }
    })
}

fn fresh_slot_meta(
    path: &Path,
    snapshot_modified: SystemTime,
    revision: Option<SnapshotRevision>,
) -> Option<SlotMeta> {
    let file = open_regular(path).ok()?;
    if revision.is_none() {
        let modified = report_entry_error(file.metadata().and_then(|m| m.modified()), path).ok()?;
        if modified < snapshot_modified {
            return None;
        }
    }
    let meta: SlotMeta = JSON::decode_reader(BufReader::new(file)).ok()?;
    revision
        .is_none_or(|revision| revision.matches_sidecar(meta.revision))
        .then_some(meta)
}

fn available_replay(path: &Path, revision: Option<SnapshotRevision>) -> bool {
    #[derive(Deserialize)]
    struct ReplayRevision {
        #[serde(default)]
        revision: Option<u128>,
    }

    let Some(revision) = revision else {
        return report_entry_error(path.symlink_metadata(), path).is_ok_and(|meta| meta.is_file());
    };
    open_regular(path)
        .and_then(|file| JSON::decode_reader::<ReplayRevision>(BufReader::new(file)))
        .is_ok_and(|sidecar| revision.matches_sidecar(sidecar.revision))
}

impl SlotPaths {
    /// Open a slot member without following a final symlink or waiting on a FIFO.
    pub fn open_member(path: &Path) -> io::Result<File> {
        open_regular(path)
    }
}

impl CompressedBincode {
    /// Inflate a complete zlib payload, limiting the decompressed byte count.
    pub fn decompress_limited(bytes: &[u8], max_output: usize) -> io::Result<Vec<u8>> {
        use miniz_oxide::inflate::{decompress_to_vec_zlib_with_limit, TINFLStatus};

        decompress_to_vec_zlib_with_limit(bytes, max_output).map_err(|error| {
            if error.status == TINFLStatus::HasMoreOutput {
                io::Error::new(
                    ErrorKind::FileTooLarge,
                    "snapshot exceeds the inflate size limit",
                )
            } else {
                io::Error::new(ErrorKind::InvalidData, error.to_string())
            }
        })
    }

    /// Inflate into a caller-supplied prefix buffer, with at most 4 KiB of input.
    pub fn read_prefix(reader: impl Read, prefix: &mut [u8]) -> io::Result<usize> {
        use miniz_oxide::inflate::core::{decompress, inflate_flags::*, DecompressorOxide};
        use miniz_oxide::inflate::TINFLStatus;

        let mut input = Vec::new();
        reader.take(4096).read_to_end(&mut input)?;
        let (status, _, written) = decompress(
            &mut DecompressorOxide::new(),
            &input,
            prefix,
            0,
            TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
        );
        match status {
            TINFLStatus::Done | TINFLStatus::HasMoreOutput => Ok(written),
            _ => Err(io::Error::new(
                ErrorKind::InvalidData,
                "could not decode zipped file prefix",
            )),
        }
    }
}

fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(unix_slot_open_flags()?);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_symlink())
        {
            nonregular_member(path)
        } else {
            error
        }
    })?;
    let metadata = report_entry_error(file.metadata(), path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(nonregular_member(path));
        }
    }
    if !metadata.is_file() {
        return Err(nonregular_member(path));
    }
    Ok(file)
}

fn nonregular_member(path: &Path) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("slot path {} is not a regular file", path.display()),
    )
}

#[cfg(unix)]
fn unix_slot_open_flags() -> io::Result<i32> {
    // O_NOFOLLOW handles a replaced symlink; O_NONBLOCK lets us reject a replaced FIFO.
    // The standard library accepts the platform flags but does not export them.
    if cfg!(any(target_os = "linux", target_os = "android")) {
        let no_follow = if cfg!(any(
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "m68k",
            target_arch = "powerpc",
            target_arch = "powerpc64"
        )) {
            0x8000
        } else {
            0x20000
        };
        let nonblock = if cfg!(any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        )) {
            0x80
        } else if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
            0x4000
        } else {
            0x800
        };
        Ok(no_follow | nonblock)
    } else if cfg!(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )) {
        Ok(0x100 | 0x4)
    } else {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "platform slot open flags are unavailable",
        ))
    }
}
