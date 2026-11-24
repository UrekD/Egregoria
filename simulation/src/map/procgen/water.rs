use crate::map::terrain::CELL_SIZE;
use crate::map::{
    Environment, WaterEditError, MAX_WATER_BASE_WRITES, MAX_WATER_CHUNKS_PER_AXIS,
    TERRAIN_CHUNK_RESOLUTION,
};
use crate::utils::rand_provider::RandProvider;
use crate::{MapPreset, NewGameOptions};
use geom::{pack_base_height_at_most, vec2, BaseNodePatch, Polygon};

/// Build the terrain for Waters before any starter infrastructure is placed.
/// The fixed starter rail, market and station at the minimum size fit west
/// of the water features, with dry land on each river bank.
pub fn generate_waters(w: u16, h: u16, seed: u64) -> Result<Environment, WaterEditError> {
    if w > MAX_WATER_CHUNKS_PER_AXIS || h > MAX_WATER_CHUNKS_PER_AXIS {
        return Err(WaterEditError::Limit);
    }
    if w < NewGameOptions::MIN_TERRAIN_CHUNKS || h < NewGameOptions::MIN_TERRAIN_CHUNKS {
        return Err(WaterEditError::OutOfBounds);
    }

    const STARTER_RESERVE_EAST: f32 = 5120.0;
    const SEA_LEVEL_DM: i16 = 0;
    let mut rng = RandProvider::new(seed);
    let width_nodes = usize::from(w) * TERRAIN_CHUNK_RESOLUTION;
    let height_nodes = usize::from(h) * TERRAIN_CHUNK_RESOLUTION;
    let width = width_nodes as f32 * CELL_SIZE;
    let height = height_nodes as f32 * CELL_SIZE;
    let coast_nodes = 15 + (rng.next_f32() * 6.0) as usize;
    let mut env = Environment::generate(w, h, MapPreset::Flat, seed, 0.0);

    let mut coast = BaseNodePatch::new(MAX_WATER_BASE_WRITES);
    let coast_bed = pack_base_height_at_most(-8.0).unwrap();
    for x in width_nodes - coast_nodes..width_nodes {
        for y in 0..height_nodes {
            coast.stage(&env.heightmap, (x, y), coast_bed)?;
        }
    }
    let prepared = env.prepare_water_base_patch(coast)?;
    env.commit_prepared_water(prepared)?;
    env.set_sea_level(Some(SEA_LEVEL_DM))?;

    let river_start = (width * 0.66).max(STARTER_RESERVE_EAST + 256.0);
    let river_end = width - 96.0;
    let span = river_end - river_start;
    let y = height * (0.42 + rng.next_f32() * 0.07);
    let bend = 144.0 + rng.next_f32() * 96.0;
    let width_m = 80.0 + rng.next_f32() * 16.0;
    env.carve_river_at_level(
        &[
            vec2(river_start, y + bend),
            vec2(river_start + span * 0.32, y - bend),
            vec2(river_start + span * 0.67, y + bend * 0.5),
            vec2(river_end, y),
        ],
        width_m,
        5.0,
        Some(SEA_LEVEL_DM),
    )?;

    let center = vec2(
        width * (0.77 + rng.next_f32() * 0.045),
        height * (0.68 + rng.next_f32() * 0.06),
    );
    let r = 160.0 + rng.next_f32() * 72.0;
    let basin = Polygon(
        [
            vec2(-1.0, -0.21),
            vec2(-0.61, -0.81),
            vec2(0.27, -0.95),
            vec2(0.91, -0.38),
            vec2(1.0, 0.36),
            vec2(0.40, 0.90),
            vec2(-0.66, 0.67),
        ]
        .into_iter()
        .map(|p| center + p * r)
        .collect(),
    );
    env.fill_lake(&basin, 60, 4.0)?;

    // Remove generated trees using the same half-open tile authority as queries.
    env.remove_wet_generated_trees();
    Ok(env)
}

#[cfg(test)]
mod water_w1_tests {
    use super::*;
    use crate::map::{RiverBank, WaterKind};
    use common::saveload::{Bincode, Encoder};

    #[test]
    fn w1_water_seeded_layout_preserves_the_starter_reserve() {
        for size in [16, 50, 80] {
            for seed in [0, 1, 77] {
                let env = generate_waters(size, size, seed).unwrap();
                let width = f32::from(size) * 512.0;
                let center_y = width * 0.4;
                assert_eq!(
                    env.water_body_kind(vec2(width - 2.0, center_y)),
                    Some(WaterKind::Sea)
                );
                let starter = Polygon::centered_rect(vec2(2560.0, 3300.0), 5120.0, 6600.0);
                assert!(!env.intersects_water(&starter));
                let mut bodies = env.freshwater_bodies();
                let (body, kind, level) = bodies.next().unwrap();
                assert_eq!((kind, level), (WaterKind::River, 0));
                assert_eq!(bodies.next().unwrap().1, WaterKind::Lake);
                assert!(bodies.next().is_none());
                assert!(env
                    .river_bank_near(body, 600, RiverBank::Left, 32)
                    .is_some());
                assert!(env
                    .river_bank_near(body, 600, RiverBank::Right, 32)
                    .is_some());
            }
        }
        let a = generate_waters(16, 16, 77).unwrap();
        let b = generate_waters(16, 16, 77).unwrap();
        assert_eq!(Bincode::encode(&a).unwrap(), Bincode::encode(&b).unwrap());
        assert!(matches!(
            generate_waters(15, 16, 0),
            Err(WaterEditError::OutOfBounds)
        ));
    }
}
