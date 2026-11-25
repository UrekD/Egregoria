use crate::gui::zoning::zone_name;
use crate::uiworld::UiWorld;
use goryak::{
    fixed_spacer, mincolumn, minrow, monospace, on_secondary_container, selectable_label_primary,
    textc, titlec, VertScrollSize, Window,
};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::{
    Budget, BudgetCategory, Government, MAX_TAX_RATE_PERMILLE, TICKS_PER_BUDGET_DAY,
};
use simulation::map::ZoneKind;
use simulation::world_command::WorldCommand;
use simulation::Simulation;
use yakui::widgets::{List, Pad, Slider};
use yakui::{divider, use_state, MainAxisAlignment};

pub fn budget(uiworld: &UiWorld, sim: &Simulation, opened: &mut bool) {
    Window {
        title: "Budget".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 8.0,
    }
    .show(|| {
        let color = on_secondary_container();
        let current_day = elapsed_tick(sim).0.saturating_sub(1) / TICKS_PER_BUDGET_DAY;
        let budget = sim.read::<Budget>();
        let selection = use_state(|| None::<u64>);
        let day = selection
            .get()
            .filter(|day| budget.days().contains_key(day))
            .unwrap_or(current_day);
        let ledger = budget.days().get(&day);
        let net: i128 = ledger
            .into_iter()
            .flat_map(|entries| entries.values())
            .sum();
        let treasury = i128::from(sim.read::<Government>().money.inner());
        titlec(color, format!("Treasury  {}", money(treasury, false)));
        minrow(26.0, || {
            mincolumn(5.0, || {
                titlec(color, format!("Day {} ledger", day + 1));
                for (category, name) in [
                    (
                        BudgetCategory::StarterInfrastructure,
                        "Starter infrastructure",
                    ),
                    (BudgetCategory::Construction, "Construction"),
                    (BudgetCategory::LandPurchase, "Land purchases"),
                    (BudgetCategory::MilestoneGrant, "Milestone grants"),
                    (BudgetCategory::TradeImport, "Imports"),
                    (BudgetCategory::TradeExport, "Exports"),
                    (
                        BudgetCategory::Tax(ZoneKind::Residential),
                        "Residential tax",
                    ),
                    (BudgetCategory::Tax(ZoneKind::Commercial), "Commercial tax"),
                    (BudgetCategory::Tax(ZoneKind::Industrial), "Industrial tax"),
                    (BudgetCategory::Tax(ZoneKind::Office), "Office tax"),
                    (BudgetCategory::RoadUpkeep, "Road upkeep"),
                    (
                        BudgetCategory::SpecialBuildingUpkeep,
                        "Special building upkeep",
                    ),
                    (BudgetCategory::ServiceUpkeep, "Service upkeep"),
                    (BudgetCategory::TransitFares, "Transit fares"),
                    (BudgetCategory::TransitUpkeep, "Transit upkeep"),
                ] {
                    let amount = ledger
                        .and_then(|entries| entries.get(&category))
                        .copied()
                        .unwrap_or(0);
                    ledger_row(name, amount);
                }
                divider(color.with_alpha(0.3), 3.0, 1.0);
                ledger_row("Net", net);
            });
            mincolumn(3.0, || {
                titlec(color, "Tax rates");
                for zone in ZoneKind::TAXABLE {
                    let rate = budget.tax_rate(zone);
                    textc(
                        color,
                        format!("{}  {:.1}%", zone_name(zone), f32::from(rate) / 10.0),
                    );

                    if uiworld.can_author() {
                        let mut slider =
                            Slider::new(f64::from(rate), 0.0, f64::from(MAX_TAX_RATE_PERMILLE));
                        slider.step = Some(1.0);
                        if let Some(value) = slider.show().value {
                            uiworld.commands().push(WorldCommand::SetTaxRate {
                                zone,
                                rate_permille: value as u16,
                            });
                        }
                    } else {
                        fixed_spacer((150.0, 36.0));
                    }
                }
                textc(color, "Mixed-use homes pay residential tax.");
                textc(color, "Mixed-use shops pay commercial tax.");
            });
        });
        divider(color.with_alpha(0.3), 3.0, 1.0);
        titlec(color, "Day history");
        if selectable_label_primary(
            selection.get().is_none(),
            format!("Current day ({})", current_day + 1).as_str(),
        )
        .clicked
        {
            selection.set(None);
        }
        VertScrollSize::Fixed(104.0).show(|| {
            mincolumn(4.0, || {
                for (&day, entries) in budget.days().range(..current_day).rev() {
                    let net: i128 = entries.values().sum();
                    let label = format!("Day {}    {}", day + 1, money(net, true));
                    if selectable_label_primary(selection.get() == Some(day), &label).clicked {
                        selection.set(Some(day));
                    }
                }
                if budget.days().range(..current_day).next().is_none() {
                    textc(color, "No completed days with transactions");
                }
            });
        });
    });
}

fn ledger_row(name: &'static str, amount: i128) {
    let mut row = List::row();
    row.item_spacing = 16.0;
    row.main_axis_alignment = MainAxisAlignment::SpaceBetween;
    row.show(|| {
        textc(on_secondary_container(), name);
        monospace(on_secondary_container(), money(amount, true));
    });
}

pub(super) fn money(inner: i128, signed: bool) -> String {
    let magnitude = inner.unsigned_abs() / 100;
    let sign = if inner < 0 {
        "−"
    } else if signed && inner > 0 {
        "+"
    } else {
        ""
    };
    format!("{sign}${}.{:02}", magnitude / 100, magnitude % 100)
}

#[cfg(test)]
#[test]
fn scale_t1_fix2_tax_slider_respects_backpressure() {
    use crate::uiworld::SaveLoadState;
    use yakui::event::Event;
    use yakui::input::MouseButton;
    use yakui::widgets::SliderWidget;
    use yakui::{Rect, Vec2, WidgetId, Yakui};

    fn first_slider(ui: &Yakui, id: WidgetId) -> Option<WidgetId> {
        let node = ui.dom().get(id).unwrap();
        if node.widget.is::<SliderWidget>() {
            return Some(id);
        }
        node.children.iter().find_map(|&id| first_slider(ui, id))
    }

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let sim = Simulation::new(false);
    let uiw = UiWorld::with_defaults();
    let mut ui = Yakui::new();
    let viewport = Vec2::new(1600.0, 900.0);
    ui.set_surface_size(viewport);
    ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
    let draw = |ui: &mut Yakui| {
        ui.start();
        budget(&uiw, &sim, &mut true);
        ui.finish();
        ui.paint();
    };
    let start_drag = |ui: &mut Yakui| {
        let slider = first_slider(ui, ui.dom().root()).unwrap();
        let knob = ui.dom().get(slider).unwrap().children[1];
        let rect = ui.layout_dom().get(knob).unwrap().rect;
        let center = rect.pos() + rect.size() * 0.5;
        ui.handle_event(Event::CursorMoved(Some(center)));
        ui.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down: true,
        });
        ui.handle_event(Event::CursorMoved(Some(center + Vec2::new(55.0, 0.0))));
    };

    draw(&mut ui);
    start_drag(&mut ui);
    uiw.commands()
        .push(WorldCommand::SpawnRandomCars { n_cars: 0 });
    uiw.write::<SaveLoadState>().authoring_blocked = true;
    draw(&mut ui);
    assert!(matches!(
        uiw.commands().as_ref(),
        [WorldCommand::SpawnRandomCars { n_cars: 0 }]
    ));

    ui.handle_event(Event::MouseButtonChanged {
        button: MouseButton::One,
        down: false,
    });
    *uiw.commands() = Default::default();
    uiw.write::<SaveLoadState>().authoring_blocked = false;
    draw(&mut ui);
    start_drag(&mut ui);
    draw(&mut ui);
    assert!(matches!(
        uiw.commands().as_ref(),
        [WorldCommand::SetTaxRate { .. }]
    ));
}
