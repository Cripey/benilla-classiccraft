//! Offline check of the dance extraction (`dances.rs`): `cc_dances <out dir>` writes every race and
//! gender's file there and prints each one's frame count and first frame. Reads `$WOW_DATA`.
#[path = "../dances.rs"]
#[allow(dead_code)]
mod dances;

fn main() -> anyhow::Result<()> {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: cc_dances <out dir>"));
    std::fs::create_dir_all(&out)?;
    let data = benilla_formats::wow_data().expect("no WoW install ($WOW_DATA)");
    let mut chain = benilla_formats::open_chain(&data)?;
    if std::env::var_os("CC_DUMP").is_some() {
        dump(&chain.read_file("character\\human\\male\\humanmale.m2")?);
        return Ok(());
    }
    for (id, folder) in dances::RACES {
        for (sex, gender) in [(0, "Male"), (1, "Female")] {
            let path = format!("character\\{folder}\\{gender}\\{folder}{gender}.m2").to_ascii_lowercase();
            let bytes = chain.read_file(&path)?;
            let vars: Vec<String> = benilla_formats::parse_m2_animations(&bytes)
                .iter()
                .filter(|a| a.anim_id == 69)
                .map(|a| format!("{:.2}s w{} loop{} replay{}-{}", a.duration, a.frequency, a.looping, a.min_replay, a.max_replay))
                .collect();
            println!("{folder:>8} {gender:<6} dance variations: {vars:?}");
            match dances::extract(&bytes) {
                Ok(variations) => {
                    let frames = &variations[0].frames;
                    let f = frames[0];
                    println!("{folder:>8} {gender:<6} {:4} frames  first: root ({:+.1},{:+.1},{:+.1}) rot ({:+.2},{:+.2},{:+.2}) head ({:+.2},{:+.2},{:+.2}) armR ({:+.2},{:+.2}) armL ({:+.2},{:+.2}) legR ({:+.2},{:+.2}) legL ({:+.2},{:+.2})",
                        frames.len(), f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9], f[10], f[11], f[12], f[13], f[14], f[15], f[16]);
                    let _ = (id, sex);
                }
                Err(e) => println!("{folder:>8} {gender:<6} ERROR {e}"),
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn dump(bytes: &[u8]) {
    let sk = benilla_formats::parse_m2_skeleton(bytes).unwrap();
    for (i, b) in sk.bones.iter().enumerate() {
        println!("bone {i:3} key {:3} parent {:3} pivot ({:+.3},{:+.3},{:+.3})", b.key_bone, b.parent, b.pivot[0], b.pivot[1], b.pivot[2]);
    }
    for a in benilla_formats::parse_m2_attachments(bytes).unwrap() {
        println!("attach {:3} bone {:3} pos ({:+.3},{:+.3},{:+.3})", a.id, a.bone, a.position[0], a.position[1], a.position[2]);
    }
}
