//! RGB565 framebuffer model of a mythroad phone screen.

/// Screen width of the most common MRP phone variant (240×320 portrait).
pub const DEFAULT_WIDTH: u16 = 240;

/// Screen height of the most common MRP phone variant (240×320 portrait).
pub const DEFAULT_HEIGHT: u16 = 320;

/// Bitmap files consumed by applications that open the phone's font directly.
pub(crate) fn system_font(path: &str) -> Option<&'static [u8]> {
    use std::sync::OnceLock;
    static SMALL: OnceLock<Vec<u8>> = OnceLock::new();
    static MEDIUM: OnceLock<Vec<u8>> = OnceLock::new();
    match path {
        "system/gb12.uc2" => Some(SMALL.get_or_init(|| make_system_font(12))),
        "system/gb16.uc2" => Some(MEDIUM.get_or_init(|| make_system_font(16))),
        _ => None,
    }
}

fn make_system_font(size: usize) -> Vec<u8> {
    // UC2 stores an MSB-first cell at a fixed offset for each UCS-2 code point.
    let cell_bytes = size * size / 8;
    let mut data = vec![0; 65536 * cell_bytes];
    for (character, glyph) in unifont::enumerate_glyphs() {
        let code = character as usize;
        if code >= 65536 {
            break;
        }
        if character.is_control() || character.is_whitespace() {
            continue;
        }
        let cell = code * cell_bytes;
        let width = if code < 128 { size / 2 } else { size };
        let stride = width.max(8);
        for y in 0..size {
            for x in 0..width {
                if glyph.get_pixel(x * glyph.get_width() / width, y * 16 / size) {
                    let bit = y * stride + x;
                    data[cell + bit / 8] |= 0x80 >> (bit % 8);
                }
            }
        }
    }
    data
}

/// A 16-bit RGB565 framebuffer in row-major order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Screen {
    width: u16,
    height: u16,
    pixels: Vec<u16>,
}

impl Screen {
    /// Creates a screen of the given size, cleared to black.
    pub fn new(width: u16, height: u16) -> Self {
        assert!(
            width > 0 && height > 0,
            "screen dimensions must be non-zero"
        );
        Self {
            width,
            height,
            pixels: vec![0; usize::from(width) * usize::from(height)],
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// RGB565 pixel data in row-major order, one entry per pixel.
    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [u16] {
        &mut self.pixels
    }

    /// Fills the whole screen with one RGB565 color.
    pub fn fill(&mut self, color: u16) {
        self.pixels.fill(color);
    }

    /// Writes one pixel; returns `false` and does nothing when the
    /// coordinates are out of bounds.
    pub fn set_pixel(&mut self, x: u16, y: u16, color: u16) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.pixels[usize::from(y) * usize::from(self.width) + usize::from(x)] = color;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_screen_is_black_with_requested_size() {
        let screen = Screen::new(4, 3);

        assert_eq!(screen.width(), 4);
        assert_eq!(screen.height(), 3);
        assert_eq!(screen.pixels(), &[0u16; 12]);
    }

    #[test]
    fn fill_overwrites_every_pixel() {
        let mut screen = Screen::new(3, 3);
        screen.fill(0x39E7);

        assert_eq!(screen.pixels(), &[0x39E7u16; 9]);
    }

    #[test]
    fn set_pixel_is_bounds_checked() {
        let mut screen = Screen::new(4, 4);
        screen.fill(0x0000);

        assert!(screen.set_pixel(3, 2, 0xFFFF));
        assert_eq!(screen.pixels()[2 * 4 + 3], 0xFFFF);

        assert!(!screen.set_pixel(4, 2, 0xFFFF));
        assert!(!screen.set_pixel(2, 4, 0xFFFF));
        assert!(screen.pixels().iter().all(|&p| p == 0 || p == 0xFFFF));
    }
}
