//! WoW music for Minecraft's jukebox (2026-10-03, user's pick): a few of the client's own tracks as
//! music discs. Extracted once from the user's install and turned into the mono Ogg Vorbis
//! Minecraft streams, `<data dir>/music/<key>.ogg` (`link.rs`), which the Fabric mod plays
//! (`McwowMusic`). Nothing of the client's data leaves the user's machine.

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
    crate::link::data_dir().map(|d| d.join("music"))
}

fn extract_missing() -> anyhow::Result<()> {
    let dir = dir().ok_or_else(|| anyhow::anyhow!("no data directory ($HOME / %APPDATA%)"))?;
    std::fs::create_dir_all(&dir)?;
    let wanted: Vec<_> = TRACKS
        .iter()
        .filter(|(key, _)| !dir.join(format!("{key}.ogg")).exists())
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let mut chain = None;
    for (key, path) in wanted {
        // A source extracted by an older build (when the mod still ran ffmpeg) is used as is.
        let source = dir.join(file_name(key, path));
        let bytes = match std::fs::read(&source) {
            Ok(b) => b,
            Err(_) => {
                if chain.is_none() {
                    let data =
                        benilla_formats::wow_data().ok_or_else(|| anyhow::anyhow!("no WoW install found"))?;
                    chain = Some(benilla_formats::open_chain(&data)?);
                }
                match chain.as_mut().expect("opened above").read_file(path) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("classiccraft: music {key} ({path}): {e}");
                        continue;
                    }
                }
            }
        };
        let tmp = dir.join(format!("{key}.tmp.ogg"));
        match to_ogg(bytes).and_then(|ogg| Ok(std::fs::write(&tmp, ogg)?)) {
            Ok(()) => {
                std::fs::rename(&tmp, dir.join(format!("{key}.ogg")))?;
                info!("classiccraft: music {key} ready");
            }
            Err(e) => warn!("classiccraft: music {key}: {e}"),
        }
    }
    Ok(())
}

/// MP3/WAV -> mono Ogg Vorbis (quality 0.5, as `ffmpeg -q:a 5` made it): Minecraft places only
/// mono sounds at the jukebox.
fn to_ogg(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let sound = kira::sound::static_sound::StaticSoundData::from_cursor(std::io::Cursor::new(bytes))?;
    let mono: Vec<f32> = sound.frames.iter().map(|f| (f.left + f.right) * 0.5).collect();
    let rate = std::num::NonZeroU32::new(sound.sample_rate).ok_or_else(|| anyhow::anyhow!("no sample rate"))?;
    let mut enc = vorbis_rs::VorbisEncoderBuilder::new(rate, std::num::NonZeroU8::MIN, Vec::new())?.build()?;
    for block in mono.chunks(4096) {
        enc.encode_audio_block([block])?;
    }
    Ok(enc.finish()?)
}
