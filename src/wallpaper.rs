use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// A decoded image in `0x00RRGGBB` pixels.
pub struct Wallpaper {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Wallpaper {
    pub fn load_png(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let mut decoder = png::Decoder::new(BufReader::new(file));
        decoder.set_transformations(png::Transformations::normalize_to_color8());

        let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
        let size = reader.output_buffer_size().ok_or("image is too large")?;
        let mut buffer = vec![0; size];
        let frame = reader
            .next_frame(&mut buffer)
            .map_err(|error| error.to_string())?;
        let bytes = buffer
            .get(..frame.buffer_size())
            .ok_or("decoder returned a short frame")?;

        let pixels: Vec<u32> = match frame.color_type {
            png::ColorType::Rgb => bytes
                .as_chunks()
                .0
                .iter()
                .map(|&[red, green, blue]| pack([red, green, blue], 255))
                .collect(),
            png::ColorType::Rgba => bytes
                .as_chunks()
                .0
                .iter()
                .map(|&[red, green, blue, alpha]| pack([red, green, blue], alpha))
                .collect(),
            png::ColorType::Grayscale => bytes.iter().map(|&gray| pack([gray; 3], 255)).collect(),
            png::ColorType::GrayscaleAlpha => bytes
                .as_chunks()
                .0
                .iter()
                .map(|&[gray, alpha]| pack([gray; 3], alpha))
                .collect(),
            png::ColorType::Indexed => return Err(String::from("indexed image was not expanded")),
        };

        let width = usize::try_from(frame.width).map_err(|error| error.to_string())?;
        let height = usize::try_from(frame.height).map_err(|error| error.to_string())?;
        if width == 0 || height == 0 || pixels.len() != width * height {
            return Err(String::from("image has an unexpected size"));
        }

        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// Scales the image to cover `width` by `height`, cropping the overhang evenly.
    pub fn cover(&self, width: usize, height: usize) -> Vec<u32> {
        let scale = (width as f64 / self.width as f64).max(height as f64 / self.height as f64);
        let offset_x = (self.width as f64 * scale - width as f64) / 2.;
        let offset_y = (self.height as f64 * scale - height as f64) / 2.;

        let columns: Vec<Sample> = (0..width)
            .map(|x| Sample::new((x as f64 + 0.5 + offset_x) / scale - 0.5, self.width))
            .collect();

        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            let row = Sample::new((y as f64 + 0.5 + offset_y) / scale - 0.5, self.height);
            let upper = self.row(row.first);
            let lower = self.row(row.second);

            for column in &columns {
                let top = blend(column.get(upper), column.get_second(upper), column.weight);
                let bottom = blend(column.get(lower), column.get_second(lower), column.weight);
                pixels.push(blend(top, bottom, row.weight));
            }
        }
        pixels
    }

    fn row(&self, y: usize) -> &[u32] {
        let start = y * self.width;
        self.pixels
            .get(start..start + self.width)
            .unwrap_or_default()
    }
}

/// Two neighboring source positions and how much the second one contributes.
struct Sample {
    first: usize,
    second: usize,
    weight: f64,
}

impl Sample {
    fn new(position: f64, length: usize) -> Self {
        let last = length.saturating_sub(1);
        let position = position.clamp(0., last as f64);
        let first = position.floor() as usize;
        Self {
            first,
            second: (first + 1).min(last),
            weight: position - first as f64,
        }
    }

    fn get(&self, row: &[u32]) -> u32 {
        row.get(self.first).copied().unwrap_or(0)
    }

    fn get_second(&self, row: &[u32]) -> u32 {
        row.get(self.second).copied().unwrap_or(0)
    }
}

pub fn solid(color: u32, width: usize, height: usize) -> Vec<u32> {
    vec![color; width * height]
}

/// Packs a color into a pixel, blending it over black by `alpha`.
fn pack([red, green, blue]: [u8; 3], alpha: u8) -> u32 {
    let channel = |value: u8| u32::from(value) * u32::from(alpha) / 255;
    channel(red) << 16 | channel(green) << 8 | channel(blue)
}

fn blend(first: u32, second: u32, weight: f64) -> u32 {
    let mix = |shift: u32| {
        let a = f64::from((first >> shift) & 0xff);
        let b = f64::from((second >> shift) & 0xff);
        ((a + (b - a) * weight).round() as u32).min(255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallpaper(width: usize, height: usize, pixels: Vec<u32>) -> Wallpaper {
        Wallpaper {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn cover_keeps_an_identical_size() {
        let pixels = vec![0x000000, 0xff0000, 0x00ff00, 0x0000ff];

        assert_eq!(wallpaper(2, 2, pixels.clone()).cover(2, 2), pixels);
    }

    #[test]
    fn cover_crops_the_longer_side() {
        let image = wallpaper(4, 1, vec![0x111111, 0x222222, 0x333333, 0x444444]);

        assert_eq!(image.cover(2, 1), vec![0x222222, 0x333333]);
    }

    #[test]
    fn cover_fills_every_pixel() {
        let image = wallpaper(3, 2, vec![0xffffff; 6]);

        assert_eq!(image.cover(7, 5), vec![0xffffff; 35]);
    }

    #[test]
    fn pack_blends_over_black() {
        assert_eq!(pack([255, 128, 0], 255), 0xff8000);
        assert_eq!(pack([255, 255, 255], 0), 0);
    }
}
