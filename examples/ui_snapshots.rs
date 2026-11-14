//! Render every settings and toast surface to PNG with fixed sample data.
//! `cargo run --example ui_snapshots -- <dir>` (default: target/ui-snapshots).

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "target/ui-snapshots".into());
    match athermemguard::ui::snapshots::write_all(std::path::Path::new(&dir)) {
        Ok(files) => {
            for f in files {
                println!("{}", f.display());
            }
        }
        Err(e) => {
            eprintln!("ui_snapshots: {}", e);
            std::process::exit(1);
        }
    }
}
