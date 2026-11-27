mod plan;
#[cfg(test)]
mod tests;

use std::rc::Rc;

use geom::{vec2, LinearColor};
use goryak::{
    button_primary, button_secondary, error, mincolumn, minrow, monospace, textc, titlec, Window,
};
use prototypes::Money;
use simulation::economy::{
    population::PopulationScale,
    progression::{milestone_population, ProgressionMode, ProgressionState},
    Government,
};
use simulation::map::{quote_tile, quote_tile_statuses, LandTileId, TileQuote, TileStatus};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand, WorldCommands};
use simulation::Simulation;
use yakui::{colored_box_container, constrained, widgets::Pad, Constraints, Vec2};

use crate::gui::{close_tool, ErrorTooltip, GuiState, PlacementPreview, Tool};
use crate::inputmap::{Bindings, InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::session::{self, SessionState};
use crate::sim_thread::{AppliedBatch, BatchId};
use crate::uiworld::{TilePurchaseReceipt, UiWorld};

use self::plan::{MapGeometry, Plan, PlanWidget, APPROACH, PROTECTED, RAIL, ROAD, WATER};

const ADJACENT_LOCK: TileStatus = TileStatus::Locked {
    milestone: 0,
    agents: 0,
};

#[derive(Default)]
pub(crate) struct LandTilesUi {
    generation: u64,
    selected: Option<LandTileId>,
    confirmation: Option<Money>,
    pending: Option<PendingPurchase>,
    receipt: Option<TilePurchaseReceipt>,
    busy: bool,
    shortcut_pending: bool,
    approaches: Option<SelectedApproaches>,
    geometry: Option<((u64, u64, u64, u64), Rc<MapGeometry>)>,
    statuses: Option<((usize, ProgressionMode, u8, Money), Rc<Vec<TileStatus>>)>,
}

struct SelectedApproaches {
    tile: LandTileId,
    revision: (u64, u64, u64, u64, usize),
    command_sequence: u64,
    site_count: Option<usize>,
    markers: Rc<Vec<Vec2>>,
}

#[derive(Clone, Copy)]
struct PendingPurchase {
    generation: u64,
    after_sequence: u64,
    command_index: usize,
    tile: LandTileId,
    batch: Option<BatchId>,
}

impl PendingPurchase {
    fn matches(&self, receipt: &TilePurchaseReceipt, batch: Option<BatchId>) -> bool {
        self.batch == batch
            && receipt.generation == self.generation
            && receipt.batch_sequence > self.after_sequence
            && receipt.tile == self.tile
            && (batch.is_none() || receipt.command_index == self.command_index)
    }

    fn command_matches(&self, commands: &WorldCommands) -> bool {
        matches!(
            commands.as_ref().get(self.command_index),
            Some(WorldCommand::BuyTile { tile }) if *tile == self.tile
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LandTilesScriptState {
    pub selected: Option<LandTileId>,
    pub quote: Option<String>,
    pub confirming: bool,
    pub pending: bool,
    pub receipt: Option<Result<i64, Rejection>>,
}

impl LandTilesUi {
    fn sync_generation(&mut self, generation: u64) {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Default::default()
            };
        }
    }

    pub(crate) fn script_state(&self, sim: &Simulation) -> LandTilesScriptState {
        let quote = self
            .selected
            .and_then(|tile| quote_tile(sim, tile).ok())
            .map(|quote| {
                format!(
                    "{:?} / {}",
                    quote.status,
                    quote.price.map_or(0, |price| price.bucks())
                )
            });
        LandTilesScriptState {
            selected: self.selected,
            quote,
            confirming: self.confirmation.is_some(),
            pending: self.pending.is_some(),
            receipt: self
                .receipt
                .map(|receipt| receipt.result.map(|purchase| purchase.paid.bucks())),
        }
    }

    pub(crate) fn confirm_script_purchase(
        &mut self,
        uiw: &UiWorld,
        sim: &Simulation,
    ) -> Result<(), String> {
        let (Some(tile), Some(price)) = (self.selected, self.confirmation) else {
            return Err("no Land Tiles purchase to confirm".into());
        };
        if self.pending.is_some() {
            return Err("a Land Tiles purchase is already pending".into());
        }
        self.submit(uiw, sim, tile, price);
        self.pending
            .is_some()
            .then_some(())
            .ok_or_else(|| "confirmed purchase did not enter pending state".into())
    }

    fn receive(&mut self, receipts: &[TilePurchaseReceipt], batch: Option<BatchId>) {
        let Some(pending) = self.pending else {
            return;
        };
        if let Some(receipt) = receipts
            .iter()
            .find(|receipt| pending.matches(receipt, batch))
        {
            self.pending = None;
            self.receipt = Some(*receipt);
        }
    }

    fn select(&mut self, tile: LandTileId) {
        if self.selected != Some(tile) {
            self.selected = Some(tile);
            self.approaches = None;
            self.confirmation = None;
        }
    }

    fn submit(&mut self, uiw: &UiWorld, sim: &Simulation, tile: LandTileId, price: Money) {
        if self.pending.is_some()
            || !crate::network::is_singleplayer(uiw)
            || !uiw.can_author()
            || !session::world_interaction(uiw)
        {
            return;
        }
        if self.confirmation != Some(price) {
            self.confirmation = Some(price);
            return;
        }

        let mut commands = uiw.commands();
        let command_index = commands.as_ref().len();
        commands.push(WorldCommand::BuyTile { tile });
        self.pending = Some(PendingPurchase {
            generation: self.generation,
            after_sequence: sim.read::<CommandOutcomes>().batch_seq(),
            command_index,
            tile,
            batch: None,
        });
        self.confirmation = None;
        self.receipt = None;
        self.busy = false;
    }
}

pub(crate) fn queued_in(uiw: &UiWorld, commands: &WorldCommands) -> bool {
    uiw.read::<LandTilesUi>()
        .pending
        .is_some_and(|pending| pending.batch.is_none() && pending.command_matches(commands))
}

pub(crate) fn bind_worker_batch(uiw: &UiWorld, id: BatchId) {
    let mut state = uiw.write::<LandTilesUi>();
    if let Some(pending) = &mut state.pending {
        if pending.batch.is_none() && pending.generation == id.generation {
            pending.batch = Some(id);
        }
    }
}

pub(crate) fn receive_worker_batch(uiw: &UiWorld, batch: &AppliedBatch) {
    uiw.write::<LandTilesUi>()
        .receive(&batch.tile_purchases, Some(batch.id));
}

#[cfg(feature = "multiplayer")]
pub(crate) fn receive_network(uiw: &UiWorld, receipts: &[TilePurchaseReceipt]) {
    uiw.write::<LandTilesUi>().receive(receipts, None);
}

pub(crate) fn reject_local_batch(uiw: &UiWorld, commands: &WorldCommands) {
    let mut state = uiw.write::<LandTilesUi>();
    if state
        .pending
        .is_some_and(|pending| pending.batch.is_none() && pending.command_matches(commands))
    {
        state.pending = None;
        state.busy = true;
    }
}

pub(crate) fn reset(uiw: &UiWorld, generation: u64) {
    *uiw.write::<LandTilesUi>() = LandTilesUi {
        generation,
        ..Default::default()
    };
    uiw.write::<GuiState>().windows.land_tiles_open = false;
}

pub(crate) fn toggle(uiw: &UiWorld) {
    let mut gui = uiw.write::<GuiState>();
    let opened = &mut gui.windows.land_tiles_open;
    *opened = !*opened;
    if *opened {
        open(uiw);
    } else {
        uiw.write::<LandTilesUi>().confirmation = None;
    }
}

pub(crate) fn open(uiw: &UiWorld) {
    close_tool(uiw);
    *uiw.write::<PlacementPreview>() = PlacementPreview::default();
    *uiw.write::<crate::gui::toolbox::transit::TransitTools>() = Default::default();
    *uiw.write::<crate::gui::zoneedit::ZoneEditState>() = Default::default();
    uiw.write::<InputMap>().reset_world_input();
}

pub(crate) fn toolbar(uiw: &UiWorld, has_tiles: bool) {
    if !has_tiles {
        return;
    }
    let label = uiw
        .read::<Bindings>()
        .0
        .get(&InputAction::OpenLandTiles)
        .filter(|binding| !binding.0.is_empty())
        .map_or_else(
            || "Land Tiles".to_owned(),
            |binding| format!("Land Tiles ({binding})"),
        );
    if goryak::tooltip(label, || button_secondary("Land Tiles").show().clicked) {
        toggle(uiw);
    }
}

pub(crate) fn latch_shortcut(uiw: &UiWorld, enabled: bool, pressed: bool) {
    let mut state = uiw.write::<LandTilesUi>();
    state.shortcut_pending = enabled && (state.shortcut_pending ^ pressed);
}

pub(crate) fn shortcut_pending(uiw: &UiWorld) -> bool {
    uiw.read::<LandTilesUi>().shortcut_pending
}

pub(crate) fn shortcut(uiw: &UiWorld, sim: &Simulation) -> bool {
    if !std::mem::take(&mut uiw.write::<LandTilesUi>().shortcut_pending)
        || sim.map().land_ownership().extent().is_none()
    {
        return false;
    }
    toggle(uiw);
    true
}

pub(crate) fn show_edit_error(uiw: &UiWorld, action: &'static str, reason: Rejection) {
    *uiw.write::<ErrorTooltip>() =
        ErrorTooltip::new(crate::gui::notifications::command_message(action, reason));
}

pub(crate) fn preview_edit(sim: &Simulation, uiw: &UiWorld, command: &WorldCommand) -> bool {
    match command.preview(sim) {
        Ok(_) => true,
        Err(reason) => {
            show_edit_error(uiw, command.kind(), reason);
            false
        }
    }
}

pub(crate) fn render(uiw: &UiWorld, sim: &Simulation, opened: &mut bool) {
    let generation = uiw.read::<SessionState>().generation;
    let mut state = uiw.write::<LandTilesUi>();
    state.sync_generation(generation);
    if !*opened {
        state.confirmation = None;
        return;
    }
    let map = sim.map();
    let land = map.land_ownership();
    let Some(extent) = land.extent() else {
        *opened = false;
        return;
    };
    let revision = map.road_preview_revision();
    let (identity, topology, water, terrain, _) = revision;
    let geometry_key = (identity, topology, water, terrain);
    let geometry = match &state.geometry {
        Some((key, geometry)) if *key == geometry_key => Rc::clone(geometry),
        _ => {
            let geometry = Rc::new(MapGeometry::build(&map, extent));
            state.geometry = Some((geometry_key, Rc::clone(&geometry)));
            geometry
        }
    };

    let treasury = sim.read::<Government>().money;
    let (mode, milestone) = {
        let progression = sim.read::<ProgressionState>();
        (progression.mode, progression.highest_milestone)
    };
    let owned = usize::from(land.owned_count());
    let key = (owned, mode, milestone, treasury);
    let sequence = sim.read::<CommandOutcomes>().batch_seq();
    if let Some(tile) = state.selected {
        if state.approaches.as_ref().is_none_or(|approaches| {
            approaches.tile != tile
                || approaches.revision != revision
                || approaches.command_sequence != sequence
        }) {
            let (site_count, markers) = match map.preview_land_approaches(tile) {
                Ok(sites) => {
                    let count = sites.len();
                    (Some(count), plan::approach_markers(sites))
                }
                Err(_) => (None, Vec::new()),
            };
            state.approaches = Some(SelectedApproaches {
                tile,
                revision,
                command_sequence: sequence,
                site_count,
                markers: Rc::new(markers),
            });
        }
    }
    drop(map);
    let statuses = match &state.statuses {
        Some((cached, statuses)) if *cached == key => Rc::clone(statuses),
        _ => {
            let statuses = Rc::new(quote_tile_statuses(sim));
            state.statuses = Some((key, Rc::clone(&statuses)));
            statuses
        }
    };
    Window {
        title: "Land Tiles".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 9.0,
    }
    .show(|| {
        let color = goryak::on_primary_container();
        if !crate::network::is_singleplayer(uiw) {
            textc(color, "Land tile purchases are unavailable in multiplayer.");
        }
        minrow(13.0, || {
            swatch(color, TileStatus::Owned, "Owned");
            swatch(color, TileStatus::Buyable, "Available");
            swatch(color, ADJACENT_LOCK, "Milestone locked");
            swatch(color, TileStatus::InsufficientFunds, "Unaffordable");
            swatch(color, TileStatus::NeedsAdjacent, "Requires adjacency");
        });
        minrow(14.0, || {
            let plan = Plan {
                geometry: Rc::clone(&geometry),
                statuses,
                selected: state.selected,
                approaches: state
                    .approaches
                    .as_ref()
                    .map(|approaches| Rc::clone(&approaches.markers)),
            };
            if let Some(tile) = yakui::util::widget::<PlanWidget>(plan).into_inner() {
                state.select(tile);
            }
            constrained(
                Constraints {
                    min: Vec2::new(286.0, 0.0),
                    max: Vec2::new(286.0, f32::INFINITY),
                },
                || {
                    mincolumn(10.0, || {
                        textc(color, format!("Treasury  {treasury}"));
                        textc(
                            color,
                            format!(
                                "{} of {} tiles owned",
                                owned,
                                usize::from(extent.width) * usize::from(extent.height)
                            ),
                        );
                        yakui::divider(color.with_alpha(0.3), 4.0, 1.0);
                        if let Some(tile) = state.selected {
                            titlec(color, tile_name(tile));
                            match quote_tile(sim, tile) {
                                Ok(quote) => tile_details(uiw, sim, &mut state, quote, color),
                                Err(reason) => {
                                    textc(error(), purchase_error(reason, sim.population_scale()));
                                }
                            };
                        } else {
                            textc(color, "Select a tile to inspect its price and access.");
                        }
                        if let Some(pending) = state.pending {
                            textc(
                                color,
                                format!("Waiting for purchase of {}…", tile_name(pending.tile)),
                            );
                        }
                        if let Some(receipt) = state.receipt {
                            let tile = tile_name(receipt.tile);
                            match receipt.result {
                                Ok(purchase) => {
                                    textc(color, format!("{tile} purchased for {}.", purchase.paid))
                                }
                                Err(reason) => textc(
                                    error(),
                                    format!(
                                        "{tile} · {}",
                                        purchase_error(reason, sim.population_scale())
                                    ),
                                ),
                            };
                        }
                        if state.busy {
                            textc(
                                error(),
                                "The batch limit was reached. Try the purchase again.",
                            );
                        }
                    });
                },
            );
        });
        minrow(14.0, || {
            legend(ROAD, color, "Road");
            legend(RAIL, color, "Rail");
            legend(PROTECTED, color, "Protected corridor");
            legend(APPROACH, color, "Sidewalk approach");
            legend(WATER, color, "Water");
        });
    });
}

fn tile_details(
    uiw: &UiWorld,
    sim: &Simulation,
    state: &mut LandTilesUi,
    quote: TileQuote,
    color: yakui::Color,
) {
    let status = match quote.status {
        TileStatus::Unavailable => "Unavailable".to_owned(),
        TileStatus::Owned => "Owned".to_owned(),
        TileStatus::NeedsAdjacent => "Buy a neighboring tile first".to_owned(),
        TileStatus::Locked { milestone, agents } => {
            format!(
                "Locked until {}",
                milestone_label(milestone, sim.population_scale().persons(agents as usize))
            )
        }
        TileStatus::InsufficientFunds => "Insufficient funds".to_owned(),
        TileStatus::Buyable => "Available to buy".to_owned(),
    };
    textc(color, status);
    if let Some(price) = quote.price {
        textc(color, format!("Price  {price}"));
    }
    match quote.quota_remaining {
        Some(remaining) => textc(color, format!("Purchases remaining  {remaining}")),
        None => textc(color, "Purchases remaining  Unlimited"),
    };
    if let Some(shortfall) = quote.treasury_shortfall {
        textc(color, format!("Additional funds needed  {shortfall}"));
    }
    if let Some(approaches) = state
        .approaches
        .as_ref()
        .filter(|approaches| approaches.tile == quote.tile)
    {
        let sites = approaches
            .site_count
            .map_or_else(|| "Unavailable".to_owned(), |count| count.to_string());
        textc(color, format!("Sidewalk approaches  {sites}"));
    }

    let price = quote.price.filter(|_| quote.status == TileStatus::Buyable);
    if state.confirmation.is_some() && state.confirmation != price {
        state.confirmation = None;
        textc(error(), "The quote changed. Review the current terms.");
    }
    if let Some(price) =
        price.filter(|_| state.pending.is_none() && crate::network::is_singleplayer(uiw))
    {
        if uiw.can_author() {
            let button = if state.confirmation == Some(price) {
                format!("Confirm purchase · {price}")
            } else {
                "Buy tile".to_owned()
            };
            minrow(7.0, || {
                if button_primary(button).show().clicked {
                    state.submit(uiw, sim, quote.tile, price);
                }
                if state.confirmation.is_some() && button_secondary("Cancel").show().clicked {
                    state.confirmation = None;
                }
            });
        } else {
            textc(color, "Simulation busy. Wait for pending edits.");
        }
    }
}

fn tile_name(tile: LandTileId) -> String {
    format!("Tile ({}, {})", tile.x, tile.y)
}

fn milestone_label(milestone: u8, population: u64) -> String {
    format!("Milestone {milestone} · {population} residents")
}

fn purchase_error(reason: Rejection, scale: PopulationScale) -> String {
    if let Rejection::Locked { milestone } = reason {
        if let Some(population) = milestone_population(milestone) {
            return format!(
                "Locked until {}",
                milestone_label(milestone, scale.persons(population as usize))
            );
        }
    }
    crate::gui::notifications::command_message("BuyTile", reason).into_owned()
}

fn swatch(text: yakui::Color, status: TileStatus, label: &'static str) {
    legend(plan::status_color(status), text, label);
}

fn legend(fill: yakui::Color, color: yakui::Color, label: &'static str) {
    minrow(5.0, || {
        colored_box_container(fill, || {
            goryak::fixed_spacer((10.0, 10.0));
        });
        monospace(color, label);
    });
}

pub(crate) fn opening_view(map: &simulation::map::Map) -> Option<(geom::Vec3, f32)> {
    let land = map.land_ownership();
    let mut owned = land
        .tiles()
        .filter(|&tile| land.owns(tile))
        .filter_map(|tile| land.tile_bounds(tile));
    let first = owned.next()?;
    let (min, max) = owned.fold((first.ll, first.ur), |(min, max), tile| {
        (min.min(tile.ll), max.max(tile.ur))
    });
    let center = (min + max) * 0.5;
    let env = map.environment();
    let height = env
        .water_surface_elevation(center)
        .or_else(|| env.height(center))
        .unwrap_or_default();
    let span = (max - min).x.max((max - min).y);
    Some((center.z(height), span))
}

/// Point-level feedback is shared by the edit tools. Footprint validation still
/// happens in each tool's prepared preview and again in simulation.
pub(crate) fn show_unowned_hover(sim: &Simulation, uiw: &UiWorld) {
    if *uiw.read::<Tool>() == Tool::Hand {
        return;
    }
    let input = uiw.read::<InputMap>();
    if goryak::pointer_over_ui(input.screen.x, input.screen.y)
        || engine::pointer_over_egui(input.screen)
    {
        return;
    }
    let map = sim.map();
    let land = map.land_ownership();
    let Some(tile) = input
        .unprojected
        .and_then(|point| land.tile_at(point.xy()))
        .filter(|&tile| !land.owns(tile))
    else {
        return;
    };
    let mut tooltip = uiw.write::<ErrorTooltip>();
    if tooltip.msg.is_none() {
        *tooltip = ErrorTooltip::new(format!(
            "{} is not owned. Open Land Tiles to expand.",
            tile_name(tile)
        ));
    }
    let Some(bounds) = land.tile_bounds(tile) else {
        return;
    };
    let env = map.environment();
    let corners = [
        bounds.ll,
        vec2(bounds.ur.x, bounds.ll.y),
        bounds.ur,
        vec2(bounds.ll.x, bounds.ur.y),
    ];
    let outline = corners
        .into_iter()
        .zip(corners.into_iter().cycle().skip(1))
        .flat_map(|(from, to)| (0..8).map(move |i| from + (to - from) * (i as f32 / 8.0)))
        .map(|point| {
            point.z(env
                .water_surface_elevation(point)
                .or_else(|| env.height(point))
                .unwrap_or_default()
                + 1.5)
        })
        .collect::<Vec<_>>();
    uiw.write::<ImmediateDraw>()
        .polyline(outline, 3.0, true)
        .color(LinearColor::new(1.0, 0.44, 0.28, 1.0));
}
