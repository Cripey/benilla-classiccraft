//! Offline: copy the client's files whose path contains every argument into the directory `$OUT`
//! (flattened names), for inspection. Never writes into the client.
fn main() -> anyhow::Result<()> {
    let out = std::path::PathBuf::from(std::env::var("OUT").expect("set $OUT"));
    std::fs::create_dir_all(&out)?;
    let words: Vec<String> = std::env::args().skip(1).map(|w| w.to_ascii_lowercase()).collect();
    let data = benilla_formats::wow_data().expect("no WoW install ($WOW_DATA)");
    let mut chain = benilla_formats::open_chain(&data)?;
    for e in chain.list()? {
        let n = e.name.to_ascii_lowercase();
        if words.iter().all(|w| n.contains(w.as_str())) {
            let bytes = chain.read_file(&n)?;
            std::fs::write(out.join(e.name.replace('\\', "__")), bytes)?;
        }
    }
    Ok(())
}
