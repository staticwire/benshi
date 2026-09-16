//! Print every media source the platform reports, and one reading from each.
//!
//! The smallest program that observes the real world, kept because every later
//! question about an adapter starts with "what does it actually see?".

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use benshi_core::clock::SystemClock;
    use benshi_core::{MediaRef, encoding};
    use benshi_detect::PlayerWatcher;
    use benshi_detect::mpris::{MprisWatcher, SOURCE_DEADLINE};

    let mut watcher = MprisWatcher::connect(SystemClock::new(), SOURCE_DEADLINE).await?;
    let sources = watcher.sources().await?;

    if sources.is_empty() {
        println!("no media source is publishing itself");
        return Ok(());
    }

    let can = |declared, name| if declared { name } else { "-" };

    for source in &sources {
        println!(
            "{:<28} app={:<16} {:<8} {} {} {} {}",
            source.player.0,
            source.app.0,
            format!("{:?}", source.state),
            can(source.capabilities.position, "position"),
            can(source.capabilities.duration, "duration"),
            can(source.capabilities.paused, "paused"),
            can(source.capabilities.location, "location"),
        );
    }

    println!();
    let outcome = watcher.poll().await?;

    for snapshot in &outcome.snapshots {
        // A filename is bytes, so it is decoded for display here and nowhere
        // else: what a byte string says is a decision, and an adapter takes
        // none. The encoding and how it was arrived at are printed with it.
        let open = match &snapshot.media {
            MediaRef::LocalFile(path) => {
                let name = encoding::decode(path.file_name());
                format!("{} [{} {:?}]", name.text, name.encoding, name.confidence)
            }
            MediaRef::Remote(address) => address.clone(),
            MediaRef::Title(title) => title.clone(),
        };

        println!(
            "{:<28} {:?} / {:?}  {open}",
            snapshot.player.0, snapshot.position, snapshot.duration
        );
    }

    for (player, error) in &outcome.failures {
        println!("{:<28} failed: {error}", player.0);
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("this example needs MPRIS, which exists only on Linux");
}
