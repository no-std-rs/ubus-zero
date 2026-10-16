//! OpenWrt fields and counter arithmetic, independent of the board and clock.

use alloc::string::{String, ToString};
use ubus::{blobmsg, BlobIter, BlobMsgTable, BlobValue};

use crate::rpc::Error;

fn integer(buf: &[u8], key: &str) -> Option<i64> {
    match blobmsg::lookup(buf, key)?.value().ok()? {
        BlobValue::Int8(n) => Some(n.into()),
        BlobValue::Int16(n) => Some(n.into()),
        BlobValue::Int32(n) => Some(n.into()),
        BlobValue::Int64(n) => Some(n),
        _ => None,
    }
}

fn number(buf: &[u8], key: &str) -> Option<u64> {
    integer(buf, key)?.try_into().ok()
}

fn text(buf: &[u8], key: &str) -> Option<String> {
    Some(blobmsg::lookup(buf, key)?.as_str()?.to_string())
}

fn table<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    match blobmsg::lookup(buf, key)?.value().ok()? {
        BlobValue::Table(buf) => Some(buf),
        _ => None,
    }
}

fn array<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    match blobmsg::lookup(buf, key)?.value().ok()? {
        BlobValue::Array(buf) => Some(buf),
        _ => None,
    }
}

#[derive(Clone, Default)]
pub struct Board {
    pub hostname: Option<String>,
    pub model: Option<String>,
    pub release: Option<String>,
}

impl Board {
    pub fn parse(reply: &BlobMsgTable) -> Self {
        let b = reply.contents();
        Self {
            hostname: text(b, "hostname"),
            model: text(b, "model"),
            release: table(b, "release").and_then(|r| text(r, "version")),
        }
    }
}

#[derive(Clone, Default)]
pub struct System {
    pub uptime: Option<u64>,
    /// Linux load averages in Q16 fixed point, not CPU utilization percentages.
    pub load: [Option<u64>; 3],
    pub memory_total: Option<u64>,
    pub memory_available: Option<u64>,
    pub root_total: Option<u64>,
    pub root_free: Option<u64>,
}

impl System {
    pub fn parse(reply: &BlobMsgTable) -> Self {
        let b = reply.contents();
        let mut load = [None; 3];
        if let Some(values) = array(b, "load") {
            for (slot, attr) in load.iter_mut().zip(BlobIter::new(values)) {
                *slot = match attr.value() {
                    Ok(BlobValue::Int32(n)) => n.try_into().ok(),
                    Ok(BlobValue::Int64(n)) => n.try_into().ok(),
                    _ => None,
                };
            }
        }
        let memory = table(b, "memory").unwrap_or_default();
        let root = table(b, "root").unwrap_or_default();
        Self {
            uptime: number(b, "uptime"),
            load,
            memory_total: number(memory, "total"),
            memory_available: number(memory, "available").or_else(|| number(memory, "free")),
            // procd reports memory in bytes but filesystem sizes in KiB.
            root_total: number(root, "total").and_then(|n| n.checked_mul(1024)),
            root_free: number(root, "free").and_then(|n| n.checked_mul(1024)),
        }
    }
}

#[derive(Clone, Default)]
pub struct Wan {
    pub up: Option<bool>,
    pub device: Option<String>,
    pub protocol: Option<String>,
    pub address: Option<String>,
    pub uptime: Option<u64>,
}

impl Wan {
    pub fn parse(reply: &BlobMsgTable) -> Self {
        let b = reply.contents();
        let address = array(b, "ipv4-address")
            .and_then(|a| BlobIter::new(a).next())
            .and_then(|a| match a.value().ok()? {
                BlobValue::Table(t) => text(t, "address"),
                _ => None,
            });
        Self {
            up: blobmsg::lookup(b, "up").and_then(|a| a.as_bool()),
            device: text(b, "l3_device").or_else(|| text(b, "device")),
            protocol: text(b, "proto"),
            address,
            uptime: number(b, "uptime"),
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Counters {
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub rx_packets: Option<u64>,
    pub tx_packets: Option<u64>,
    pub rx_errors: Option<u64>,
    pub tx_errors: Option<u64>,
    pub rx_dropped: Option<u64>,
    pub tx_dropped: Option<u64>,
}

impl Counters {
    pub fn parse(reply: &BlobMsgTable) -> Self {
        let b = table(reply.contents(), "statistics").unwrap_or_default();
        Self {
            rx_bytes: number(b, "rx_bytes"),
            tx_bytes: number(b, "tx_bytes"),
            rx_packets: number(b, "rx_packets"),
            tx_packets: number(b, "tx_packets"),
            rx_errors: number(b, "rx_errors"),
            tx_errors: number(b, "tx_errors"),
            rx_dropped: number(b, "rx_dropped"),
            tx_dropped: number(b, "tx_dropped"),
        }
    }
}

#[derive(Clone, Default)]
pub struct Wifi {
    pub ssid: Option<String>,
    pub channel: Option<u64>,
    pub signal: Option<i32>,
    pub quality: Option<u64>,
    pub quality_max: Option<u64>,
    pub clients: Option<usize>,
}

impl Wifi {
    pub fn parse(reply: &BlobMsgTable) -> Self {
        let b = reply.contents();
        // rpcd-mod-iwinfo has emitted negative dBm as both signed JSON and
        // unsigned 32-bit two's-complement values across implementations.
        let signal = integer(b, "signal").and_then(|n| {
            i32::try_from(n)
                .ok()
                .or_else(|| u32::try_from(n).ok().map(|n| n as i32))
        });
        Self {
            ssid: text(b, "ssid"),
            channel: number(b, "channel"),
            signal,
            quality: number(b, "quality"),
            quality_max: number(b, "quality_max"),
            clients: None,
        }
    }

    pub fn set_clients(&mut self, reply: &BlobMsgTable) {
        self.clients = array(reply.contents(), "results").map(|a| BlobIter::new(a).count());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rates {
    /// WAN receive is download; WAN transmit is upload. Units: bytes/second.
    pub down: u64,
    pub up: u64,
}

#[derive(Default)]
pub struct RateTracker {
    previous: Option<(String, u64, u64, u64, u64)>,
}

impl RateTracker {
    pub fn reset(&mut self) {
        self.previous = None;
    }

    pub fn update(
        &mut self,
        device: &str,
        uptime: u64,
        now_ms: u64,
        counters: Counters,
    ) -> Option<Rates> {
        let (Some(rx), Some(tx)) = (counters.rx_bytes, counters.tx_bytes) else {
            self.reset();
            return None;
        };
        let previous = self
            .previous
            .replace((device.to_string(), uptime, now_ms, rx, tx));
        let (old_device, old_uptime, old_ms, old_rx, old_tx) = previous?;
        if old_device != device
            || uptime < old_uptime
            || rx < old_rx
            || tx < old_tx
            || now_ms <= old_ms
        {
            return None;
        }
        let elapsed = u128::from(now_ms - old_ms);
        let per_second = |delta| (u128::from(delta) * 1000 / elapsed).min(u64::MAX.into()) as u64;
        Some(Rates {
            down: per_second(rx - old_rx),
            up: per_second(tx - old_tx),
        })
    }
}

#[derive(Clone)]
pub struct Dashboard {
    pub board: Board,
    pub system: System,
    pub wan: Option<Wan>,
    pub counters: Option<Counters>,
    pub rates: Option<Rates>,
    /// Oldest at index 0. Gaps are retained on failures and counter resets.
    pub history: [Option<Rates>; 60],
    pub wifi: Option<Wifi>,
    pub last_sample_ms: Option<u64>,
    pub error: Option<Error>,
    pub wan_error: Option<Error>,
    pub wifi_error: Option<Error>,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            board: Board::default(),
            system: System::default(),
            wan: None,
            counters: None,
            rates: None,
            history: [None; 60],
            wifi: None,
            last_sample_ms: None,
            error: None,
            wan_error: None,
            wifi_error: None,
        }
    }
}

impl Dashboard {
    pub fn push_rate(&mut self, rates: Option<Rates>) {
        self.rates = rates;
        self.history.rotate_left(1);
        self.history[59] = rates;
    }

    pub fn stale(&self, now_ms: u64) -> bool {
        self.error.is_some()
            || self
                .last_sample_ms
                .is_none_or(|t| now_ms.saturating_sub(t) > 5000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ubus::json::json_to_blobmsg;

    fn counters(rx: u64, tx: u64) -> Counters {
        Counters {
            rx_bytes: Some(rx),
            tx_bytes: Some(tx),
            ..Default::default()
        }
    }

    #[test]
    fn measures_elapsed_time_with_64_bit_counters() {
        let mut tracker = RateTracker::default();
        let base = 1u64 << 40;
        assert_eq!(
            tracker.update("pppoe-wan", 100, 1000, counters(base, base)),
            None
        );
        assert_eq!(
            tracker.update("pppoe-wan", 102, 3500, counters(base + 25000, base + 5000)),
            Some(Rates {
                down: 10000,
                up: 2000
            })
        );
    }

    #[test]
    fn resets_on_device_change_reboot_counter_reset_and_missing_sample() {
        let mut tracker = RateTracker::default();
        for (device, uptime, now, counts) in [
            ("eth0", 100, 1000, counters(100, 200)),
            ("eth1", 101, 2000, counters(200, 300)),
            ("eth1", 1, 3000, counters(300, 400)),
            ("eth1", 2, 4000, counters(1, 2)),
            ("eth1", 3, 5000, Counters::default()),
            ("eth1", 4, 6000, counters(100, 200)),
            ("eth1", 4, 6000, counters(100, 200)),
        ] {
            assert_eq!(tracker.update(device, uptime, now, counts), None);
        }
        assert_eq!(
            tracker.update("eth1", 5, 7000, counters(1100, 2200)),
            Some(Rates {
                down: 1000,
                up: 2000
            })
        );
    }

    #[test]
    fn parses_openwrt_units_optional_fields_and_signed_wifi_signal() {
        let system = System::parse(
            &json_to_blobmsg(&json!({
                "uptime":1234,"load":[65536,32768,16384],
                "memory":{"total":536870912,"available":268435456},
                    "root":{"total":102400,"free":51200}
            }))
            .unwrap(),
        );
        assert_eq!(system.load, [Some(65536), Some(32768), Some(16384)]);
        assert_eq!(system.memory_available, Some(268435456));
        assert_eq!(system.root_total, Some(104857600));
        assert_eq!(system.root_free, Some(52428800));
        let wan = Wan::parse(
            &json_to_blobmsg(&json!({"up":true,"l3_device":"pppoe-wan",
            "ipv4-address":[{"address":"203.0.113.5","mask":32}]}))
            .unwrap(),
        );
        assert_eq!(wan.up, Some(true));
        assert_eq!(wan.device.as_deref(), Some("pppoe-wan"));
        assert_eq!(wan.address.as_deref(), Some("203.0.113.5"));
        for signal in [-56i64, 4294967240] {
            let mut wifi = Wifi::parse(&json_to_blobmsg(&json!({"signal":signal})).unwrap());
            assert_eq!(wifi.signal, Some(-56));
            wifi.set_clients(&json_to_blobmsg(&json!({"results":[{},{}]})).unwrap());
            assert_eq!(wifi.clients, Some(2));
        }
        let missing = System::parse(&BlobMsgTable::empty());
        assert_eq!(missing.memory_total, None);
        assert_eq!(missing.load, [None; 3]);
        assert_eq!(Wan::parse(&BlobMsgTable::empty()).up, None);
    }

    #[test]
    fn keeps_failures_visible_and_graph_gaps_explicit() {
        let mut dashboard = Dashboard {
            last_sample_ms: Some(1000),
            ..Default::default()
        };
        assert!(!dashboard.stale(6000));
        assert!(dashboard.stale(6001));
        dashboard.push_rate(Some(Rates { down: 10, up: 1 }));
        dashboard.push_rate(None);
        assert_eq!(dashboard.history[58], Some(Rates { down: 10, up: 1 }));
        assert_eq!(dashboard.history[59], None);
        dashboard.error = Some(Error::Timeout);
        assert!(dashboard.stale(1000));
    }
}
