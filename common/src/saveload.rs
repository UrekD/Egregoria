//! This module contains the `Encoder` trait, which is used to serialize and deserialize data.
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::File;
use std::io;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Result, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod atomic;
mod slots;

use atomic::write_staged;
pub use atomic::{write_atomic, AtomicWriteError};
pub use slots::{
    list_slots, read_slot_meta, slot_paths, write_slot_meta, SlotEntry, SlotId, SlotIdError,
    SlotMeta, SlotPaths, SnapshotKind,
};

static SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(test)]
mod slot_tests;

/// Override the root for saves and persisted settings before the first use.
pub fn set_save_dir(path: PathBuf) -> std::result::Result<(), PathBuf> {
    SAVE_DIR.set(path)
}

pub fn save_dir() -> &'static Path {
    SAVE_DIR.get().map_or(Path::new("world"), PathBuf::as_path)
}

/// Move a world and its replay aside without replacing any existing backup.
pub fn backup_save_files(name: &str, version: Option<&str>) -> Result<Vec<PathBuf>> {
    backup_save_files_in(save_dir(), name, version)
}

fn backup_save_files_in(dir: &Path, name: &str, version: Option<&str>) -> Result<Vec<PathBuf>> {
    let version = version
        .map(str::trim)
        .filter(|version| {
            !version.is_empty()
                && version.len() <= 64
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-+_".contains(&b))
        })
        .unwrap_or("unknown");
    // Move the replay and legacy save before the compressed save so a failed
    // backup can be retried at startup while the refused save is still in place.
    let save_files = [
        format!("{name}_replay.{}", JSONPretty::EXTENSION),
        format!("{name}_replay.{}", CompressedBincode::EXTENSION),
        format!("{name}.{}", Bincode::EXTENSION),
        format!("{name}.{}", CompressedBincode::EXTENSION),
    ];
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let filename = entry.file_name();
        let Some(index) = save_files.iter().position(|name| filename == name.as_str()) else {
            continue;
        };
        let kind = entry.file_type()?;
        if kind.is_file() || kind.is_symlink() {
            files.push((index, entry.path()));
        }
    }
    files.sort_by_key(|(index, _)| *index);
    let mut backups = Vec::with_capacity(files.len());
    for (index, source) in files {
        let name = &save_files[index];
        for suffix in 0.. {
            let filename = if suffix == 0 {
                format!("{name}.{version}.bak")
            } else {
                format!("{name}.{version}.{suffix}.bak")
            };
            let target = dir.join(filename);
            // A same-directory hard link claims the destination atomically, unlike rename.
            match std::fs::hard_link(&source, &target) {
                Ok(()) => {
                    std::fs::remove_file(&source)?;
                    backups.push(target);
                    break;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
    Ok(backups)
}

pub fn walkdir(dir: &Path) -> impl Iterator<Item = PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir not found") {
        let Ok(entry) = entry else {
            continue;
        };
        let ftype = entry.file_type().unwrap();
        if ftype.is_dir() {
            paths.extend(walkdir(&entry.path()));
        } else if ftype.is_file() {
            paths.push(entry.path());
        }
    }
    paths.into_iter()
}

pub trait Encoder {
    const EXTENSION: &'static str;

    fn encode(x: &impl Serialize) -> Result<Vec<u8>>;

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T>;

    fn encode_writer(x: &impl Serialize, mut w: impl Write) -> Result<()> {
        let buf = Self::encode(x)?;
        w.write_all(&buf)
    }

    fn decode_reader<T: DeserializeOwned>(mut r: impl Read) -> Result<T> {
        let mut buf = vec![];
        r.read_to_end(&mut buf)?;
        Self::decode(&buf)
    }

    fn filename(name: &str) -> PathBuf {
        save_dir().join(format!("{}.{}", name, Self::EXTENSION))
    }

    fn load_reader(name: &str) -> Result<BufReader<File>> {
        let file = File::open(Self::filename(name))?;
        Ok(BufReader::new(file))
    }

    fn save(x: &impl Serialize, name: &str) -> Option<()> {
        Self::save_silent(x, name)?;
        log::info!("successfully saved {}", name);
        Some(())
    }

    fn save_silent(x: &impl Serialize, name: &str) -> Option<()> {
        Self::save_atomic(x, name)
            .map_err(|error| log::error!("failed saving {}: {}", name, error))
            .ok()
    }

    /// Encode and atomically replace the file for this encoder and name.
    fn save_atomic(x: &impl Serialize, name: &str) -> std::result::Result<(), AtomicWriteError> {
        std::fs::create_dir_all(save_dir()).map_err(AtomicWriteError::PreCommit)?;
        write_staged(&Self::filename(name), |file| {
            let mut writer = BufWriter::new(file);
            Self::encode_writer(x, &mut writer)?;
            writer.flush()
        })
    }

    fn load<T: DeserializeOwned>(name: &str) -> Result<T> {
        Self::decode_reader(Self::load_reader(name)?)
            .map_err(|err| {
                io::Error::new(
                    ErrorKind::Other,
                    format!("failed deserializing {}: {}", name, err),
                )
            })
            .map(|x| {
                log::info!("successfully loaded {}", name);
                x
            })
    }
}

pub struct Bincode;

use ::bincode::{DefaultOptions, Options};

impl Bincode {
    /// Decode a leading value from a slice, leaving subsequent fields unread.
    pub fn decode_prefix<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
        DefaultOptions::new()
            .allow_trailing_bytes()
            .deserialize(bytes)
            .map_err(|error| io::Error::new(ErrorKind::Other, error))
    }
}

impl Encoder for Bincode {
    const EXTENSION: &'static str = "bc";

    fn encode(x: &impl Serialize) -> Result<Vec<u8>> {
        let mut v = Vec::with_capacity(4096);
        DefaultOptions::new()
            .serialize_into(std::io::Cursor::new(&mut v), x)
            .map_err(|x| std::io::Error::new(ErrorKind::Other, x))?;
        Ok(v)
    }

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T> {
        DefaultOptions::new()
            .deserialize(x)
            .map_err(|x| std::io::Error::new(ErrorKind::Other, x))
    }

    fn encode_writer(x: &impl Serialize, w: impl Write) -> Result<()> {
        DefaultOptions::new()
            .serialize_into(w, x)
            .map_err(|x| std::io::Error::new(ErrorKind::Other, x))
    }

    fn decode_reader<T: DeserializeOwned>(r: impl Read) -> Result<T> {
        DefaultOptions::new()
            .deserialize_from(r)
            .map_err(|x| std::io::Error::new(ErrorKind::Other, x))
    }
}

pub struct CompressedBincode;

impl CompressedBincode {
    /// Decompress a payload so callers can inspect its Bincode prefix before decoding it.
    pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
        miniz_oxide::inflate::decompress_to_vec_zlib(bytes)
            .map_err(|_| std::io::Error::new(ErrorKind::Other, "could not decode zipped file"))
    }
}

impl Encoder for CompressedBincode {
    const EXTENSION: &'static str = "zip";

    fn encode(x: &impl Serialize) -> Result<Vec<u8>> {
        let encoded = &*Bincode::encode(x)?;
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(encoded, 1); // bigger level values take far too long and only compress a bit better (about 5%)
        Ok(compressed)
    }

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T> {
        Bincode::decode(&Self::decompress(x)?)
    }
}

pub struct JSON;

impl Encoder for JSON {
    const EXTENSION: &'static str = "json";

    fn encode(x: &impl Serialize) -> Result<Vec<u8>> {
        serde_json::to_vec(x).map_err(Into::into)
    }

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T> {
        serde_json::from_slice(x).map_err(Into::into)
    }
    fn encode_writer(x: &impl Serialize, w: impl Write) -> Result<()> {
        serde_json::to_writer(w, x).map_err(Into::into)
    }

    fn decode_reader<T: DeserializeOwned>(r: impl Read) -> Result<T> {
        serde_json::from_reader(r).map_err(Into::into)
    }
}

pub struct JSONPretty;

impl Encoder for JSONPretty {
    const EXTENSION: &'static str = "json";

    fn encode(x: &impl Serialize) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(x).map_err(Into::into)
    }

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T> {
        serde_json::from_slice(x).map_err(Into::into)
    }

    fn encode_writer(x: &impl Serialize, w: impl Write) -> Result<()> {
        serde_json::to_writer_pretty(w, x).map_err(Into::into)
    }

    fn decode_reader<T: DeserializeOwned>(r: impl Read) -> Result<T> {
        serde_json::from_reader(r).map_err(Into::into)
    }
}

pub fn load_raw(p: impl AsRef<Path>) -> Result<Vec<u8>> {
    std::fs::read(p)
}

pub fn load_string(p: impl AsRef<Path>) -> Result<String> {
    std::fs::read_to_string(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_dir_routes_world_settings_and_replay() {
        let root = std::env::temp_dir().join(format!(
            "egr-save-dir-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        set_save_dir(root.clone()).unwrap();

        for name in ["settings", "world_replay"] {
            assert_eq!(JSON::filename(name), root.join(format!("{name}.json")));
            JSONPretty::save_silent(&42, name).unwrap();
            assert_eq!(JSON::load::<i32>(name).unwrap(), 42);
        }
        CompressedBincode::save_silent(&43, "world").unwrap();
        assert_eq!(CompressedBincode::load::<i32>("world").unwrap(), 43);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn backup_preserves_save_family_and_existing_backups() {
        let dir = std::env::temp_dir().join(format!(
            "egr-save-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        for (name, bytes) in [
            ("world.bc", "legacy"),
            ("world.zip", "current"),
            ("world_replay.json", "replay"),
            ("world_replay.zip", "compressed replay"),
            ("world.zip.0.6.1.bak", "earlier backup"),
            ("settings.json", "settings"),
            ("world2.zip", "other world"),
            ("world-notes.txt", "notes"),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }

        let backups = backup_save_files_in(&dir, "world", Some("0.6.1")).unwrap();
        for (name, bytes) in [("world2.zip", "other world"), ("world-notes.txt", "notes")] {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), bytes);
        }
        assert_eq!(backups.len(), 4);
        for (name, bytes) in [
            ("world.bc.0.6.1.bak", "legacy"),
            ("world.zip.0.6.1.1.bak", "current"),
            ("world_replay.json.0.6.1.bak", "replay"),
            ("world_replay.zip.0.6.1.bak", "compressed replay"),
            ("world.zip.0.6.1.bak", "earlier backup"),
            ("settings.json", "settings"),
        ] {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), bytes);
        }
        for name in [
            "world.bc",
            "world.zip",
            "world_replay.json",
            "world_replay.zip",
        ] {
            assert!(!dir.join(name).exists());
        }

        std::fs::write(dir.join("world.zip"), "unreadable").unwrap();
        backup_save_files_in(&dir, "world", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("world.zip.unknown.bak")).unwrap(),
            "unreadable"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
