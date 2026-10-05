//! Zed's bundled assets, with Forge's branding layered on top: Forge's own images under
//! `images/forge_*`, and Zed's logo path answered with Forge's mark wherever Zed UI shows it.

use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

pub struct ForgeAssets;

const OVERRIDES: &[(&str, &[u8])] = &[
    ("images/forge_logo.svg", include_bytes!("../assets/images/forge_logo.svg")),
    ("images/forge_mark.svg", include_bytes!("../assets/images/forge_mark.svg")),
    // Icons for Forge's own panels (see forge_ui::panel_icon).
    ("icons/forge_tests.svg", include_bytes!("../assets/icons/forge_tests.svg")),
    ("icons/forge_agents.svg", include_bytes!("../assets/icons/forge_agents.svg")),
    ("icons/forge_extensions.svg", include_bytes!("../assets/icons/forge_extensions.svg")),
    ("icons/forge_anvil.svg", include_bytes!("../assets/icons/forge_anvil.svg")),
    ("icons/forge_output.svg", include_bytes!("../assets/icons/forge_output.svg")),
    ("icons/forge_history.svg", include_bytes!("../assets/icons/forge_history.svg")),
    ("icons/forge_git.svg", include_bytes!("../assets/icons/forge_git.svg")),
    ("icons/forge_solution.svg", include_bytes!("../assets/icons/forge_solution.svg")),
    ("icons/forge_project.svg", include_bytes!("../assets/icons/forge_project.svg")),
    ("icons/forge_nuget.svg", include_bytes!("../assets/icons/forge_nuget.svg")),
    // `ui::Icon::from_path` only embeds paths under `icons/`.
    ("icons/forge_mark.svg", include_bytes!("../assets/images/forge_mark.svg")),
    // `IconName::ZedAgent` (agent actions, thread lists) draws Forge's agents icon, the
    // same as the Threads panel's, instead of Zed's.
    ("icons/zed_agent.svg", include_bytes!("../assets/icons/forge_agents.svg")),
    // Rendered by Zed's `Vector` (as a single-colour mask) on Zed-provided screens.
    ("images/zed_logo.svg", include_bytes!("../assets/images/forge_mark.svg")),
];

impl AssetSource for ForgeAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OVERRIDES.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut out = assets::Assets.list(path)?;
        out.extend(OVERRIDES.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)));
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branding_overrides_zed_assets() {
        let logo = ForgeAssets.load("images/zed_logo.svg").unwrap().unwrap();
        assert!(std::str::from_utf8(&logo).unwrap().contains("<svg"));
        assert_eq!(logo, ForgeAssets.load("images/forge_mark.svg").unwrap().unwrap());
        assert_eq!(ForgeAssets.load("icons/zed_agent.svg").unwrap().unwrap(), ForgeAssets.load("icons/forge_agents.svg").unwrap().unwrap());
        assert!(ForgeAssets.load("icons/LICENSES").unwrap().is_some(), "other assets still come from Zed");
    }
}
