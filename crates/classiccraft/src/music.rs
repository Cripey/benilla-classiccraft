//! WoW music for Minecraft's jukebox (2026-10-03, user's pick): a few of the client's own tracks as
//! music discs. Extracted once from the user's install to `~/.local/share/classiccraft/music/
//! <key>.mp3`; the Fabric mod converts each to the Ogg Minecraft streams (`McwowMusic`). Nothing
//! of the client's data leaves the user's machine.

use std::path::PathBuf;

use bevy::prelude::*;

/// The discs (and the waygate's arrival sound, user's pick 2026-10-03): Minecraft key and the
/// client's file, written as `<key>.<its extension>`.
pub const TRACKS: [(&str, &str); 5] = [
    ("tavern_alliance", "sound\\music\\zonemusic\\tavernalliance\\tavernalliance01.mp3"),
    ("sacred", "sound\\music\\musical moments\\sacred\\sacred01.mp3"),
    ("main_theme", "sound\\music\\gluescreenmusic\\wow_main_theme.mp3"),
    ("thunder_bluff", "sound\\music\\citymusic\\thunderbluff\\thunderbluff walking 03.mp3"),
    ("waygate_teleport", "sound\\spells\\teleport.wav"),
];

fn file_name(key: &str, path: &str) -> String {
    format!("{key}.{}", path.rsplit('.').next().unwrap_or("mp3"))
}

pub struct MusicPlugin;

impl Plugin for MusicPlugin {
    fn build(&self, _app: &mut App) {
        std::thread::spawn(|| {
            if let Err(e) = extract_missing() {
                warn!("classiccraft: music: {e}");
            }
        });
    }
}

fn dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/classiccraft/music"))
}

fn extract_missing() -> anyhow::Result<()> {
    let dir = dir().ok_or_else(|| anyhow::anyhow!("no $HOME"))?;
    std::fs::create_dir_all(&dir)?;
    let wanted: Vec<_> = TRACKS
        .iter()
        .filter(|(key, path)| !dir.join(file_name(key, path)).exists())
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let data = benilla_formats::wow_data().ok_or_else(|| anyhow::anyhow!("no WoW install found"))?;
    let mut chain = benilla_formats::open_chain(&data)?;
    for (key, path) in wanted {
        match chain.read_file(path) {
            Ok(bytes) => {
                std::fs::write(dir.join(file_name(key, path)), bytes)?;
                info!("classiccraft: music {key} extracted");
            }
            Err(e) => warn!("classiccraft: music {key} ({path}): {e}"),
        }
    }
    Ok(())
}
