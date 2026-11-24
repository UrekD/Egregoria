//! Bounded canonical water records. Both encodings use ordered sequences;
//! binary lengths are checked before allocating or reading sequence entries.

use super::water::{
    empty_fresh_chunk, packed_bed_limit, valid_river_width, valid_water_depth, BodyShape,
    FreshBody, FreshCell, WaterBasin, WaterBodyId, WaterEdit, WaterState, WaterStroke,
    MAX_FRESHWATER_BODIES, MAX_WATER_CHUNKS, MAX_WATER_EDIT_BYTES, MAX_WATER_RIVER_VALIDATION_WORK,
    MAX_WATER_RIVER_VERTICES, MAX_WATER_VERTICES, MIN_DEPTH, TILES_PER_CHUNK,
};
use super::TerrainChunkID;
use common::saveload::Encoder;
use geom::Vec2;
use serde::de::{DeserializeSeed, Error, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::marker::PhantomData;

impl WaterEdit {
    /// Decode a standalone, length-delimited edit. Embedded edits are bounded
    /// by their Serde field and sequence visitors.
    pub fn decode_bounded<E: Encoder>(payload: &[u8]) -> io::Result<Self> {
        if payload.len() > MAX_WATER_EDIT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "water edit exceeds 8 KiB",
            ));
        }
        E::decode(payload)
    }
}

struct RejectExtra(&'static str);

impl<'de> DeserializeSeed<'de> for RejectExtra {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
        Err(Error::custom(self.0))
    }
}

struct BoundedSequence<T, const LIMIT: usize>(Vec<T>);

impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for BoundedSequence<T, LIMIT> {
    type Value = Self;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a sequence with at most {LIMIT} water records")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<Self, A::Error> {
        if let Some(len) = seq.size_hint().filter(|&len| len > LIMIT) {
            return Err(Error::invalid_length(len, &self));
        }
        while self.0.len() < LIMIT {
            let Some(value) = seq.next_element()? else {
                return Ok(self);
            };
            self.0.push(value);
        }
        seq.next_element_seed(RejectExtra("water record limit exceeded"))?;
        Ok(self)
    }
}

impl<'de, T: Deserialize<'de>, const LIMIT: usize> Deserialize<'de> for BoundedSequence<T, LIMIT> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(Self(Vec::new()))
    }
}

impl<'de> Deserialize<'de> for WaterStroke {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let points = BoundedSequence::<Vec2, MAX_WATER_VERTICES>::deserialize(deserializer)?.0;
        Self::new(&points).map_err(Error::custom)
    }
}

impl<'de> Deserialize<'de> for WaterBasin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let points = BoundedSequence::<Vec2, MAX_WATER_VERTICES>::deserialize(deserializer)?.0;
        Self::new(&points).map_err(Error::custom)
    }
}

pub(super) fn river_width<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let value = f32::deserialize(deserializer)?;
    if !valid_river_width(value) {
        return Err(Error::custom(
            "river width must be between 32 and 512 metres",
        ));
    }
    Ok(value)
}

pub(super) fn depth<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let value = f32::deserialize(deserializer)?;
    if !valid_water_depth(f64::from(value)) {
        return Err(Error::custom(
            "water depth must be finite and at least 0.25 metres",
        ));
    }
    Ok(value)
}

pub(super) fn level<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i16, D::Error> {
    let value = i16::deserialize(deserializer)?;
    packed_bed_limit(value, MIN_DEPTH).map_err(Error::custom)?;
    Ok(value)
}

pub(super) fn optional_level<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i16>, D::Error> {
    let value = Option::<i16>::deserialize(deserializer)?;
    if let Some(dm) = value {
        packed_bed_limit(dm, MIN_DEPTH).map_err(Error::custom)?;
    }
    Ok(value)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBody<T> {
    id: WaterBodyId,
    level_dm: i16,
    shape: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChunk<T> {
    chunk: TerrainChunkID,
    cells: T,
}

struct BodiesRef<'a>(&'a BTreeMap<WaterBodyId, FreshBody>);

impl Serialize for BodiesRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for (&id, body) in self.0 {
            seq.serialize_element(&WireBody {
                id,
                level_dm: body.level_dm,
                shape: &body.shape,
            })?;
        }
        seq.end()
    }
}

struct ChunksRef<'a>(&'a WaterState);

impl Serialize for ChunksRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Cells<'a>(&'a [FreshCell; TILES_PER_CHUNK]);
        impl Serialize for Cells<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut seq = serializer
                    .serialize_seq(Some(self.0.iter().filter(|c| c.body != 0).count()))?;
                for (index, cell) in self.0.iter().enumerate().filter(|(_, c)| c.body != 0) {
                    seq.serialize_element(&(index as u16, cell.body, cell.reach))?;
                }
                seq.end()
            }
        }
        let mut seq = serializer.serialize_seq(Some(self.0.fresh.len()))?;
        for (&chunk, cells) in &self.0.fresh {
            seq.serialize_element(&WireChunk {
                chunk,
                cells: Cells(cells),
            })?;
        }
        seq.end()
    }
}

struct Bodies {
    table: BTreeMap<WaterBodyId, FreshBody>,
    vertices: usize,
    shape_work: usize,
}

impl<'de> Deserialize<'de> for Bodies {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(Records::<Self, MAX_FRESHWATER_BODIES>(PhantomData))
    }
}

type WireCells = BoundedSequence<(u16, u16, u16), TILES_PER_CHUNK>;
type Chunks = BTreeMap<TerrainChunkID, Box<[FreshCell; TILES_PER_CHUNK]>>;

struct DecodedChunks(Chunks);

impl<'de> Deserialize<'de> for DecodedChunks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(Records::<Self, MAX_WATER_CHUNKS>(PhantomData))
    }
}

trait RecordTable: Default {
    type Entry: for<'de> Deserialize<'de>;
    fn push<E: Error>(&mut self, entry: Self::Entry) -> Result<(), E>;
}

struct Records<T, const LIMIT: usize>(PhantomData<T>);

impl<'de, T: RecordTable, const LIMIT: usize> Visitor<'de> for Records<T, LIMIT> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at most {LIMIT} ordered water records")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        if let Some(len) = seq.size_hint().filter(|&len| len > LIMIT) {
            return Err(Error::invalid_length(len, &self));
        }
        let mut records = T::default();
        for _ in 0..LIMIT {
            let Some(record) = seq.next_element::<T::Entry>()? else {
                return Ok(records);
            };
            records.push(record)?;
        }
        seq.next_element_seed(RejectExtra("water record limit exceeded"))?;
        Ok(records)
    }
}

impl Default for Bodies {
    fn default() -> Self {
        Self {
            table: BTreeMap::new(),
            vertices: 0,
            shape_work: 0,
        }
    }
}

impl RecordTable for Bodies {
    type Entry = WireBody<BodyShape>;

    fn push<E: Error>(&mut self, record: Self::Entry) -> Result<(), E> {
        if record.id == WaterBodyId::SEA
            || self
                .table
                .last_key_value()
                .is_some_and(|(&last, _)| last >= record.id)
        {
            return Err(Error::custom("unordered or invalid freshwater body ID"));
        }
        if let BodyShape::River { stroke, .. } = &record.shape {
            self.shape_work += stroke.validation_work();
            if self.shape_work > MAX_WATER_RIVER_VALIDATION_WORK {
                return Err(Error::custom(
                    "total river shape validation work limit exceeded",
                ));
            }
        }
        let body = FreshBody::new(record.level_dm, record.shape);
        self.vertices += body.vertex_count();
        if self.vertices > MAX_WATER_RIVER_VERTICES {
            return Err(Error::custom("total river vertex limit exceeded"));
        }
        self.table.insert(record.id, body);
        Ok(())
    }
}

impl Default for DecodedChunks {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl RecordTable for DecodedChunks {
    type Entry = WireChunk<WireCells>;

    fn push<E: Error>(&mut self, record: Self::Entry) -> Result<(), E> {
        let chunk = record.chunk;
        if self
            .0
            .last_key_value()
            .is_some_and(|(&last, _)| last >= chunk)
        {
            return Err(Error::custom("unordered or duplicate freshwater chunk"));
        }
        if record.cells.0.is_empty() {
            return Err(Error::custom("empty freshwater chunk"));
        }
        let mut cells = empty_fresh_chunk();
        let mut last = None;
        for (index, body, reach) in record.cells.0 {
            if last.is_some_and(|prev| prev >= index)
                || index as usize >= TILES_PER_CHUNK
                || body == 0
            {
                return Err(Error::custom("unordered or invalid freshwater cell"));
            }
            last = Some(index);
            cells[index as usize] = FreshCell { body, reach };
        }
        self.0.insert(chunk, cells);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaterRecord<B, C> {
    strict: bool,
    revision: u64,
    sea_level_dm: Option<i16>,
    next_freshwater_id: u32,
    bodies: B,
    chunks: C,
}

impl Serialize for WaterState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WaterRecord {
            strict: self.strict,
            revision: self.revision,
            sea_level_dm: self.sea_level_dm,
            next_freshwater_id: self.next_freshwater_id,
            bodies: BodiesRef(&self.bodies),
            chunks: ChunksRef(self),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WaterState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let record = WaterRecord::<Bodies, DecodedChunks>::deserialize(deserializer)?;
        let mut state = Self::default();
        state.strict = record.strict;
        state.revision = record.revision;
        state.sea_level_dm = record.sea_level_dm;
        state.next_freshwater_id = record.next_freshwater_id;
        state.bodies = record.bodies.table;
        state.river_vertex_count = record.bodies.vertices;
        state.fresh = record.chunks.0;
        Ok(state)
    }
}

#[cfg(test)]
mod water_w1_tests {
    use super::*;
    use crate::map::{Environment, Heightmap};
    use common::saveload::{Bincode, JSON};
    use geom::{vec2, Polygon};

    #[test]
    fn w1_water_readable_edit_rejects_oversized_unknown_field() {
        let input = format!(
            r#"{{"SetSeaLevel":{{"level_dm":0,"extra":"{}"}}}}"#,
            "x".repeat(9 * 1024)
        );
        let mut reader = input.as_bytes();
        let result = JSON::decode_reader::<WaterEdit>(&mut reader);
        assert!(result.is_err(), "{result:?}");
        assert!(reader.len() > 8 * 1024, "read the oversized field");

        let edit = WaterEdit::SetSeaLevel { level_dm: Some(0) };
        let mut framed = JSON::encode(&edit).unwrap();
        assert_eq!(WaterEdit::decode_bounded::<JSON>(&framed).unwrap(), edit);
        framed.resize(MAX_WATER_EDIT_BYTES, b' ');
        assert_eq!(WaterEdit::decode_bounded::<JSON>(&framed).unwrap(), edit);
        framed.push(b' ');
        assert!(WaterEdit::decode_bounded::<JSON>(&framed).is_err());
    }

    #[test]
    fn w1_water_wire_rejects_advertised_lengths_and_invalid_vertices() {
        let huge = Bincode::encode(&u64::MAX).unwrap();
        assert!(Bincode::decode::<WaterStroke>(&huge)
            .unwrap_err()
            .to_string()
            .contains("at most 256"));
        assert!(Bincode::decode::<WaterBasin>(&huge).is_err());
        let huge_bodies = Bincode::encode(&(false, 0u64, None::<i16>, 1u32, u64::MAX)).unwrap();
        assert!(Bincode::decode::<WaterState>(&huge_bodies).is_err());
        let huge_chunks =
            Bincode::encode(&(false, 0u64, None::<i16>, 1u32, 0u64, u64::MAX)).unwrap();
        assert!(Bincode::decode::<WaterState>(&huge_chunks).is_err());

        let points = vec![Vec2::ZERO; MAX_WATER_VERTICES + 1];
        assert!(JSON::decode::<WaterBasin>(&JSON::encode(&points).unwrap()).is_err());
        let nonfinite = [Vec2::ZERO, vec2(f32::NAN, 5.0)];
        assert!(
            Bincode::decode::<WaterStroke>(&Bincode::encode(&nonfinite.to_vec()).unwrap()).is_err()
        );
        assert!(WaterStroke::new(&[
            vec2(0.0, 0.0),
            vec2(32.0, 32.0),
            vec2(0.0, 32.0),
            vec2(32.0, 0.0)
        ])
        .is_err());

        let edit = WaterEdit::CarveRiver {
            polyline: WaterStroke::new(&[Vec2::ZERO, vec2(32.0, 0.0)]).unwrap(),
            width_m: f32::INFINITY,
            depth_m: 2.0,
            level_dm: None,
        };
        assert!(Bincode::decode::<WaterEdit>(&Bincode::encode(&edit).unwrap()).is_err());
    }

    #[derive(Serialize)]
    struct EnvironmentRecord<'a, T> {
        h: &'a Heightmap,
        trees: Vec<()>,
        water: T,
    }

    fn reject_record(map: &Heightmap, record: impl Serialize) {
        let data = EnvironmentRecord {
            h: map,
            trees: Vec::new(),
            water: record,
        };
        assert!(Bincode::decode::<Environment>(&Bincode::encode(&data).unwrap()).is_err());
        assert!(JSON::decode::<Environment>(&JSON::encode(&data).unwrap()).is_err());
    }

    #[test]
    fn w1_water_wire_rejects_invalid_authority_in_both_formats() {
        let mut env = Environment::new(1, 1);
        // Use a uniform dry datum so an adjacent higher sea demonstrably floods.
        env.terrain_apply(env.bounds(), |_| 10.0);
        env.fill_lake(
            &Polygon::centered_rect(vec2(80.0, 80.0), 32.0, 32.0),
            0,
            2.0,
        )
        .unwrap();
        let body = || WireBody {
            id: WaterBodyId(1),
            level_dm: 0,
            shape: BodyShape::Lake,
        };
        let cells = || vec![(132u16, 1u16, 0u16), (133, 1, 0), (164, 1, 0), (165, 1, 0)];
        let chunk = || WireChunk {
            chunk: TerrainChunkID::new_i16(0, 0),
            cells: cells(),
        };
        let record = || WaterRecord {
            strict: true,
            revision: 1,
            sea_level_dm: None,
            next_freshwater_id: 2,
            bodies: vec![body()],
            chunks: vec![chunk()],
        };
        let decode_ok = EnvironmentRecord {
            h: &env.heightmap,
            trees: Vec::new(),
            water: record(),
        };
        assert!(Bincode::decode::<Environment>(&Bincode::encode(&decode_ok).unwrap()).is_ok());

        let mut bad = record();
        bad.bodies.push(body());
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks.push(chunk());
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].chunk = TerrainChunkID::new_i16(1, 0);
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].cells[1].0 = 132;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].cells[0].2 = 7;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].cells[0].1 = 2;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].cells[0].0 = 0;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.chunks[0].cells.remove(1);
        bad.chunks[0].cells.remove(1);
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.next_freshwater_id = 65_537;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.strict = false;
        reject_record(&env.heightmap, bad);
        let mut bad = record();
        bad.sea_level_dm = Some(110);
        reject_record(&env.heightmap, bad);
        reject_record(&Heightmap::new(81, 1), record());

        #[allow(dead_code)]
        #[derive(Serialize)]
        enum InvalidShape {
            Lake,
            River,
            Sea,
        }
        reject_record(
            &env.heightmap,
            WaterRecord {
                strict: true,
                revision: 1,
                sea_level_dm: None,
                next_freshwater_id: 2,
                bodies: vec![WireBody {
                    id: WaterBodyId(1),
                    level_dm: 0,
                    shape: InvalidShape::Sea,
                }],
                chunks: vec![chunk()],
            },
        );
    }
}
