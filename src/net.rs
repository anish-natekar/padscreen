//! Find the laptop's address on the USB network link to the iPad.

use std::{
    fs,
    path::Path,
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Kernel drivers an iPad shows up as when tethered over USB.
const USB_NET_DRIVERS: &[&str] = &["cdc_ncm", "ipheth", "cdc_ether", "rndis_host"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub iface: String,
    pub ip: String,
}

/// Parse `ip -4 -o addr show` output into (iface, ip) pairs.
pub fn parse_ip_addr(out: &str) -> Vec<Link> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _idx = it.next()?;
            let iface = it.next()?.trim_end_matches(':').to_string();
            let mut it = it.skip_while(|t| *t != "inet");
            it.next()?;
            let ip = it.next()?.split('/').next()?.to_string();
            Some(Link { iface, ip })
        })
        .collect()
}

fn driver_of(iface: &str) -> Option<String> {
    let p = fs::read_link(
        Path::new("/sys/class/net")
            .join(iface)
            .join("device/driver"),
    )
    .ok()?;
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Addresses of interfaces that are USB network links (the iPad end is the same /24).
pub fn usb_links() -> Vec<Link> {
    let Ok(o) = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
    else {
        return vec![];
    };
    parse_ip_addr(&String::from_utf8_lossy(&o.stdout))
        .into_iter()
        .filter(|l| driver_of(&l.iface).is_some_and(|d| USB_NET_DRIVERS.contains(&d.as_str())))
        .collect()
}

/// `usb_links()` spawns `ip`, so remember the answer briefly instead of running it per request.
pub struct LinkCache {
    ttl: Duration,
    cached: Mutex<Option<(Instant, Vec<Link>)>>,
}

impl LinkCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            cached: Mutex::new(None),
        }
    }

    pub fn get(&self) -> Vec<Link> {
        let mut c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        match &*c {
            Some((at, links)) if at.elapsed() < self.ttl => links.clone(),
            _ => {
                let links = usb_links();
                *c = Some((Instant::now(), links.clone()));
                links
            }
        }
    }

    /// Loopback or the USB link; loopback never needs the `ip` lookup.
    pub fn allows(&self, peer: std::net::IpAddr) -> bool {
        is_local_or_usb(peer, &[]) || is_local_or_usb(peer, &self.get())
    }
}

/// Loopback, or an address on the same /24 as one of our USB links.
pub fn is_local_or_usb(peer: std::net::IpAddr, links: &[Link]) -> bool {
    use std::net::IpAddr;
    let peer = match peer {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    if peer.is_loopback() {
        return true;
    }
    let IpAddr::V4(p) = peer else { return false };
    links.iter().any(|l| {
        l.ip.parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.octets()[..3] == p.octets()[..3])
    })
}

pub fn urls(links: &[Link], port: u16, tls: bool) -> Vec<String> {
    let scheme = if tls { "https" } else { "http" };
    links
        .iter()
        .map(|l| format!("{scheme}://{}:{port}/", l.ip))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_output() {
        let out = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n\
                   5: enx1a2b3c    inet 10.42.1.1/24 brd 10.42.1.255 scope global noprefixroute enx1a2b3c\\       valid_lft forever\n\
                   7: wlp2s0    inet 192.168.1.5/24 brd 192.168.1.255 scope global dynamic wlp2s0\n";
        let l = parse_ip_addr(out);
        assert_eq!(l.len(), 3);
        assert_eq!(
            l[1],
            Link {
                iface: "enx1a2b3c".into(),
                ip: "10.42.1.1".into()
            }
        );
        assert_eq!(urls(&l[1..2], 8443, true), vec!["https://10.42.1.1:8443/"]);
    }

    #[test]
    fn input_gate() {
        let usb = vec![Link {
            iface: "enx1".into(),
            ip: "10.42.1.1".into(),
        }];
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert!(is_local_or_usb(ip("10.42.1.41"), &usb));
        assert!(is_local_or_usb(ip("127.0.0.1"), &usb));
        assert!(is_local_or_usb(ip("::ffff:10.42.1.41"), &usb));
        assert!(
            !is_local_or_usb(ip("192.168.1.77"), &usb),
            "wifi peer must not inject"
        );
        assert!(!is_local_or_usb(ip("10.42.2.5"), &usb));
        assert!(!is_local_or_usb(ip("10.42.1.41"), &[]));
    }
}
