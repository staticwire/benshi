//! Record one episode into the database at the path given, say so, and sleep
//! until killed.
//!
//! Exists for one test, which kills this process with `SIGKILL` after the line
//! is printed and then reads what the file holds. It is not a command anybody
//! runs by hand.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

use benshi_core::path::RawPath;
use benshi_store::{Store, Viewing};

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("the database path is the one argument");
    let mut store = Store::open(&path).expect("the database opens");

    store
        .record(&Viewing {
            title: "Show Title".to_owned(),
            episode: Some(3),
            media: RawPath::from_bytes(b"/anime/[Group] Show Title - 03.mkv".to_vec()),
            at: SystemTime::now(),
        })
        .expect("recording cannot fail");
    println!("recorded");

    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
