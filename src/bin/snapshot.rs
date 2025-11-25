use openworkers_runtime_v8::snapshot::create_runtime_snapshot;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");

fn main() -> std::io::Result<()> {
    println!("Creating V8 runtime snapshot...");

    let snapshot = match create_runtime_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("Failed to create snapshot: {}", err);
            std::process::exit(1);
        }
    };

    let path = PathBuf::from(RUNTIME_SNAPSHOT_PATH);
    let mut file = File::create(&path)?;
    file.write_all(&snapshot.output)?;

    println!(
        "✅ Snapshot created: {} ({} bytes)",
        path.display(),
        snapshot.output.len()
    );

    Ok(())
}
