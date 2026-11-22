use crate::gui::zoning::{zone_name, ZONES};
use crate::uiworld::UiWorld;
use goryak::{
    mincolumn, minrow, monospace, on_secondary_container, selectable_label_primary, textc, titlec,
    VertScrollSize, Window,
};
use simulation::economy::{
    Budget, BudgetCategory, Government, MAX_TAX_RATE_PERMILLE, TICKS_PER_BUDGET_DAY,
};
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
        let budget = sim.read::<Budget>();
        let selection = use_state(|| None::<u64>);
        let current_day = sim.get_tick().saturating_sub(1) / TICKS_PER_BUDGET_DAY;
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
                    (BudgetCategory::MilestoneGrant, "Milestone grants"),
                    (BudgetCategory::TradeImport, "Imports"),
                    (BudgetCategory::TradeExport, "Exports"),
                    (BudgetCategory::Tax(ZONES[0]), "Residential tax"),
                    (BudgetCategory::Tax(ZONES[1]), "Commercial tax"),
                    (BudgetCategory::Tax(ZONES[2]), "Industrial tax"),
                    (BudgetCategory::Tax(ZONES[3]), "Office tax"),
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
                for zone in ZONES {
                    let rate = budget.tax_rate(zone);
                    textc(
                        color,
                        format!("{}  {:.1}%", zone_name(zone), f32::from(rate) / 10.0),
                    );

                    let mut slider =
                        Slider::new(f64::from(rate), 0.0, f64::from(MAX_TAX_RATE_PERMILLE));
                    slider.step = Some(1.0);
                    if let Some(value) = slider.show().value {
                        uiworld.commands().push(WorldCommand::SetTaxRate {
                            zone,
                            rate_permille: value as u16,
                        });
                    }
                }
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
