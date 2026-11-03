//! Needs a real X server with a spare disconnected RandR output (your laptop session).
//! Run manually: `cargo test --test display_x11 -- --ignored --nocapture`
//! Creates and removes a virtual monitor; leaves the layout as it found it.

use padscreen::display::{DisplayOpts, Placement, Rect, VirtualDisplay};
use std::process::Command;

fn monitors() -> Vec<String> {
    let o = Command::new("xrandr")
        .arg("--listmonitors")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .skip(1)
        .map(|l| l.trim().to_string())
        .collect()
}

/// Monitor list lags the RandR request slightly (server/mutter update), so poll briefly.
fn wait_monitors(what: &str, ok: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..40 {
        let m = monitors();
        if ok(&m) {
            return m;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let m = monitors();
    panic!("{what}: {m:?}");
}

fn snapshot() -> String {
    // geometry of all monitors, order-insensitive
    let mut m: Vec<String> = monitors()
        .iter()
        .filter_map(|l| l.split_whitespace().nth(2).map(String::from))
        .collect();
    m.sort();
    m.join(" ")
}

fn screen_size() -> String {
    let o = Command::new("xdpyinfo").output().unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find(|l| l.contains("dimensions:"))
        .unwrap()
        .trim()
        .to_string()
}

fn opts(p: Placement) -> DisplayOpts {
    DisplayOpts {
        output: None,
        reference: None,
        placement: p,
        offset: 0,
        hz: 60,
    }
}

#[test]
#[ignore]
fn create_resize_destroy_all_placements() {
    // connect() removes stale padscreen monitors from earlier runs, so take the baseline after it.
    drop(VirtualDisplay::connect(opts(Placement::Right)).expect("connect"));
    let before_mon = snapshot();
    let before_size = screen_size();
    println!("before: {before_mon} | {before_size}");

    for p in [
        Placement::Right,
        Placement::Below,
        Placement::Left,
        Placement::Above,
    ] {
        let mut d = VirtualDisplay::connect(opts(p)).expect("connect");
        let r: Rect = d.up(800, 600).expect("up 800x600");
        let name = d.output_name().unwrap();
        println!("{p:?}: {name} -> {r:?}");
        wait_monitors(&format!("{p:?}: new monitor missing"), |l| {
            l.iter().any(|l| l.contains(&name) && l.contains("800/"))
        });
        assert_eq!((r.w, r.h), (800, 600));
        assert!(
            r.x >= 0 && r.y >= 0,
            "positions must be non-negative: {r:?}"
        );

        // resize (rotation-like)
        let r2 = d.up(600, 800).expect("up 600x800");
        assert_eq!((r2.w, r2.h), (600, 800));
        if matches!(p, Placement::Right | Placement::Below) {
            assert_eq!(
                (r2.x, r2.y),
                (r.x, r.y),
                "{p:?}: in-place resize must keep the top-left corner"
            );
        }
        assert_eq!(d.up(600, 800).unwrap(), r2, "same size is a no-op");
        wait_monitors(&format!("{p:?}: resized monitor missing"), |l| {
            l.iter().any(|l| l.contains(&name) && l.contains("600/"))
        });

        d.down();
        wait_monitors(&format!("{p:?}: monitor still present after down"), |l| {
            !l.iter().any(|l| l.contains(&name))
        });
        assert_eq!(snapshot(), before_mon, "{p:?}: layout not restored");
        assert_eq!(
            screen_size(),
            before_size,
            "{p:?}: screen size not restored"
        );
    }
}

#[test]
#[ignore]
fn drop_cleans_up_and_stale_modes_are_removed() {
    let before = snapshot();
    {
        let mut d = VirtualDisplay::connect(opts(Placement::Right)).unwrap();
        d.up(640, 480).unwrap();
        assert_ne!(snapshot(), before);
    }
    assert_eq!(snapshot(), before, "Drop must remove the monitor");
}
