//! The `classiccraft` launcher: benilla with the Minecraft bridge on top, started through
//! [`benilla_app::run_with`] as a crate built on benilla starts it.

mod bridge;
mod combat;
mod dances;
mod decks;
mod geom;
mod music;
mod holes;
mod input;
mod overlay;
mod render;
mod shm;
mod terrain;

use benilla_app::BuildId;

fn main() -> benilla_app::AppExit {
    benilla_app::run_with(
        BuildId {
            version: env!("CARGO_PKG_VERSION"),
            describe: env!("BENILLA_GIT_DESCRIBE"),
            sha: env!("BENILLA_GIT_SHA"),
            short: env!("BENILLA_GIT_SHORT"),
            date: env!("BENILLA_GIT_DATE"),
            profile: env!("BENILLA_PROFILE"),
            project_dir: env!("BENILLA_PROJECT_DIR"),
            ..Default::default()
        },
        |app| {
            app.add_plugins((
                bridge::BridgePlugin,
                combat::CombatPlugin,
                dances::DancesPlugin,
                decks::DecksPlugin,
                geom::GeomPlugin,
                music::MusicPlugin,
                holes::HolesPlugin,
                input::InputBridgePlugin,
                overlay::OverlayPlugin,
                render::RenderLinkPlugin,
                terrain::TerrainPlugin,
            ));
        },
    )
}
