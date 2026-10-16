//! Render the actual tiled dashboard to a PPM contact sheet on the host.
use core::convert::Infallible;
use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use serde_json::json;
use std::{
    fs::File,
    io::{BufWriter, Write},
};
use ubus::json::json_to_blobmsg;
use ubus_router_display::{
    panel::{Tile, TILE_ROWS, WIDTH},
    render,
    stats::{Board, Counters, Dashboard, Rates, System, Wan, Wifi},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "preview.ppm".into());
    let mut snapshot = Dashboard {
        board: Board::parse(&json_to_blobmsg(
            &json!({"hostname":"Beryl AX", "model":"GL.iNet GL-MT3000", "release":{"version":"24.10"}}),
        )?),
        system: System::parse(&json_to_blobmsg(
            &json!({"uptime":245820,"load":[24248,18350,12452],
            "memory":{"total":536870912,"available":335544320},"root":{"total":102400,"free":71680}}),
        )?),
        wan: Some(Wan::parse(&json_to_blobmsg(
            &json!({"up":true,"l3_device":"eth1", "proto":"dhcp",
            "uptime":240000,"ipv4-address":[{"address":"192.0.2.42"}]}),
        )?)),
        counters: Some(Counters::parse(&json_to_blobmsg(&json!({"statistics":{
            "rx_bytes":5368709120u64,"tx_bytes":1610612736u64,"rx_packets":4723198,"tx_packets":1849320,
            "rx_errors":0,"tx_errors":0,"rx_dropped":2,"tx_dropped":0}}))?)),
        last_sample_ms: Some(1000),
        ..Default::default()
    };
    let mut wifi = Wifi::parse(&json_to_blobmsg(
        &json!({"ssid":"Beryl home","channel":6,"signal":-48,"quality":62,"quality_max":70}),
    )?);
    wifi.clients = Some(7);
    snapshot.wifi = Some(wifi);
    for i in 0u64..60 {
        let down = 600_000 + (i * 7919 % 29) * 41_000;
        snapshot.push_rate(Some(Rates { down, up: down / 7 }));
    }

    let sheet_width = 256 * render::PAGE_COUNT;
    let mut image = vec![[7u8, 9, 14]; sheet_width * 256];
    let mut pixels = [Rgb565::BLACK; WIDTH * TILE_ROWS];
    for page in 0..render::PAGE_COUNT {
        for y in (0..240u16).step_by(TILE_ROWS) {
            let result: Result<(), Infallible> = render::draw(
                &mut Tile {
                    y,
                    pixels: &mut pixels,
                },
                &snapshot,
                page,
                1000,
            );
            result.unwrap();
            for (i, pixel) in pixels.iter().enumerate() {
                let (x, row) = (i % WIDTH, y as usize + i / WIDTH);
                if (x as i32 - 120).pow(2) + (row as i32 - 120).pow(2) >= 120 * 120 {
                    continue;
                }
                image[(row + 8) * sheet_width + page * 256 + x + 8] = [
                    (u16::from(pixel.r()) * 255 / 31) as u8,
                    (u16::from(pixel.g()) * 255 / 63) as u8,
                    (u16::from(pixel.b()) * 255 / 31) as u8,
                ];
            }
        }
    }
    let mut file = BufWriter::new(File::create(&output)?);
    write!(file, "P6\n{sheet_width} 256\n255\n")?;
    for pixel in image {
        file.write_all(&pixel)?;
    }
    file.flush()?;
    println!("Wrote {output} (illustrative data)");
    Ok(())
}
