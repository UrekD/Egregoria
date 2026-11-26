use engine::{GfxContext, Texture, TextureBuilder};
use prototypes::{try_prototype, GoodsCompanyPrototype, ServiceID, ServiceKind};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ServiceIcon {
    Fire,
    Police,
    Clinic,
    School,
    Water,
    Sewage,
    Power,
    Garbage,
    Park,
    Warehouse,
    Depot,
    BusStop,
    Services,
}

impl ServiceIcon {
    pub(crate) const ALL: [Self; 13] = [
        Self::Fire,
        Self::Police,
        Self::Clinic,
        Self::School,
        Self::Water,
        Self::Sewage,
        Self::Power,
        Self::Garbage,
        Self::Park,
        Self::Warehouse,
        Self::Depot,
        Self::BusStop,
        Self::Services,
    ];

    pub(crate) fn for_company(company: &GoodsCompanyPrototype) -> Option<Self> {
        if let Some(service) = try_prototype(ServiceID::from(company.id)) {
            return Some(match service.service_kind {
                ServiceKind::Fire => Self::Fire,
                ServiceKind::Police => Self::Police,
                ServiceKind::Health => Self::Clinic,
                ServiceKind::School => Self::School,
                ServiceKind::WaterPump => Self::Water,
                ServiceKind::SewageOutlet => Self::Sewage,
                ServiceKind::Garbage => Self::Garbage,
                ServiceKind::Park => Self::Park,
            });
        }
        if company.storage.is_some() {
            return Some(if company.name == "small-depot" {
                Self::Depot
            } else {
                Self::Warehouse
            });
        }
        company.power_production.is_some().then_some(Self::Power)
    }

    pub(crate) fn texture(self, gfx: &GfxContext) -> Texture {
        macro_rules! png {
            ($name:literal) => {
                include_bytes!(concat!("../art/service_icons/", $name, ".png"))
            };
        }
        let bytes: &[u8] = match self {
            Self::Fire => png!("fire"),
            Self::Police => png!("police"),
            Self::Clinic => png!("clinic"),
            Self::School => png!("school"),
            Self::Water => png!("water"),
            Self::Sewage => png!("sewage"),
            Self::Power => png!("power"),
            Self::Garbage => png!("garbage"),
            Self::Park => png!("park"),
            Self::Warehouse => png!("warehouse"),
            Self::Depot => png!("depot"),
            Self::BusStop => png!("bus-stop"),
            Self::Services => png!("services"),
        };
        TextureBuilder::from_bytes(bytes)
            .expect("embedded service icon should be a valid PNG")
            .with_label("service icon")
            .with_mipmaps(&gfx.mipmap_gen)
            .build(&gfx.device, &gfx.queue)
    }
}
