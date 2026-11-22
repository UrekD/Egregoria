use serde::{de::IgnoredAny, Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::io::Read;

use super::ImportError;

pub(crate) const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ELEMENTS: usize = 20_000;
const MAX_GEOMETRY: usize = 300_000;

#[derive(Debug, Deserialize)]
pub struct OverpassExtract {
    #[serde(default)]
    osm3s: Metadata,
    #[serde(default, deserialize_with = "present")]
    remark: bool,
    pub(super) elements: Vec<Element>,
}

impl OverpassExtract {
    pub fn timestamp(&self) -> Option<&str> {
        self.osm3s.timestamp_osm_base.as_deref()
    }
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(deserializer).map(|_| true)
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    timestamp_osm_base: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub(super) enum ElementKind {
    Node,
    Way,
    Relation,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub(super) struct Coordinate {
    pub lat: f64,
    pub lon: f64,
}

impl Coordinate {
    fn valid(self) -> bool {
        (-90.0..=90.0).contains(&self.lat) && (-180.0..=180.0).contains(&self.lon)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Element {
    #[serde(rename = "type")]
    pub kind: ElementKind,
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

    pub fn point(&self) -> Option<Coordinate> {
        Some(Coordinate {
            lat: self.lat?,
            lon: self.lon?,
        })
    }

    pub fn label(&self) -> String {
        let kind = match self.kind {
            ElementKind::Node => "node",
            ElementKind::Way => "way",
            ElementKind::Relation => "relation",
        };
        format!("{kind}/{}", self.id)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Member {
    #[serde(rename = "type")]
    pub kind: ElementKind,
    #[serde(rename = "ref")]
    pub id: u64,
    pub role: String,
    #[serde(default)]
    pub geometry: Vec<Coordinate>,
}

pub fn read_overpass(reader: impl Read) -> Result<OverpassExtract, ImportError> {
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Overpass response exceeds 16 MiB".into());
    }
    let mut extract: OverpassExtract = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid Overpass JSON: {error}"))?;
    if extract.remark {
        return Err("Overpass response has a remark and may be incomplete".into());
    }
    if extract.elements.is_empty() || extract.elements.len() > MAX_ELEMENTS {
        return Err("Overpass element count is outside the supported range".into());
    }
    extract.elements.sort_by_key(|e| (e.kind, e.id));
    for pair in extract.elements.windows(2) {
        if (pair[0].kind, pair[0].id) == (pair[1].kind, pair[1].id) {
            return Err(format!("duplicate {}", pair[0].label()).into());
        }
    }
    let mut geometry_count = 0;
    let mut nodes = BTreeMap::new();
    for element in &extract.elements {
        let valid = match element.kind {
            ElementKind::Node => element.point().is_some_and(Coordinate::valid),
            ElementKind::Way => {
                element.nodes.len() >= 2 && element.nodes.len() == element.geometry.len()
            }
            ElementKind::Relation => true,
        };
        let geometry = element
            .geometry
            .iter()
            .chain(element.members.iter().flat_map(|member| &member.geometry));
        geometry_count += geometry.clone().count() + element.members.len();
        if !valid || !geometry.into_iter().copied().all(Coordinate::valid) {
            return Err(format!(
                "malformed coordinates or node/geometry pairs in {}",
                element.label()
            )
            .into());
        }
        if geometry_count > MAX_GEOMETRY {
            return Err("Overpass geometry count exceeds the supported range".into());
        }
        for (id, point) in element
            .nodes
            .iter()
            .copied()
            .zip(element.geometry.iter().copied())
            .chain(element.point().map(|point| (element.id, point)))
        {
            if nodes
                .insert(id, point)
                .is_some_and(|previous| previous != point)
            {
                return Err(format!("inconsistent coordinates for node/{id}").into());
            }
        }
    }
    Ok(extract)
}
