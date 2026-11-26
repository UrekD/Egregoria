use common::FastMap;
use engine::yakui::YakuiWrapper;
use engine::GfxContext;
use std::path::Path;

use crate::service_icon::ServiceIcon;

#[derive(Default)]
pub struct UiTextures {
    yakui_textures: FastMap<String, yakui::TextureId>,
    service_icons: FastMap<ServiceIcon, yakui::TextureId>,
}

impl UiTextures {
    pub fn new(gfx: &mut GfxContext, yakui: &mut YakuiWrapper) -> Self {
        let texdirs = [("assets/ui", ""), ("assets/ui/icons", "icon/")];

        let mut yakui_textures: FastMap<String, yakui::TextureId> = Default::default();

        for (prefix, path) in texdirs.iter().flat_map(|(path, prefix)| {
            common::saveload::walkdir(Path::new(path)).map(move |x| (prefix, x))
        }) {
            let name = prefix.to_string() + path.file_stem().unwrap().to_str().unwrap();

            yakui_textures.insert(name, yakui.load_texture(gfx, &path));
        }
        let service_icons = ServiceIcon::ALL
            .into_iter()
            .map(|icon| (icon, yakui.add_texture(&icon.texture(gfx))))
            .collect();
        Self {
            yakui_textures,
            service_icons,
        }
    }

    pub fn get(&self, name: &str) -> yakui::TextureId {
        match self.yakui_textures.get(name) {
            None => panic!("Couldn't find texture (yakui) {}", name),
            Some(x) => *x,
        }
    }

    pub fn try_get(&self, name: &str) -> Option<yakui::TextureId> {
        self.yakui_textures.get(name).copied()
    }

    pub(crate) fn service_icon(&self, icon: ServiceIcon) -> Option<yakui::TextureId> {
        self.service_icons.get(&icon).copied()
    }
}
