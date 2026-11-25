use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read, Write};

use common::saveload::{Bincode, Encoder};
use geom::{vec2, BaseNode, Vec2, AABB};
use prototypes::Money;
use serde::{Deserialize, Serialize};

use crate::economy::progression::{milestone_population, ProgressionMode, ProgressionState};
use crate::economy::Government;
use crate::map::{BuildingID, IntersectionID, LotLayout, Map, ProjectKind, RoadID, WaterFootprint};
use crate::world_command::Rejection;
use crate::Simulation;

pub const LAND_TILE_SIZE: f32 = 512.0;
pub const MAX_NATIVE_TILED_SIDE: u16 = 32;
pub const MAX_NATIVE_LAND_SIDE: u16 = 50;
const MAX_RESERVES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LandTileId {
    pub x: u16,
    pub y: u16,
}

impl Ord for LandTileId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.y, self.x).cmp(&(other.y, other.x))
    }
}

impl PartialOrd for LandTileId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileExtent {
    pub width: u16,
    pub height: u16,
}

impl TileExtent {
    pub(super) fn area(self) -> Option<usize> {
        (self.width > 0
            && self.height > 0
            && self.width <= MAX_NATIVE_LAND_SIDE
            && self.height <= MAX_NATIVE_LAND_SIDE)
            .then(|| usize::from(self.width) * usize::from(self.height))
    }

    fn index(self, tile: LandTileId) -> Option<usize> {
        (tile.x < self.width && tile.y < self.height)
            .then(|| usize::from(tile.y) * usize::from(self.width) + usize::from(tile.x))
    }

    pub fn tiles(self) -> impl Iterator<Item = LandTileId> {
        (0..self.height).flat_map(move |y| (0..self.width).map(move |x| LandTileId { x, y }))
    }

    pub(super) fn bounds(self) -> AABB {
        AABB::new_ll_ur(
            Vec2::ZERO,
            vec2(self.width as f32, self.height as f32) * LAND_TILE_SIZE,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OwnedTileRect {
    pub origin: LandTileId,
    pub width: u16,
    pub height: u16,
}

impl OwnedTileRect {
    pub fn centered(extent: TileExtent) -> Self {
        Self {
            origin: LandTileId {
                x: extent.width.saturating_sub(2) / 2,
                y: extent.height.saturating_sub(2) / 2,
            },
            width: 2,
            height: 2,
        }
    }

    pub fn tiles(self) -> impl Iterator<Item = LandTileId> {
        let x = self.origin.x..self.origin.x.saturating_add(self.width);
        let y = self.origin.y..self.origin.y.saturating_add(self.height);
        y.flat_map(move |y| x.clone().map(move |x| LandTileId { x, y }))
    }

    pub(super) fn valid(self, extent: TileExtent) -> bool {
        self.width > 0
            && self.height > 0
            && self.origin.x > 0
            && self.origin.y > 0
            && u32::from(self.origin.x) + u32::from(self.width) < u32::from(extent.width)
            && u32::from(self.origin.y) + u32::from(self.height) < u32::from(extent.height)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LandMode {
    #[default]
    Unrestricted,
    Tiled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LandRecipe {
    #[default]
    Unrestricted,
    NativeFlatV1 {
        lot_layout: LotLayout,
    },
    AuthoredV1 {
        recipe_id: u64,
        extent: TileExtent,
        owned_rect: OwnedTileRect,
        lot_layout: LotLayout,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandSetupError {
    InvalidMode(LandMode),
    InvalidExtent(TileExtent),
    InvalidOwnedRect(OwnedTileRect),
    InvalidLayout(LotLayout),
    InvalidOpeningBudget(u32),
    MissingGateway(GatewayRole),
    InvalidGateway(GatewayRole),
    NoBuildableFrontage(LandTileId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GatewayRole {
    Highway,
    Rail,
    Freight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProtectedRoot {
    Road(GatewayRole, RoadID),
    Junction(GatewayRole, IntersectionID),
    Building(GatewayRole, BuildingID),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandDenial {
    OutsideMap,
    Unowned(LandTileId),
    ProtectedGateway,
    RequiresMutableAccess,
    ForbiddenOperation,
    WorkLimit,
}

#[derive(Clone, Debug, Default)]
pub enum LandOwnership {
    #[default]
    Unrestricted,
    Tiled(TiledLand),
}

#[derive(Clone, Debug)]
pub struct TiledLand {
    extent: TileExtent,
    bits: Vec<u8>,
    initial_owned: u16,
    reserves: BTreeSet<ProtectedRoot>,
    protected: BTreeSet<ProjectKind>,
}

impl LandOwnership {
    pub(crate) fn tiled(
        extent: TileExtent,
        rect: OwnedTileRect,
        reserves: BTreeSet<ProtectedRoot>,
    ) -> Result<Self, LandSetupError> {
        let area = extent.area().ok_or(LandSetupError::InvalidExtent(extent))?;
        if !rect.valid(extent) {
            return Err(LandSetupError::InvalidOwnedRect(rect));
        }
        let mut land = TiledLand {
            extent,
            bits: vec![0; area.div_ceil(8)],
            initial_owned: rect.width * rect.height,
            protected: protected_objects(&reserves)?,
            reserves,
        };
        for tile in rect.tiles() {
            land.set(tile);
        }
        Ok(Self::Tiled(land))
    }

    pub fn extent(&self) -> Option<TileExtent> {
        match self {
            Self::Unrestricted => None,
            Self::Tiled(land) => Some(land.extent),
        }
    }

    pub fn tile_at(&self, point: Vec2) -> Option<LandTileId> {
        let extent = self.extent()?;
        let limit = extent.bounds().ur;
        if !point.is_finite()
            || point.x < 0.0
            || point.y < 0.0
            || point.x >= limit.x
            || point.y >= limit.y
        {
            return None;
        }
        Some(LandTileId {
            x: (point.x / LAND_TILE_SIZE) as u16,
            y: (point.y / LAND_TILE_SIZE) as u16,
        })
    }

    pub fn tile_bounds(&self, tile: LandTileId) -> Option<AABB> {
        self.extent()?.index(tile)?;
        Some(tile_bounds(tile))
    }

    pub fn owns(&self, tile: LandTileId) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Tiled(land) => land.owns(tile),
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = LandTileId> {
        self.extent().into_iter().flat_map(TileExtent::tiles)
    }

    pub fn is_protected(&self, object: ProjectKind) -> bool {
        match self {
            Self::Unrestricted => false,
            Self::Tiled(land) => land.protected.contains(&object),
        }
    }

    fn has_reserve(&self, root: ProtectedRoot) -> bool {
        matches!(self, Self::Tiled(land) if land.reserves.contains(&root))
    }

    pub(crate) fn reserves(&self) -> impl Iterator<Item = ProtectedRoot> + '_ {
        let reserves = match self {
            Self::Unrestricted => None,
            Self::Tiled(land) => Some(&land.reserves),
        };
        reserves.into_iter().flatten().copied()
    }

    pub fn owned_count(&self) -> u16 {
        match self {
            Self::Unrestricted => 0,
            Self::Tiled(land) => land.owned_count(),
        }
    }

    pub(crate) fn revision(&self) -> usize {
        usize::from(self.owned_count())
    }

    pub(crate) fn is_tiled(&self) -> bool {
        matches!(self, Self::Tiled(_))
    }

    pub(super) fn check_point(&self, point: Vec2) -> Result<(), LandDenial> {
        if !self.is_tiled() {
            return Ok(());
        }
        let tile = self.tile_at(point).ok_or(LandDenial::OutsideMap)?;
        if !self.owns(tile) {
            return Err(LandDenial::Unowned(tile));
        }
        Ok(())
    }

    pub(crate) fn check_polygon(
        &self,
        points: &[Vec2],
        purchased: Option<LandTileId>,
    ) -> Result<(), LandDenial> {
        let Some(extent) = self.extent() else {
            return Ok(());
        };
        let footprint = WaterFootprint::new(points).ok_or(LandDenial::OutsideMap)?;
        let world = extent.bounds();
        if points.iter().any(|&point| !world.contains(point)) {
            return Err(LandDenial::OutsideMap);
        }
        for tile in candidate_tiles(extent, points) {
            let bounds = tile_bounds(tile);
            if Some(tile) != purchased
                && !self.owns(tile)
                && footprint.overlaps_square(bounds.ll, LAND_TILE_SIZE)
            {
                return Err(LandDenial::Unowned(tile));
            }
        }
        Ok(())
    }

    pub(crate) fn purchase(&mut self, tile: LandTileId) {
        let Self::Tiled(land) = self else {
            unreachable!("prepared tile purchase")
        };
        land.set(tile);
    }

    pub(crate) fn validate_start(&self, extent: TileExtent, rect: OwnedTileRect) -> bool {
        let Self::Tiled(land) = self else {
            return false;
        };
        let owned = land.owned_count();
        if land.extent != extent
            || !rect.valid(extent)
            || land.initial_owned != rect.width * rect.height
            || !rect.tiles().all(|tile| land.owns(tile))
        {
            return false;
        }
        let mut seen = BTreeSet::from([rect.origin]);
        let mut queue = VecDeque::from([rect.origin]);
        while let Some(tile) = queue.pop_front() {
            for neighbor in neighbors(tile) {
                if land.owns(neighbor) && seen.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }
        seen.len() == usize::from(owned)
    }
}

fn protected_objects(
    reserves: &BTreeSet<ProtectedRoot>,
) -> Result<BTreeSet<ProjectKind>, LandSetupError> {
    let mut objects = BTreeSet::new();
    for &root in reserves {
        let (role, object) = match root {
            ProtectedRoot::Road(role, id) => (role, ProjectKind::Road(id)),
            ProtectedRoot::Junction(role, id) => (role, ProjectKind::Intersection(id)),
            ProtectedRoot::Building(role, id) => (role, ProjectKind::Building(id)),
        };
        if reserves.len() > MAX_RESERVES || !objects.insert(object) {
            return Err(LandSetupError::InvalidGateway(role));
        }
    }
    Ok(objects)
}

impl TiledLand {
    fn owns(&self, tile: LandTileId) -> bool {
        self.extent
            .index(tile)
            .is_some_and(|index| self.bits[index / 8] & (1 << (index % 8)) != 0)
    }

    fn set(&mut self, tile: LandTileId) {
        let index = self.extent.index(tile).expect("validated tile");
        self.bits[index / 8] |= 1 << (index % 8);
    }

    fn owned_count(&self) -> u16 {
        self.bits.iter().map(|bits| bits.count_ones() as u16).sum()
    }
}

pub(super) fn tile_bounds(tile: LandTileId) -> AABB {
    let min = vec2(tile.x as f32, tile.y as f32) * LAND_TILE_SIZE;
    AABB::new_ll_ur(min, min + Vec2::splat(LAND_TILE_SIZE))
}

pub(super) fn candidate_tiles(
    extent: TileExtent,
    points: &[Vec2],
) -> impl Iterator<Item = LandTileId> {
    let (min, max) = points.iter().fold(
        (Vec2::splat(f32::INFINITY), Vec2::ZERO),
        |(min, max), &point| (min.min(point), max.max(point)),
    );
    let x =
        (min.x / LAND_TILE_SIZE) as u16..=((max.x / LAND_TILE_SIZE) as u16).min(extent.width - 1);
    let y =
        (min.y / LAND_TILE_SIZE) as u16..=((max.y / LAND_TILE_SIZE) as u16).min(extent.height - 1);
    y.flat_map(move |y| x.clone().map(move |x| LandTileId { x, y }))
}

fn neighbors(tile: LandTileId) -> impl Iterator<Item = LandTileId> {
    [
        tile.y.checked_sub(1).map(|y| LandTileId { y, ..tile }),
        tile.x.checked_sub(1).map(|x| LandTileId { x, ..tile }),
        tile.x.checked_add(1).map(|x| LandTileId { x, ..tile }),
        tile.y.checked_add(1).map(|y| LandTileId { y, ..tile }),
    ]
    .into_iter()
    .flatten()
}

pub(crate) fn encode_land(land: &LandOwnership) -> io::Result<Vec<u8>> {
    let mut bytes = vec![1, u8::from(land.is_tiled())];
    if let LandOwnership::Tiled(land) = land {
        Bincode::encode_writer(
            &(land.extent, land.initial_owned, land.reserves.len() as u16),
            &mut bytes,
        )?;
        bytes.write_all(&land.bits)?;
        for reserve in &land.reserves {
            Bincode::encode_writer(reserve, &mut bytes)?;
        }
    }
    Ok(bytes)
}

pub(crate) fn decode_land(mut bytes: &[u8]) -> io::Result<LandOwnership> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid land ownership");
    if bytes == [1, 0] {
        return Ok(LandOwnership::Unrestricted);
    }
    bytes = bytes.strip_prefix(&[1, 1]).ok_or_else(invalid)?;
    let (extent, initial_owned, count): (TileExtent, u16, u16) =
        Bincode::decode_reader(&mut bytes)?;
    let area = extent.area().ok_or_else(invalid)?;
    if initial_owned == 0 || usize::from(initial_owned) > area || usize::from(count) > MAX_RESERVES
    {
        return Err(invalid());
    }
    let mut bits = vec![0; area.div_ceil(8)];
    bytes.read_exact(&mut bits)?;
    if area % 8 != 0 && bits.last().copied().unwrap() >> (area % 8) != 0 {
        return Err(invalid());
    }
    let mut reserves = BTreeSet::new();
    for _ in 0..count {
        let root = Bincode::decode_reader(&mut bytes)?;
        if reserves.last().is_some_and(|last| *last >= root) || !reserves.insert(root) {
            return Err(invalid());
        }
    }
    let protected = protected_objects(&reserves).map_err(|_| invalid())?;
    let land = TiledLand {
        extent,
        bits,
        initial_owned,
        reserves,
        protected,
    };
    if !bytes.is_empty() || land.owned_count() < initial_owned {
        return Err(invalid());
    }
    Ok(LandOwnership::Tiled(land))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileStatus {
    Unavailable,
    Owned,
    NeedsAdjacent,
    Locked { milestone: u8, agents: u32 },
    InsufficientFunds,
    Buyable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileQuote {
    pub tile: LandTileId,
    pub price: Option<Money>,
    pub status: TileStatus,
    pub quota_remaining: Option<u16>,
    pub treasury_shortfall: Option<Money>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TilePurchased {
    pub tile: LandTileId,
    pub paid: Money,
}

pub fn quote_tile(sim: &Simulation, tile: LandTileId) -> Result<TileQuote, Rejection> {
    let map = sim.map();
    let LandOwnership::Tiled(land) = map.land_ownership() else {
        return Ok(unavailable_quote(tile));
    };
    if land.extent.index(tile).is_none() {
        return Ok(unavailable_quote(tile));
    }
    TileQuoteContext::new(sim, land).quote(land, tile)
}

/// Statuses in extent order, with purchase terms shared across the whole plan.
pub fn quote_tile_statuses(sim: &Simulation) -> Vec<TileStatus> {
    let map = sim.map();
    let LandOwnership::Tiled(land) = map.land_ownership() else {
        return Vec::new();
    };
    let context = TileQuoteContext::new(sim, land);
    land.extent
        .tiles()
        .map(|tile| {
            context
                .quote(land, tile)
                .map_or(TileStatus::Unavailable, |quote| quote.status)
        })
        .collect()
}

fn unavailable_quote(tile: LandTileId) -> TileQuote {
    TileQuote {
        tile,
        price: None,
        status: TileStatus::Unavailable,
        quota_remaining: None,
        treasury_shortfall: None,
    }
}

struct TileQuoteContext {
    quota_remaining: Option<u16>,
    tier: u8,
    price: Result<Money, Rejection>,
    shortfall: Result<Option<Money>, Rejection>,
}

impl TileQuoteContext {
    fn new(sim: &Simulation, land: &TiledLand) -> Self {
        let purchases = land.owned_count() - land.initial_owned;
        let progression = sim.read::<ProgressionState>();
        let tier = progression.highest_milestone;
        let quota_remaining = if progression.mode == ProgressionMode::Guided {
            [0u16, 2, 6]
                .get(usize::from(tier))
                .map(|limit| limit.saturating_sub(purchases))
        } else {
            None
        };
        let price = Money::new_bucks(500)
            .inner()
            .checked_mul(i64::from(purchases))
            .and_then(|increase| Money::new_bucks(10_000).inner().checked_add(increase))
            .map(Money::new_inner)
            .ok_or(Rejection::CostOverflow);
        let balance = sim.read::<Government>().money;
        let shortfall = price.and_then(|price| {
            if balance < price {
                price
                    .inner()
                    .checked_sub(balance.inner())
                    .map(|difference| Some(Money::new_inner(difference)))
                    .ok_or(Rejection::CostOverflow)
            } else {
                Ok(None)
            }
        });
        Self {
            quota_remaining,
            tier,
            price,
            shortfall,
        }
    }

    fn quote(&self, land: &TiledLand, tile: LandTileId) -> Result<TileQuote, Rejection> {
        let mut quote = unavailable_quote(tile);
        quote.quota_remaining = self.quota_remaining;
        if land.owns(tile) {
            quote.status = TileStatus::Owned;
            return Ok(quote);
        }
        quote.price = Some(self.price?);
        quote.status = if !neighbors(tile).any(|neighbor| land.owns(neighbor)) {
            TileStatus::NeedsAdjacent
        } else if self.quota_remaining == Some(0) {
            let milestone = self.tier + 1;
            TileStatus::Locked {
                milestone,
                agents: milestone_population(milestone).unwrap(),
            }
        } else if let Some(shortfall) = self.shortfall? {
            quote.treasury_shortfall = Some(shortfall);
            TileStatus::InsufficientFunds
        } else {
            TileStatus::Buyable
        };
        Ok(quote)
    }
}

pub(crate) fn prepare_purchase(
    sim: &Simulation,
    tile: LandTileId,
) -> Result<TilePurchased, Rejection> {
    let quote = quote_tile(sim, tile)?;
    let paid = match quote.status {
        TileStatus::Unavailable => return Err(Rejection::TileUnavailable),
        TileStatus::Owned => return Err(Rejection::TileAlreadyOwned),
        TileStatus::NeedsAdjacent => return Err(Rejection::TileNotAdjacent),
        TileStatus::Locked { milestone, .. } => return Err(Rejection::Locked { milestone }),
        TileStatus::InsufficientFunds => return Err(Rejection::InsufficientFunds),
        TileStatus::Buyable => quote.price.expect("buyable tile has a price"),
    };
    Ok(TilePurchased { tile, paid })
}

pub(super) fn base_node_support(node: BaseNode, map: &Map) -> AABB {
    let pitch = super::terrain::CELL_SIZE;
    let point = vec2(node.0 as f32, node.1 as f32) * pitch;
    AABB::new_ll_ur(
        (point - Vec2::splat(pitch)).max(Vec2::ZERO),
        (point + Vec2::splat(pitch)).min(map.environment().bounds().ur),
    )
}

pub(crate) fn validate_loaded_land(sim: &Simulation) -> Result<(), String> {
    use super::PreparedEffects;
    let map = sim.map();
    let options = *sim.read::<crate::SimulationOptions>();
    let land = map.land_ownership();
    let (width, height) = map.environment().size();
    let terrain = TileExtent { width, height };
    let (extent, rect, layout) = match (options.land_mode, options.land_recipe) {
        (LandMode::Unrestricted, LandRecipe::Unrestricted) if !land.is_tiled() => return Ok(()),
        (LandMode::Tiled, LandRecipe::NativeFlatV1 { lot_layout })
            if options.valid_for_bootstrap() =>
        {
            let side = options.terrain_size;
            let extent = TileExtent {
                width: side,
                height: side,
            };
            (extent, OwnedTileRect::centered(extent), lot_layout)
        }
        (
            LandMode::Tiled,
            LandRecipe::AuthoredV1 {
                recipe_id,
                extent,
                owned_rect,
                lot_layout,
            },
        ) if recipe_id != 0
            && !options.save_replay
            && !sim.read::<crate::Replay>().enabled
            && options.preset == crate::MapPreset::Flat
            && options.terrain_size == extent.width =>
        {
            (extent, owned_rect, lot_layout)
        }
        _ => return Err("Land mode and recipe do not match".into()),
    };
    if extent != terrain || layout != map.lot_layout() || !land.validate_start(extent, rect) {
        return Err(
            "Land ownership, terrain extent or lot layout does not match the recipe".into(),
        );
    }

    let mut roles = BTreeSet::new();
    for reserve in land.reserves() {
        let role = match reserve {
            ProtectedRoot::Road(role, id) => {
                let Some(road) = map.get(id) else {
                    return Err("Missing protected road".into());
                };
                let right_kind = match role {
                    GatewayRole::Rail => road.lanes_iter().any(|(_, kind)| kind.is_rail()),
                    GatewayRole::Highway => road
                        .lanes_iter()
                        .any(|(_, kind)| kind == super::LaneKind::Driving),
                    GatewayRole::Freight => false,
                };
                if !right_kind
                    || !land.is_protected(road.src.into())
                    || !land.is_protected(road.dst.into())
                    || road
                        .connected_buildings
                        .iter()
                        .any(|&id| !land.is_protected(id.into()))
                {
                    return Err("Invalid protected road or attachment".into());
                }
                role
            }
            ProtectedRoot::Junction(role, id) => {
                let Some(junction) = map.get(id) else {
                    return Err("Missing protected junction".into());
                };
                if !extent.bounds().contains(junction.pos.xy())
                    || !junction
                        .roads
                        .iter()
                        .any(|&id| land.has_reserve(ProtectedRoot::Road(role, id)))
                {
                    return Err("Invalid protected junction".into());
                }
                role
            }
            ProtectedRoot::Building(role, id) => {
                let Some(building) = map.get(id) else {
                    return Err("Missing protected facility".into());
                };
                if role != GatewayRole::Freight
                    || !matches!(
                        building.kind,
                        super::BuildingKind::ExternalTrading
                            | super::BuildingKind::RailFreightStation(_)
                    )
                    || building
                        .obb
                        .corners
                        .iter()
                        .any(|&point| !extent.bounds().contains(point))
                {
                    return Err("Invalid protected facility".into());
                }
                role
            }
        };
        roles.insert(role);
    }
    for role in [
        GatewayRole::Highway,
        GatewayRole::Rail,
        GatewayRole::Freight,
    ] {
        if !roles.contains(&role) {
            return Err(format!("Missing protected gateway {role:?}"));
        }
    }
    let check = |effects| {
        map.authorize_land_effects(effects)
            .map_err(|error| format!("Invalid saved land effect: {error:?}"))
    };
    for id in map
        .roads()
        .keys()
        .filter(|id| !land.is_protected((*id).into()))
    {
        check(PreparedEffects::Road(id))?;
    }
    for id in map
        .intersections()
        .keys()
        .filter(|id| !land.is_protected((*id).into()))
    {
        check(PreparedEffects::Junction(id))?;
        let outline = map
            .land_junction_outline(id)
            .map_err(|error| error.to_string())?;
        if outline.len() >= 3 {
            map.authorize_land_effects(PreparedEffects::Footprint(outline.as_slice()))
                .map_err(|error| format!("Invalid saved junction footprint: {error:?}"))?;
        }
    }
    for id in map
        .buildings()
        .keys()
        .filter(|id| !land.is_protected((*id).into()))
    {
        check(PreparedEffects::Building(id))?;
    }
    for id in map.lots().keys() {
        check(PreparedEffects::Lot(id))?;
    }
    for &cell in sim.read::<super::ZoneGrid>().cells().keys() {
        let center = super::zonegrid::cell_center(cell);
        let corners = super::land_site::rect_corners(AABB::centered(
            center,
            Vec2::splat(super::ZONE_CELL_SIZE),
        ));
        map.authorize_land_effects(PreparedEffects::Footprint(&corners))
            .map_err(|error| format!("Invalid saved zone: {error:?}"))?;
    }
    for origin in map.lot_origins.values() {
        land.check_polygon(&origin.shape.corners, None)
            .map_err(|error| format!("Invalid saved lot origin: {error:?}"))?;
    }
    crate::map_dynamic::transit::validate_saved_land_stops(sim)?;
    Ok(())
}
