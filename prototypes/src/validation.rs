#![allow(clippy::iter_over_hash_type)]

use thiserror::Error;

use common::error::MultiError;

use crate::{CompanyKind, GrowableCompanyID, Money, Prototypes};

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("{0}: only factories can have trucks")]
    WrongTrucks(String),
    #[error("{0}: factories must have trucks if it produces things")]
    ZeroTrucks(String),
    #[error("{0}.{1}: referenced prototype not found")]
    ReferencedProtoNotFound(String, &'static str),

    #[error("{0}.{1}: {2}")]
    InvalidField(String, &'static str, String),
}

pub(crate) fn validate(proto: &Prototypes) -> Result<(), MultiError<ValidationError>> {
    let mut errors = vec![];

    for service in proto.service.values() {
        for (invalid, field, message) in [
            (
                !(service.radius_m > 0.0 && service.radius_m <= 3000.0),
                "radius_m",
                "must be finite and in (0, 3000]",
            ),
            (
                !(1..=10_000).contains(&service.capacity),
                "capacity",
                "must be in [1, 10000]",
            ),
            (
                !(1..=1000).contains(&service.n_workers),
                "n_workers",
                "must be in [1, 1000]",
            ),
            (
                service.upkeep_per_day < Money::ZERO,
                "upkeep_per_day",
                "must not be negative",
            ),
            (service.kind != CompanyKind::Store, "kind", "must be a store"),
            (service.n_trucks != 0, "n_trucks", "must be zero"),
            (service.recipe.is_some(), "recipe", "must be omitted"),
        ] {
            if invalid {
                errors.push(ValidationError::InvalidField(
                    service.name.clone(),
                    field,
                    message.to_string(),
                ));
            }
        }
    }

    for comp in proto.goods_company.values() {
        if comp.n_trucks > 0 && comp.kind != CompanyKind::Factory {
            errors.push(ValidationError::WrongTrucks(comp.name.clone()));
        }

        if comp.n_trucks == 0
            && comp.kind == CompanyKind::Factory
            && !proto
                .growable_company
                .get(&GrowableCompanyID::from(comp.id))
                .is_some_and(|growable| growable.growable)
            && comp
                .recipe
                .as_ref()
                .map(|r| !r.production.is_empty())
                .unwrap_or(false)
        {
            errors.push(ValidationError::ZeroTrucks(comp.name.clone()));
        }

        if let Some(ref r) = comp.recipe {
            for item in &r.consumption {
                if !proto.item.contains_key(&item.id) {
                    errors.push(ValidationError::ReferencedProtoNotFound(
                        comp.name.clone(),
                        "consumption",
                    ));
                }
            }

            for item in &r.production {
                if !proto.item.contains_key(&item.id) {
                    errors.push(ValidationError::ReferencedProtoNotFound(
                        comp.name.clone(),
                        "production",
                    ));
                }
            }
        }

        if comp.power_consumption.map_or(false, |v| v.0 < 0) {
            errors.push(ValidationError::InvalidField(
                comp.name.clone(),
                "power_consumption",
                "must not be negative".to_string(),
            ));
        }

        if comp.power_production.map_or(false, |v| v.0 < 0) {
            errors.push(ValidationError::InvalidField(
                comp.name.clone(),
                "power_production",
                "must not be negative".to_string(),
            ));
        }
    }

    if !errors.is_empty() {
        return Err(MultiError(errors));
    }
    Ok(())
}
