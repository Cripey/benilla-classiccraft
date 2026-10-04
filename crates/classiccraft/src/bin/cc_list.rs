//! Offline: list the client's files whose path contains every argument (case-insensitive).
fn main() -> anyhow::Result<()> {
    let words: Vec<String> = std::env::args().skip(1).map(|w| w.to_ascii_lowercase()).collect();
    let data = benilla_formats::wow_data().expect("no WoW install ($WOW_DATA)");
    let chain = benilla_formats::open_chain(&data)?;
    for e in chain.list()? {
        let n = e.name.to_ascii_lowercase();
        if words.iter().all(|w| n.contains(w.as_str())) {
            println!("{:>10}  {}", e.size, e.name);
        }
    }
    Ok(())
}
