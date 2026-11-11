use std::ops::Range;
use std::time::{Duration, Instant};

/// Ends the verifying look if no authentication result ever arrives.
const VERIFYING_HOLD: Duration = Duration::from_secs(10);

/// Random placements tried per band or block, keeping the most visible one.
const CANDIDATES: usize = 8;

/// The glitch shown over the background, advanced one frame per key press.
pub struct Glitch {
    frame: u64,
    seed: u64,
    look: Look,
    hold_times: HoldTimes,
    clear_at: Option<Instant>,
}

/// How long each look stays before the background returns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HoldTimes {
    pub typing: Duration,
    pub rejected: Duration,
    pub cleared: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Look {
    Clean,
    Typing(Kind),
    Verifying,
    Rejected,
    Cleared,
}

/// A color cast over the distorted parts of a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tint {
    None,
    Magenta,
    Green,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Bands,
    Fringe,
    Blocks,
}

const KINDS: [Kind; 3] = [Kind::Bands, Kind::Fringe, Kind::Blocks];

/// One distortion of the background. All offsets are in pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Shifts whole rows sideways and splits their color channels apart.
    Band {
        rows: Range<usize>,
        shift: isize,
        split: isize,
        tint: Tint,
    },
    /// Splits the color channels of the whole image, giving every edge colored outlines.
    Fringe {
        split_x: isize,
        split_y: isize,
        tint: Tint,
    },
    /// Moves a rectangle of the image by an offset.
    Block {
        rows: Range<usize>,
        columns: Range<usize>,
        shift_x: isize,
        shift_y: isize,
    },
}

impl Glitch {
    pub fn new(seed: u64, hold_times: HoldTimes) -> Self {
        Self {
            frame: 0,
            seed,
            look: Look::Clean,
            hold_times,
            clear_at: None,
        }
    }

    /// Shows the next typing frame, using a different kind than the frame before.
    pub fn advance(&mut self, now: Instant) {
        let previous = match self.look {
            Look::Typing(kind) => Some(kind),
            _ => None,
        };
        let mut random = self.random_for_frame(self.frame.wrapping_add(1));
        let index = random.below(KINDS.len());
        let mut kind = KINDS.get(index).copied().unwrap_or(Kind::Bands);
        if Some(kind) == previous {
            kind = KINDS
                .get((index + 1) % KINDS.len())
                .copied()
                .unwrap_or(Kind::Bands);
        }

        self.show(Look::Typing(kind), now + self.hold_times.typing);
    }

    /// Shows a short green frame after the password was cleared.
    pub fn cleared(&mut self, now: Instant) {
        self.show(Look::Cleared, now + self.hold_times.cleared);
    }

    /// Shows the tinted frame while a password is checked.
    pub fn verify(&mut self, now: Instant) {
        self.show(Look::Verifying, now + VERIFYING_HOLD);
    }

    /// Shows a new tinted frame after a failed check.
    pub fn reject(&mut self, now: Instant) {
        self.show(Look::Rejected, now + self.hold_times.rejected);
    }

    fn show(&mut self, look: Look, clear_at: Instant) {
        self.frame = self.frame.wrapping_add(1);
        self.look = look;
        self.clear_at = Some(clear_at);
    }

    pub fn clear(&mut self) {
        self.look = Look::Clean;
        self.clear_at = None;
    }

    pub fn clear_at(&self) -> Option<Instant> {
        self.clear_at
    }

    /// Returns to the background once the hold time is over, returning `true` if it did.
    pub fn clear_if_due(&mut self, now: Instant) -> bool {
        let is_due = self.clear_at.is_some_and(|clear_at| clear_at <= now);
        if is_due {
            self.clear();
        }
        is_due
    }

    /// The effects of the current frame on `background`, which is `width` pixels wide.
    pub fn effects(&self, background: &[u32], width: usize) -> Vec<Effect> {
        let height = background.len() / width.max(1);
        if width < 2 || height < 2 {
            return Vec::new();
        }

        let image = Image {
            pixels: background,
            width,
            height,
        };
        let mut random = self.random_for_frame(self.frame).skip_kind_choice();
        match self.look {
            Look::Clean => Vec::new(),
            Look::Typing(Kind::Bands) => bands(&image, &mut random, 0.5, Tint::None),
            Look::Typing(Kind::Fringe) => vec![fringe(&image, &mut random, 1., Tint::None)],
            Look::Typing(Kind::Blocks) => blocks(&image, &mut random),
            Look::Verifying | Look::Rejected => tinted_frame(&image, &mut random, Tint::Magenta),
            Look::Cleared => tinted_frame(&image, &mut random, Tint::Green),
        }
    }

    fn random_for_frame(&self, frame: u64) -> Random {
        Random::new(splitmix(self.seed ^ splitmix(frame)))
    }
}

impl Effect {
    /// The rows this effect changes.
    pub fn rows(&self, height: usize) -> Range<usize> {
        match self {
            Effect::Band { rows, .. } | Effect::Block { rows, .. } => rows.clone(),
            Effect::Fringe { .. } => 0..height,
        }
    }

    /// Draws the effect into `target` using the untouched `background` as source.
    pub fn draw(&self, background: &[u32], target: &mut [u32], width: usize) {
        let image = Image {
            pixels: background,
            width,
            height: background.len() / width.max(1),
        };

        match self {
            Effect::Band {
                rows,
                shift,
                split,
                tint,
            } => {
                for_each_pixel(target, width, rows.clone(), 0..width, |x, y| {
                    let x = x - shift;
                    split_channels(&image, (x - split, y), (x, y), (x + split, y), *tint)
                });
            }
            Effect::Fringe {
                split_x,
                split_y,
                tint,
            } => {
                for_each_pixel(target, width, 0..image.height, 0..width, |x, y| {
                    let red = (x - split_x, y - split_y);
                    let blue = (x + split_x, y + split_y);
                    split_channels(&image, red, (x, y), blue, *tint)
                });
            }
            Effect::Block {
                rows,
                columns,
                shift_x,
                shift_y,
            } => {
                for_each_pixel(target, width, rows.clone(), columns.clone(), |x, y| {
                    image.sample(x - shift_x, y - shift_y)
                });
            }
        }
    }
}

/// Copies `rows` from `background` back into `target`.
pub fn restore_rows(background: &[u32], target: &mut [u32], width: usize, rows: Range<usize>) {
    let range = rows.start * width..rows.end * width;
    if let (Some(source), Some(destination)) =
        (background.get(range.clone()), target.get_mut(range))
    {
        destination.copy_from_slice(source);
    }
}

/// A strong fringe with bands on top, all cast in `tint`.
fn tinted_frame(image: &Image, random: &mut Random, tint: Tint) -> Vec<Effect> {
    let mut effects = vec![fringe(image, random, 1.5, tint)];
    effects.extend(bands(image, random, 0.8, tint));
    effects
}

fn bands(image: &Image, random: &mut Random, intensity: f64, tint: Tint) -> Vec<Effect> {
    let count = 1 + (intensity * 6.) as usize;
    let (width, height) = (image.width as f64, image.height);

    (0..count)
        .filter_map(|_| {
            let band_fraction = random.between(0.01, 0.06) * (0.5 + intensity);
            let band_height = ((height as f64 * band_fraction) as usize).clamp(2, height);
            let shift = random.magnitude(width * 0.04 * intensity) as isize;
            let split = (width * 0.006 * intensity).max(1.) as isize;

            (0..CANDIDATES)
                .map(|_| {
                    let top = random.below(height - band_height + 1);
                    top..top + band_height
                })
                .max_by_key(|rows| visible_change(image, rows, &(0..image.width), shift, 0))
                .map(|rows| Effect::Band {
                    rows,
                    shift,
                    split,
                    tint,
                })
        })
        .collect()
}

fn fringe(image: &Image, random: &mut Random, intensity: f64, tint: Tint) -> Effect {
    let split = image.width as f64 * random.between(0.003, 0.006) * intensity;
    let direction = if random.below(2) == 0 { -1. } else { 1. };

    Effect::Fringe {
        split_x: (split * direction).round() as isize,
        split_y: (split * random.between(-0.5, 0.5)).round() as isize,
        tint,
    }
}

/// Moves a few rectangles, each placed where the move is most visible.
fn blocks(image: &Image, random: &mut Random) -> Vec<Effect> {
    let count = 3 + random.below(4);
    let (width, height) = (image.width as f64, image.height as f64);

    (0..count)
        .filter_map(|_| {
            let block_width = ((width * random.between(0.08, 0.25)) as usize).clamp(1, image.width);
            let block_height =
                ((height * random.between(0.02, 0.06)) as usize).clamp(1, image.height);
            let shift_x = random.magnitude(width * 0.03) as isize;
            let shift_y = (random.between(-1., 1.) * height * 0.01) as isize;

            (0..CANDIDATES)
                .map(|_| {
                    let left = random.below(image.width - block_width + 1);
                    let top = random.below(image.height - block_height + 1);
                    (top..top + block_height, left..left + block_width)
                })
                .max_by_key(|(rows, columns)| {
                    visible_change(image, rows, columns, shift_x, shift_y)
                })
                .map(|(rows, columns)| Effect::Block {
                    rows,
                    columns,
                    shift_x,
                    shift_y,
                })
        })
        .collect()
}

/// Estimates how many pixels moving an area changes by checking a grid of samples.
fn visible_change(
    image: &Image,
    rows: &Range<usize>,
    columns: &Range<usize>,
    shift_x: isize,
    shift_y: isize,
) -> usize {
    const GRID: usize = 8;

    let mut changed = 0;
    for row_step in 0..GRID {
        for column_step in 0..GRID {
            let y = (rows.start + rows.len() * row_step / GRID) as isize;
            let x = (columns.start + columns.len() * column_step / GRID) as isize;
            if image.sample(x, y) != image.sample(x - shift_x, y - shift_y) {
                changed += 1;
            }
        }
    }
    changed
}

fn split_channels(
    image: &Image,
    red: (isize, isize),
    green: (isize, isize),
    blue: (isize, isize),
    tint: Tint,
) -> u32 {
    let red = image.sample(red.0, red.1) & 0xff0000;
    let green = image.sample(green.0, green.1) & 0x00ff00;
    let blue = image.sample(blue.0, blue.1) & 0x0000ff;
    tint.apply(red | green | blue)
}

impl Tint {
    /// Halves the channels that do not belong to the tint color.
    fn apply(self, pixel: u32) -> u32 {
        match self {
            Tint::None => pixel,
            Tint::Magenta => (pixel & 0xff00ff) | ((pixel >> 1) & 0x007f00),
            Tint::Green => ((pixel >> 1) & 0x7f007f) | (pixel & 0x00ff00),
        }
    }
}

/// Sets every pixel of `target` inside `rows` and `columns` to `color(x, y)`.
fn for_each_pixel(
    target: &mut [u32],
    width: usize,
    rows: Range<usize>,
    columns: Range<usize>,
    color: impl Fn(isize, isize) -> u32,
) {
    for y in rows {
        let Some(row) = target.get_mut(y * width..(y + 1) * width) else {
            continue;
        };
        let Some(pixels) = row.get_mut(columns.clone()) else {
            continue;
        };
        for (x, pixel) in (columns.start..).zip(pixels) {
            *pixel = color(x as isize, y as isize);
        }
    }
}

struct Image<'a> {
    pixels: &'a [u32],
    width: usize,
    height: usize,
}

impl Image<'_> {
    /// The pixel at `(x, y)`, clamped to the image edges.
    fn sample(&self, x: isize, y: isize) -> u32 {
        let clamp = |value: isize, length: usize| {
            usize::try_from(value)
                .unwrap_or(0)
                .min(length.saturating_sub(1))
        };
        let index = clamp(y, self.height) * self.width + clamp(x, self.width);
        self.pixels.get(index).copied().unwrap_or(0)
    }
}

/// Scrambles a value so that nearby inputs give unrelated outputs.
fn splitmix(value: u64) -> u64 {
    let mut value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// A xorshift generator, plenty for visual noise.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// Skips the value `Glitch::advance` used to pick the kind, so the layout is independent of it.
    fn skip_kind_choice(mut self) -> Self {
        self.next();
        self
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn between(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    /// A value with random sign and a magnitude between 40% and 100% of `maximum`.
    fn magnitude(&mut self, maximum: f64) -> f64 {
        let sign = if self.below(2) == 0 { -1. } else { 1. };
        sign * self.between(0.4, 1.) * maximum
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glitch(seed: u64) -> Glitch {
        Glitch::new(
            seed,
            HoldTimes {
                typing: Duration::from_millis(100),
                rejected: Duration::from_millis(600),
                cleared: Duration::from_millis(250),
            },
        )
    }

    #[test]
    fn clearing_shows_a_short_green_frame() {
        let now = Instant::now();
        let background = subject(200, 200);
        let mut glitch = glitch(4);

        glitch.cleared(now);

        let effects = glitch.effects(&background, 200);
        assert!(effects.iter().all(|effect| match effect {
            Effect::Band { tint, .. } | Effect::Fringe { tint, .. } => *tint == Tint::Green,
            Effect::Block { .. } => false,
        }));
        assert!(!glitch.clear_if_due(now + Duration::from_millis(249)));
        assert!(glitch.clear_if_due(now + Duration::from_millis(250)));
    }

    #[test]
    fn tints_halve_the_other_channels() {
        assert_eq!(Tint::None.apply(0xffffff), 0xffffff);
        assert_eq!(Tint::Magenta.apply(0xffffff), 0xff7fff);
        assert_eq!(Tint::Green.apply(0xffffff), 0x7fff7f);
    }

    /// A dark image with a bright square in the middle, like a subject on a plain background.
    fn subject(width: usize, height: usize) -> Vec<u32> {
        (0..width * height)
            .map(|index| {
                let (x, y) = (index % width, index / width);
                let inside = (width / 4..width * 3 / 4).contains(&x)
                    && (height / 4..height * 3 / 4).contains(&y);
                if inside { 0xff8020 } else { 0x0d0d0a }
            })
            .collect()
    }

    #[test]
    fn effects_stay_inside_the_image() {
        let now = Instant::now();
        let background = subject(300, 50);
        for seed in 0..300 {
            let mut glitch = glitch(seed);
            for _ in 0..4 {
                glitch.advance(now);
                for effect in glitch.effects(&background, 300) {
                    let rows = effect.rows(50);
                    assert!(rows.start < rows.end && rows.end <= 50);
                    if let Effect::Block { columns, .. } = effect {
                        assert!(columns.start < columns.end && columns.end <= 300);
                    }
                }
            }
            glitch.reject(now);
            assert!(!glitch.effects(&background, 300).is_empty());
        }
    }

    #[test]
    fn consecutive_frames_use_different_kinds() {
        let now = Instant::now();
        let mut glitch = glitch(11);
        let mut previous = None;
        for _ in 0..50 {
            glitch.advance(now);
            let Look::Typing(kind) = glitch.look else {
                unreachable!("advance always shows a typing frame");
            };
            assert_ne!(Some(kind), previous);
            previous = Some(kind);
        }
    }

    #[test]
    fn frames_are_stable_until_the_next_key_press() {
        let now = Instant::now();
        let background = subject(500, 500);
        let mut glitch = glitch(7);
        glitch.advance(now);
        let first = glitch.effects(&background, 500);
        glitch.advance(now);
        let second = glitch.effects(&background, 500);

        assert!(!first.is_empty());
        assert_ne!(first, second);
        assert_eq!(second, glitch.effects(&background, 500));
    }

    #[test]
    fn frames_clear_after_their_hold_time() {
        let now = Instant::now();
        let mut glitch = glitch(3);

        glitch.advance(now);
        assert!(!glitch.clear_if_due(now + Duration::from_millis(99)));
        assert!(glitch.clear_if_due(now + Duration::from_millis(100)));
        assert!(glitch.effects(&subject(100, 100), 100).is_empty());
        assert_eq!(glitch.clear_at(), None);

        glitch.reject(now);
        assert!(!glitch.clear_if_due(now + Duration::from_millis(100)));
        assert!(glitch.clear_if_due(now + Duration::from_millis(600)));
    }

    #[test]
    fn verifying_holds_until_the_result_and_rejecting_shows_a_new_frame() {
        let now = Instant::now();
        let background = subject(200, 200);
        let mut glitch = glitch(9);

        glitch.verify(now);
        let verifying = glitch.effects(&background, 200);
        assert!(!glitch.clear_if_due(now + Duration::from_secs(5)));

        glitch.reject(now);
        assert!(!verifying.is_empty());
        assert_ne!(verifying, glitch.effects(&background, 200));
    }

    #[test]
    fn tiny_images_get_no_effects() {
        let mut glitch = glitch(1);
        glitch.reject(Instant::now());

        assert!(glitch.effects(&[], 0).is_empty());
        assert!(glitch.effects(&[0; 100], 100).is_empty());
    }

    #[test]
    fn band_splits_channels() {
        let background = vec![0x000000, 0xffffff, 0x000000, 0x000000, 0xffffff, 0x000000];
        let mut target = background.clone();
        let band = Effect::Band {
            rows: 0..1,
            shift: 0,
            split: 1,
            tint: Tint::None,
        };

        band.draw(&background, &mut target, 3);

        assert_eq!(target.get(..3), Some(&[0x0000ff, 0x00ff00, 0xff0000][..]));
        assert_eq!(target.get(3..), background.get(3..));
    }

    #[test]
    fn fringe_outlines_edges_on_opposite_sides() {
        let background = vec![0x000000, 0x000000, 0xffffff, 0x000000, 0x000000];
        let mut target = background.clone();
        let fringe = Effect::Fringe {
            split_x: 1,
            split_y: 0,
            tint: Tint::None,
        };

        fringe.draw(&background, &mut target, 5);

        assert_eq!(
            target,
            vec![0x000000, 0x0000ff, 0x00ff00, 0xff0000, 0x000000]
        );
    }

    #[test]
    fn block_moves_a_rectangle() {
        let background: Vec<u32> = (0..16).collect();
        let mut target = background.clone();
        let block = Effect::Block {
            rows: 1..3,
            columns: 1..3,
            shift_x: 1,
            shift_y: 1,
        };

        block.draw(&background, &mut target, 4);

        assert_eq!(
            target,
            vec![0, 1, 2, 3, 4, 0, 1, 7, 8, 4, 5, 11, 12, 13, 14, 15]
        );
    }

    #[test]
    fn blocks_land_on_the_subject() {
        let background = subject(400, 400);
        let image = Image {
            pixels: &background,
            width: 400,
            height: 400,
        };
        let mut random = Random::new(5);

        let changed: usize = blocks(&image, &mut random)
            .iter()
            .map(|block| match block {
                Effect::Block {
                    rows,
                    columns,
                    shift_x,
                    shift_y,
                } => visible_change(&image, rows, columns, *shift_x, *shift_y),
                _ => 0,
            })
            .sum();

        assert!(changed > 0);
    }

    #[test]
    fn effects_ignore_rows_outside_the_image() {
        let background = vec![0x123456; 4];
        let mut target = background.clone();
        let band = Effect::Band {
            rows: 1..10,
            shift: 5,
            split: 3,
            tint: Tint::Magenta,
        };

        band.draw(&background, &mut target, 2);
        restore_rows(&background, &mut target, 2, 0..10);
        restore_rows(&background, &mut target, 2, 0..2);

        assert_eq!(target, background);
    }
}
