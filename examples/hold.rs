//! Debug helper: bring the virtual monitor up and hold it for N seconds (default 5).
use padscreen::display::{DisplayOpts, Placement, VirtualDisplay};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let mut d = VirtualDisplay::connect(DisplayOpts {
        output: None,
        reference: None,
        placement: Placement::Right,
        offset: 0,
        hz: 60,
    })
    .unwrap();
    println!("{:?}", d.up(800, 600).unwrap());
    std::thread::sleep(std::time::Duration::from_secs(secs));
}
