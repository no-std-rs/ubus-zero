//! 240×240 round-screen layout. Can draw into a small tile or a host preview.

use crate::stats::Dashboard;
use alloc::{format, string::String};
use embedded_graphics::{
    mono_font::{
        ascii::{FONT_6X10, FONT_9X18_BOLD},
        MonoTextStyle,
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

pub const PAGE_COUNT: usize = 5;
pub const BACKGROUND: Rgb565 = Rgb565::new(1, 3, 6);
const WHITE: Rgb565 = Rgb565::new(29, 59, 30);
const MUTED: Rgb565 = Rgb565::new(14, 31, 18);
const DOWN: Rgb565 = Rgb565::new(6, 57, 23);
const UP: Rgb565 = Rgb565::new(31, 35, 9);
const PINK: Rgb565 = Rgb565::new(29, 14, 23);

fn short(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    if s.chars().count() > max && max >= 3 {
        out.truncate(max - 3);
        out.push_str("...");
    }
    out
}

fn text<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    s: &str,
    x: i32,
    y: i32,
    color: Rgb565,
    align: Alignment,
) -> Result<(), D::Error> {
    Text::with_alignment(
        s,
        Point::new(x, y),
        MonoTextStyle::new(&FONT_6X10, color),
        align,
    )
    .draw(d)?;
    Ok(())
}

fn center<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    s: &str,
    y: i32,
    color: Rgb565,
) -> Result<(), D::Error> {
    text(d, &short(s, 30), 120, y, color, Alignment::Center)
}

fn row<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    label: &str,
    value: &str,
    y: i32,
) -> Result<(), D::Error> {
    text(d, label, 32, y, MUTED, Alignment::Left)?;
    text(d, &short(value, 21), 208, y, WHITE, Alignment::Right)
}

fn large<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    s: &str,
    y: i32,
    color: Rgb565,
) -> Result<(), D::Error> {
    Text::with_alignment(
        s,
        Point::new(120, y),
        MonoTextStyle::new(&FONT_9X18_BOLD, color),
        Alignment::Center,
    )
    .draw(d)?;
    Ok(())
}

pub fn megabits(bytes_per_second: u64) -> String {
    let hundredths = u128::from(bytes_per_second) * 8 / 10_000;
    format!("{}.{:02} Mb/s", hundredths / 100, hundredths % 100)
}

pub fn setup<D: DrawTarget<Color = Rgb565>>(d: &mut D) -> Result<(), D::Error> {
    d.clear(BACKGROUND)?;
    center(d, "UBUS ZERO", 55, PINK)?;
    center(d, "SETUP REQUIRED", 92, WHITE)?;
    center(d, "Open the USB flasher", 119, MUTED)?;
    center(d, "on your computer.", 136, MUTED)?;
    center(d, "Enter Wi-Fi and router", 162, WHITE)?;
    center(d, "settings, then install.", 179, WHITE)
}

fn bytes(n: Option<u64>) -> String {
    match n {
        Some(n) if n >= 1024 * 1024 * 1024 => format!(
            "{}.{:01} GiB",
            n / (1 << 30),
            (n % (1 << 30)) * 10 / (1 << 30)
        ),
        Some(n) => format!("{} MiB", n / (1 << 20)),
        None => String::from("--"),
    }
}

fn count(n: Option<u64>) -> String {
    n.map(|n| format!("{n}"))
        .unwrap_or_else(|| String::from("--"))
}

fn uptime(n: Option<u64>) -> String {
    n.map(|n| format!("{}d {:02}h {:02}m", n / 86400, n / 3600 % 24, n / 60 % 60))
        .unwrap_or_else(|| String::from("--"))
}

fn load(n: Option<u64>) -> String {
    n.map(|n| format!("{}.{:02}", n / 65536, u128::from(n % 65536) * 100 / 65536))
        .unwrap_or_else(|| String::from("--"))
}

pub fn draw<D: DrawTarget<Color = Rgb565>>(
    d: &mut D,
    s: &Dashboard,
    page: usize,
    now_ms: u64,
) -> Result<(), D::Error> {
    d.clear(BACKGROUND)?;
    let page = page % PAGE_COUNT;
    let title = ["WAN TRAFFIC", "SYSTEM", "WI-FI", "LINK COUNTERS", "ROUTER"][page];
    center(d, title, 28, PINK)?;
    Line::new(Point::new(62, 36), Point::new(178, 36))
        .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
        .draw(d)?;

    match page {
        0 => {
            center(d, "DOWNLOAD", 52, MUTED)?;
            large(
                d,
                &s.rates
                    .map(|r| megabits(r.down))
                    .unwrap_or_else(|| "-- Mb/s".into()),
                73,
                DOWN,
            )?;
            center(d, "UPLOAD", 92, MUTED)?;
            large(
                d,
                &s.rates
                    .map(|r| megabits(r.up))
                    .unwrap_or_else(|| "-- Mb/s".into()),
                113,
                UP,
            )?;
            let scale = s
                .history
                .iter()
                .flatten()
                .map(|r| r.down.max(r.up))
                .max()
                .unwrap_or(0)
                .max(125_000);
            let height = |n: u64| (u128::from(n) * 42 / u128::from(scale)) as i32;
            for i in 1..60 {
                if let (Some(a), Some(b)) = (s.history[i - 1], s.history[i]) {
                    let x = 32 + i as i32 * 3;
                    for (a, b, color) in [(a.down, b.down, DOWN), (a.up, b.up, UP)] {
                        Line::new(
                            Point::new(x - 3, 170 - height(a)),
                            Point::new(x, 170 - height(b)),
                        )
                        .into_styled(PrimitiveStyle::with_stroke(color, 1))
                        .draw(d)?;
                    }
                }
            }
            Line::new(Point::new(32, 171), Point::new(209, 171))
                .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
                .draw(d)?;
            center(d, "LAST 60 SAMPLES", 185, MUTED)?;
        }
        1 => {
            center(d, "LOAD  1 / 5 / 15 MIN", 55, MUTED)?;
            center(
                d,
                &format!(
                    "{} / {} / {}",
                    load(s.system.load[0]),
                    load(s.system.load[1]),
                    load(s.system.load[2])
                ),
                73,
                WHITE,
            )?;
            let used = s
                .system
                .memory_total
                .zip(s.system.memory_available)
                .map(|(t, a)| t.saturating_sub(a));
            row(
                d,
                "RAM",
                &format!("{} / {}", bytes(used), bytes(s.system.memory_total)),
                100,
            )?;
            if let Some((used, total)) = used.zip(s.system.memory_total).filter(|(_, t)| *t != 0) {
                Rectangle::new(Point::new(36, 109), Size::new(168, 5))
                    .into_styled(PrimitiveStyle::with_fill(MUTED))
                    .draw(d)?;
                let w = (u128::from(used.min(total)) * 168 / u128::from(total)) as u32;
                Rectangle::new(Point::new(36, 109), Size::new(w, 5))
                    .into_styled(PrimitiveStyle::with_fill(DOWN))
                    .draw(d)?;
            }
            row(d, "ROOT FREE", &bytes(s.system.root_free), 140)?;
            row(d, "ROOT SIZE", &bytes(s.system.root_total), 159)?;
            center(d, &uptime(s.system.uptime), 183, WHITE)?;
        }
        2 => {
            if let Some(w) = &s.wifi {
                center(
                    d,
                    &short(w.ssid.as_deref().unwrap_or("Unknown SSID"), 27),
                    58,
                    WHITE,
                )?;
                row(d, "CHANNEL", &count(w.channel), 84)?;
                row(
                    d,
                    "SIGNAL",
                    &w.signal
                        .map(|s| format!("{s} dBm"))
                        .unwrap_or_else(|| "--".into()),
                    108,
                )?;
                row(
                    d,
                    "QUALITY",
                    &w.quality
                        .zip(w.quality_max)
                        .filter(|(_, max)| *max > 0)
                        .map(|(q, max)| {
                            format!("{}%", u128::from(q.min(max)) * 100 / u128::from(max))
                        })
                        .unwrap_or_else(|| "--".into()),
                    132,
                )?;
                row(
                    d,
                    "CLIENTS",
                    &w.clients
                        .map(|n| format!("{n}"))
                        .unwrap_or_else(|| "--".into()),
                    156,
                )?;
                center(d, "SELECTED RADIO / AP", 182, MUTED)?;
            } else {
                center(d, "Wi-Fi stats unavailable", 91, WHITE)?;
                center(
                    d,
                    s.wifi_error
                        .map(|e| e.label())
                        .unwrap_or("Set WIFI_DEVICE at build"),
                    118,
                    MUTED,
                )?;
                center(d, "Other pages stay live", 146, MUTED)?;
            }
        }
        3 => {
            center(d, "RECEIVED / SENT", 56, MUTED)?;
            let c = s.counters.unwrap_or_default();
            row(
                d,
                "TOTAL",
                &format!("{} / {}", bytes(c.rx_bytes), bytes(c.tx_bytes)),
                82,
            )?;
            center(d, "PACKETS", 105, MUTED)?;
            center(
                d,
                &format!("{} / {}", count(c.rx_packets), count(c.tx_packets)),
                122,
                WHITE,
            )?;
            row(
                d,
                "ERRORS",
                &format!("{} / {}", count(c.rx_errors), count(c.tx_errors)),
                149,
            )?;
            row(
                d,
                "DROPS",
                &format!("{} / {}", count(c.rx_dropped), count(c.tx_dropped)),
                173,
            )?;
        }
        _ => {
            center(
                d,
                &short(s.board.hostname.as_deref().unwrap_or("OpenWrt"), 26),
                56,
                WHITE,
            )?;
            center(
                d,
                &short(s.board.model.as_deref().unwrap_or("Unknown board"), 30),
                76,
                MUTED,
            )?;
            row(
                d,
                "VERSION",
                s.board.release.as_deref().unwrap_or("--"),
                102,
            )?;
            row(
                d,
                "WAN",
                s.wan
                    .as_ref()
                    .and_then(|w| w.up)
                    .map(|up| if up { "UP" } else { "DOWN" })
                    .unwrap_or("--"),
                122,
            )?;
            row(
                d,
                "DEVICE",
                s.wan
                    .as_ref()
                    .and_then(|w| w.device.as_deref())
                    .unwrap_or("--"),
                142,
            )?;
            center(
                d,
                s.wan
                    .as_ref()
                    .and_then(|w| w.address.as_deref())
                    .unwrap_or("No WAN IPv4 address"),
                162,
                WHITE,
            )?;
            center(
                d,
                &uptime(s.wan.as_ref().and_then(|w| w.uptime)),
                182,
                MUTED,
            )?;
        }
    }

    let stale = s.stale(now_ms);
    let page_error = match page {
        2 => s.wifi_error,
        0 | 3 | 4 => s.wan_error,
        _ => None,
    };
    let status = if let Some(error) = s.error {
        error.label()
    } else if stale {
        if s.last_sample_ms.is_none() {
            "CONNECTING..."
        } else {
            "STALE - RECONNECTING"
        }
    } else if let Some(error) = page_error {
        error.label()
    } else if matches!(page, 0 | 3 | 4) && s.wan.as_ref().and_then(|w| w.up) == Some(false) {
        "WAN DOWN"
    } else {
        "LIVE"
    };
    let color = if status == "LIVE" { DOWN } else { UP };
    center(d, &short(status, 24), 202, color)?;
    // A slow pulse shows that the UI still runs during network retries.
    if status == "LIVE" && (now_ms / 500).is_multiple_of(2) {
        Circle::new(Point::new(97, 196), 4)
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(d)?;
    }
    for i in 0..PAGE_COUNT {
        Circle::new(Point::new(91 + i as i32 * 13, 215), 5)
            .into_styled(PrimitiveStyle::with_fill(if i == page {
                PINK
            } else {
                MUTED
            }))
            .draw(d)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::mock_display::MockDisplay;

    #[test]
    fn rates_use_decimal_megabits_and_do_not_overflow() {
        assert_eq!(megabits(125_000), "1.00 Mb/s");
        assert_eq!(megabits(0), "0.00 Mb/s");
        assert!(megabits(u64::MAX).ends_with("Mb/s"));
    }

    #[test]
    fn every_page_handles_absent_stats_and_unicode_names() {
        let mut d = MockDisplay::new();
        d.set_allow_out_of_bounds_drawing(true);
        d.set_allow_overdraw(true);
        let mut s = Dashboard::default();
        s.board.hostname = Some("router\n日本語".into());
        for page in 0..PAGE_COUNT {
            draw(&mut d, &s, page, 0).unwrap();
        }
    }
}
