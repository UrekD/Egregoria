use crate::osm_city::{read_overpass, GeoBounds, LESCE_BOUNDS};
use geom::{vec2, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub const LESCE_SHA256: &str = "ba6bf6be50435cd00402f024f60adfafef6dbf8851ca5768c1beb322612150db";
pub const SAVA_SHA256: &str = "a82e383d6b0a374685a31ecf13043a78ff4f1f04075a54461023f4ef6582512e";
pub const DEFAULT_LESCE: &str =
    "/var/tmp/egr-keep/osm/signals/lesce-20261122-signals.overpass.json";
pub const DEFAULT_SAVA: &str = "/var/tmp/egr/osm-cache/lesce-town-sava-20261122.overpass.json";
pub const ATTRIBUTION: &str = "© OpenStreetMap contributors, ODbL 1.0";
const SNAPSHOT: &str = "2026-11-22T12:24:51Z";
const LESCE_QUERY: &str = include_str!("../../../scripts/osm/lesce.overpassql");
const SAVA_QUERY: &str = include_str!("../../../scripts/osm/lesce-town-sava.overpassql");
const SAVA_BOUNDS: GeoBounds = GeoBounds {
    south: 46.346,
    west: 14.134,
    north: 46.373,
    east: 14.173,
};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TownConfig {
    pub map_chunks: u16,
    #[serde(serialize_with = "serialize_point")]
    pub owned_min: Vec2,
    #[serde(serialize_with = "serialize_point")]
    pub owned_max: Vec2,
    /// Geographic coordinate projected to the centre of the start area before translation.
    pub latitude: f64,
    pub longitude: f64,
    #[serde(serialize_with = "serialize_point")]
    pub translation: Vec2,
}

impl Default for TownConfig {
    fn default() -> Self {
        Self {
            map_chunks: 6,
            owned_min: Vec2::ZERO,
            owned_max: vec2(3072.0, 3072.0),
            latitude: 46.3605,
            longitude: 14.1567,
            translation: vec2(180.0, 140.0),
        }
    }
}

impl TownConfig {
    pub const SCALE: f32 = 2.0;

    pub fn project(self, lat: f64, lon: f64) -> Vec2 {
        let centre = LESCE_BOUNDS.project(self.latitude, self.longitude);
        let point = LESCE_BOUNDS.project(lat, lon);
        (self.owned_min + self.owned_max) * 0.5 + self.translation + Self::SCALE * (point - centre)
    }

    pub(crate) fn validate(self) -> Result<(), String> {
        let extent = f32::from(self.map_chunks) * 512.0;
        if !self.owned_min.is_finite()
            || !self.owned_max.is_finite()
            || !self.translation.is_finite()
            || !(-90.0..=90.0).contains(&self.latitude)
            || !(-180.0..=180.0).contains(&self.longitude)
            || self.owned_min.x < 0.0
            || self.owned_min.y < 0.0
            || self.owned_max.x > extent
            || self.owned_max.y > extent
            || self.owned_max.x - self.owned_min.x < 64.0
            || self.owned_max.y - self.owned_min.y < 64.0
        {
            return Err("CITYMAP owned rectangle does not fit within its map extent".into());
        }
        Ok(())
    }
}

pub(crate) fn xy(point: Vec2) -> [f32; 2] {
    [point.x, point.y]
}

pub(crate) fn serialize_point<S: serde::Serializer>(
    point: &Vec2,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    xy(*point).serialize(serializer)
}

pub(crate) fn serialize_points<S: serde::Serializer>(
    points: &[Vec2],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(points.iter().copied().map(xy))
}

pub(crate) fn serialize_optional_point<S: serde::Serializer>(
    point: &Option<Vec2>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    point.map(xy).serialize(serializer)
}

pub struct SourcePaths<'a> {
    pub lesce: &'a Path,
    pub sava: Option<&'a Path>,
}

pub struct PinnedInputs {
    pub lesce: Extract,
    pub sava: Option<Extract>,
}

pub struct Extract {
    pub provenance: Provenance,
    elements: Vec<Element>,
}

impl Extract {
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Provenance {
    pub query: &'static str,
    pub requested_snapshot: &'static str,
    pub timestamp_osm_base: String,
    pub bounds: GeoBounds,
    pub sha256: &'static str,
    pub attribution: &'static str,
    pub elements: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Node,
    Way,
    Relation,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Coordinate {
    pub lat: f64,
    pub lon: f64,
}

impl Coordinate {
    pub fn project(self, config: TownConfig) -> Vec2 {
        config.project(self.lat, self.lon)
    }
}

/// Read-only view of bytes already validated by the legacy Overpass reader.
/// The shared importer keeps its raw element types private.
#[derive(Debug, Deserialize)]
pub struct Element {
    #[serde(rename = "type")]
    pub kind: Kind,
    pub id: u64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub nodes: Vec<u64>,
    #[serde(default)]
    pub geometry: Vec<Coordinate>,
    #[serde(default)]
    pub members: Vec<Member>,
}

impl Element {
    pub fn tag(&self, key: &str) -> &str {
        self.tags.get(key).map_or("", String::as_str)
    }

    pub fn point(&self, config: TownConfig) -> Option<Vec2> {
        Some(config.project(self.lat?, self.lon?))
    }
}

#[derive(Debug, Deserialize)]
pub struct Member {
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(rename = "ref")]
    pub id: u64,
    pub role: String,
    #[serde(default)]
    pub geometry: Vec<Coordinate>,
}

#[derive(Deserialize)]
struct ElementView {
    elements: Vec<Element>,
}

pub fn read_inputs(paths: SourcePaths<'_>) -> Result<PinnedInputs, String> {
    Ok(PinnedInputs {
        lesce: read_pin(paths.lesce, LESCE_SHA256, LESCE_BOUNDS, LESCE_QUERY)?,
        sava: paths
            .sava
            .map(|path| read_pin(path, SAVA_SHA256, SAVA_BOUNDS, SAVA_QUERY))
            .transpose()?,
    })
}

fn read_pin(
    path: &Path,
    sha256: &'static str,
    bounds: GeoBounds,
    query: &'static str,
) -> Result<Extract, String> {
    let read = || -> Result<Extract, String> {
        const LIMIT: u64 = 16 * 1024 * 1024;
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
            .map_err(|err| err.to_string())?;
        if bytes.len() as u64 > LIMIT {
            return Err("Overpass response exceeds 16 MiB".into());
        }
        let digest = digest(&bytes)?;
        if digest != sha256 {
            return Err(format!("SHA-256 mismatch: expected {sha256}, got {digest}"));
        }
        let validated = read_overpass(bytes.as_slice()).map_err(|err| err.to_string())?;
        let mut view: ElementView =
            serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
        view.elements
            .sort_by_key(|element| (element.kind, element.id));
        Ok(Extract {
            provenance: Provenance {
                query,
                requested_snapshot: SNAPSHOT,
                timestamp_osm_base: validated.timestamp().ok_or("missing OSM timestamp")?.into(),
                bounds,
                sha256,
                attribution: ATTRIBUTION,
                elements: view.elements.len(),
            },
            elements: view.elements,
        })
    };
    read().map_err(|err| format!("{}: {err}", path.display()))
}

fn digest(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|err| format!("start sha256sum: {err}"))?;
    let written = child.stdin.take().unwrap().write_all(bytes);
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    written.map_err(|err| format!("write to sha256sum: {err}"))?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .filter(|hash| output.status.success() && hash.len() == 64)
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum failed to digest the extract".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citymap_projection_doubles_offsets_about_the_owned_frame_centre() {
        let config = TownConfig {
            translation: Vec2::ZERO,
            ..TownConfig::default()
        };
        let centre = LESCE_BOUNDS.project(46.3605, 14.1567);
        let sample = LESCE_BOUNDS.project(46.3615, 14.1577);
        assert_eq!(config.project(46.3605, 14.1567), vec2(1536.0, 1536.0));
        assert_eq!(
            config.project(46.3615, 14.1577),
            vec2(1536.0, 1536.0) + 2.0 * (sample - centre)
        );
    }

    #[test]
    fn citymap_digest_checks_the_exact_validated_bytes() {
        assert_eq!(
            digest(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
