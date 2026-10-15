//! GUITION ESP32-2424S012-V1.0 panel configuration from pawelchcki/razem-tag.
//! GC9A01 initialization values match that board, including BGR and inversion.

use core::convert::Infallible;
use embedded_graphics::{pixelcolor::Rgb565, prelude::*, primitives::Rectangle};
use embedded_hal::{delay::DelayNs, digital::OutputPin, spi::SpiBus};

pub const WIDTH: usize = 240;
pub const TILE_ROWS: usize = 16;

pub struct Panel<SPI, DC> {
    spi: SPI,
    dc: DC,
}

impl<SPI: SpiBus, DC: OutputPin<Error = Infallible>> Panel<SPI, DC> {
    pub fn new(spi: SPI, dc: DC) -> Self {
        Self { spi, dc }
    }

    fn command(&mut self, command: u8, data: &[u8]) -> Result<(), SPI::Error> {
        self.dc.set_low().unwrap();
        self.spi.write(&[command])?;
        self.spi.flush()?;
        self.dc.set_high().unwrap();
        self.spi.write(data)?;
        self.spi.flush()
    }

    pub fn init(&mut self, delay: &mut impl DelayNs) -> Result<(), SPI::Error> {
        self.command(0x01, &[])?; // Software reset: no LCD reset GPIO on this board.
        delay.delay_ms(150);
        for (cmd, data) in [
            (0xEF, &[][..]),
            (0xEB, &[0x14]),
            (0xFE, &[]),
            (0xEF, &[]),
            (0xEB, &[0x14]),
            (0x84, &[0x40]),
            (0x85, &[0xFF]),
            (0x86, &[0xFF]),
            (0x87, &[0xFF]),
            (0x88, &[0x0A]),
            (0x89, &[0x21]),
            (0x8A, &[0x00]),
            (0x8B, &[0x80]),
            (0x8C, &[0x01]),
            (0x8D, &[0x01]),
            (0x8E, &[0xFF]),
            (0x8F, &[0xFF]),
            (0xB6, &[0x00, 0x20]),
            (0x36, &[0x08]),
            (0x3A, &[0x55]),
            (0x90, &[0x08, 0x08, 0x08, 0x08]),
            (0xBD, &[0x06]),
            (0xBC, &[0x00]),
            (0xFF, &[0x60, 0x01, 0x04]),
            (0xC3, &[0x13]),
            (0xC4, &[0x13]),
            (0xC9, &[0x22]),
            (0xBE, &[0x11]),
            (0xE1, &[0x10, 0x0E]),
            (0xDF, &[0x20, 0x0C, 0x02]),
            (0xF0, &[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
            (0xF1, &[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
            (0xF2, &[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
            (0xF3, &[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
            (0xED, &[0x18, 0x0B]),
            (0xAE, &[0x77]),
            (0xCD, &[0x63]),
            (
                0x70,
                &[0x07, 0x07, 0x04, 0x0E, 0x0F, 0x09, 0x07, 0x08, 0x03],
            ),
            (0xE8, &[0x34]),
            (
                0x62,
                &[
                    0x18, 0x0D, 0x71, 0xED, 0x70, 0x70, 0x18, 0x0F, 0x71, 0xEF, 0x70, 0x70,
                ],
            ),
            (
                0x63,
                &[
                    0x18, 0x11, 0x71, 0xF1, 0x70, 0x70, 0x18, 0x13, 0x71, 0xF3, 0x70, 0x70,
                ],
            ),
            (0x64, &[0x28, 0x29, 0xF1, 0x01, 0xF1, 0x00, 0x07]),
            (
                0x66,
                &[0x3C, 0x00, 0xCD, 0x67, 0x45, 0x45, 0x10, 0x00, 0x00, 0x00],
            ),
            (
                0x67,
                &[0x00, 0x3C, 0x00, 0x00, 0x00, 0x01, 0x54, 0x10, 0x32, 0x98],
            ),
            (0x74, &[0x10, 0x85, 0x80, 0x00, 0x00, 0x4E, 0x00]),
            (0x98, &[0x3E, 0x07]),
            (0x21, &[]),
            (0x11, &[]),
        ] {
            self.command(cmd, data)?;
        }
        delay.delay_ms(120);
        self.command(0x29, &[])
    }

    pub fn write_tile(
        &mut self,
        y: u16,
        pixels: &[Rgb565; WIDTH * TILE_ROWS],
    ) -> Result<(), SPI::Error> {
        self.command(0x2A, &[0, 0, 0, 239])?;
        let end = y + TILE_ROWS as u16 - 1;
        self.command(
            0x2B,
            &[(y >> 8) as u8, y as u8, (end >> 8) as u8, end as u8],
        )?;
        self.command(0x2C, &[])?;
        let mut bytes = [0u8; WIDTH * 2];
        for row in pixels.chunks_exact(WIDTH) {
            for (pixel, bytes) in row.iter().zip(bytes.chunks_exact_mut(2)) {
                bytes.copy_from_slice(&pixel.into_storage().to_be_bytes());
            }
            self.spi.write(&bytes)?;
        }
        self.spi.flush()
    }
}

/// Clip drawing to 16 rows, using 7.5 KiB instead of a 112.5 KiB framebuffer.
pub struct Tile<'a> {
    pub y: u16,
    pub pixels: &'a mut [Rgb565; WIDTH * TILE_ROWS],
}

impl OriginDimensions for Tile<'_> {
    fn size(&self) -> Size {
        Size::new(240, 240)
    }
}

impl DrawTarget for Tile<'_> {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Infallible> {
        for Pixel(p, color) in pixels {
            if (0..240).contains(&p.x)
                && (i32::from(self.y)..i32::from(self.y) + TILE_ROWS as i32).contains(&p.y)
            {
                self.pixels[(p.y - i32::from(self.y)) as usize * WIDTH + p.x as usize] = color;
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Rgb565) -> Result<(), Infallible> {
        let clip = area.intersection(&Rectangle::new(
            Point::new(0, self.y.into()),
            Size::new(240, TILE_ROWS as u32),
        ));
        for p in clip.points() {
            self.pixels[(p.y - i32::from(self.y)) as usize * WIDTH + p.x as usize] = color;
        }
        Ok(())
    }
}
