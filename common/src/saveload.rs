//! This module contains the `Encoder` trait, which is used to serialize and deserialize data.
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::File;
use std::io;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Result, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Override the root for saves and persisted settings before the first use.
pub fn set_save_dir(path: PathBuf) -> std::result::Result<(), PathBuf> {
    SAVE_DIR.set(path)
}

pub fn save_dir() -> &'static Path {
    SAVE_DIR.get().map_or(Path::new("world"), PathBuf::as_path)
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
        let path = Self::filename(name);
        std::fs::create_dir_all(save_dir())
            .and_then(|()| File::create(&path))
            .and_then(|file| {
                let mut w = BufWriter::new(file);
                Self::encode_writer(x, &mut w)?;
                w.flush()
            })
            .map_err(|e| log::error!("failed serializing: {}", e))
            .ok()?;
        Some(())
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

impl Encoder for CompressedBincode {
    const EXTENSION: &'static str = "zip";

    fn encode(x: &impl Serialize) -> Result<Vec<u8>> {
        let encoded = &*Bincode::encode(x)?;
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(encoded, 1); // bigger level values take far too long and only compress a bit better (about 5%)
        Ok(compressed)
    }

    fn decode<T: DeserializeOwned>(x: &[u8]) -> Result<T> {
        let v = &miniz_oxide::inflate::decompress_to_vec_zlib(x)
            .map_err(|_| std::io::Error::new(ErrorKind::Other, "could not decode zipped file"))?;
        Bincode::decode(v)
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
