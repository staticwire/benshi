//! Print every media source the platform currently reports.
//!
//! The smallest program that observes the real world, kept because every later
//! question about an adapter starts with "what does it actually see?".

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use benshi_detect::mpris::MprisWatcher;
    use std::time::Duration;

    let watcher = MprisWatcher::connect(Duration::from_secs(2)).await?;
    let sources = watcher.sources().await?;

    if sources.is_empty() {
        println!("no media source is publishing itself");
        return Ok(());
    }

    let can = |declared, name| if declared { name } else { "-" };

    for source in sources {
        println!(
            "{:<28} app={:<16} {:<8} {} {} {} {}",
            source.player.0,
            source.app.0,
            format!("{:?}", source.state),
            can(source.capabilities.position, "position"),
            can(source.capabilities.duration, "duration"),
            can(source.capabilities.paused, "paused"),
            can(source.capabilities.file_path, "path"),
        );
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("this example needs MPRIS, which exists only on Linux");
}
