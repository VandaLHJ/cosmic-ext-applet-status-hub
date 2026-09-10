use crate::core::icons::RgbaImage;

use super::{ThemeContext, raster};

const MIN_ALPHA: u8 = 16;
const MAX_NEUTRAL_CHROMA: u8 = 16;
const MAX_MONOTONE_SPAN: u8 = 24;
const TONE_TOLERANCE: u8 = 12;
const MIN_DUOTONE_PERCENT: u64 = 98;
const MAX_SUBTLE_BORDER_SPAN: u8 = 48;
const MAX_COMPLEX_SCANLINE_PERCENT: usize = 20;
const ANTIALIAS_RADIUS: usize = 2;
const MIN_CLEAR_PERCENT: usize = 10;
const MAX_BADGE_BOX_PERCENT: usize = 40;
const MAX_BADGE_VISIBLE_PERCENT: usize = 50;
const MIN_BADGE_FILL_PERCENT: usize = 15;
const BADGE_ANALYSIS_MARGIN: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintDecision {
    Original(&'static str),
    Symbolic,
    Monotone,
    Duotone {
        low: u8,
        high: u8,
        high_is_primary: bool,
    },
}

impl PaintDecision {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original(reason) => reason,
            Self::Symbolic => "symbolic-explicit",
            Self::Monotone => "monotone",
            Self::Duotone { .. } => "duotone",
        }
    }
}

pub fn recolour(image: &mut RgbaImage, theme: &ThemeContext, explicit: bool) -> PaintDecision {
    if !theme.colour_icons && !explicit {
        return PaintDecision::Original("original-disabled");
    }
    let Some((width, height, pixels)) = raster::pixels(image) else {
        return PaintDecision::Original("original-invalid");
    };
    let badge = classify_badge(pixels, width, height);
    let decision = if explicit {
        PaintDecision::Symbolic
    } else {
        classify(pixels, width, height, badge.as_ref())
    };
    if matches!(decision, PaintDecision::Original(_)) {
        return decision;
    }

    let secondary: [u8; 3] = std::array::from_fn(|channel| {
        u8::try_from(
            (u16::from(theme.ink[channel]) * 7 + u16::from(theme.background[channel]) * 3 + 5) / 10,
        )
        .unwrap_or_default()
    });
    for (index, pixel) in image.bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        if pixel[3] == 0
            || badge
                .as_ref()
                .is_some_and(|b| b.contains(index % width, index / width))
        {
            continue;
        }
        let colour = match decision {
            PaintDecision::Duotone {
                low,
                high,
                high_is_primary,
            } => {
                let span = u32::from(high - low);
                let level = u32::from(intensity(*pixel).clamp(low, high) - low);
                let primary_weight = if high_is_primary { level } else { span - level };
                std::array::from_fn(|channel| {
                    u8::try_from(
                        (u32::from(theme.ink[channel]) * primary_weight
                            + u32::from(secondary[channel]) * (span - primary_weight)
                            + span / 2)
                            / span,
                    )
                    .unwrap_or_default()
                })
            }
            _ => theme.ink,
        };
        pixel[..3].copy_from_slice(&colour);
    }
    decision
}

fn intensity(pixel: [u8; 4]) -> u8 {
    u8::try_from(
        (u32::from(pixel[0]) * 54 + u32::from(pixel[1]) * 183 + u32::from(pixel[2]) * 19 + 128)
            / 256,
    )
    .unwrap_or_default()
}

fn classify(
    pixels: &[[u8; 4]],
    width: usize,
    height: usize,
    badge: Option<&Badge>,
) -> PaintDecision {
    let sample = |index: usize| {
        let pixel = pixels[index];
        (pixel[3] >= MIN_ALPHA
            && !badge.is_some_and(|b| b.affects_analysis(index % width, index / width)))
        .then_some(pixel)
    };
    let mut histogram = [0u64; 256];
    for index in 0..pixels.len() {
        if let Some(pixel) = sample(index) {
            let chroma =
                pixel[0].max(pixel[1]).max(pixel[2]) - pixel[0].min(pixel[1]).min(pixel[2]);
            if chroma > MAX_NEUTRAL_CHROMA {
                return PaintDecision::Original("original-coloured");
            }
            histogram[usize::from(intensity(pixel))] += u64::from(pixel[3]);
        }
    }
    let Some(low) = histogram.iter().position(|&mass| mass > 0) else {
        return PaintDecision::Original("original-empty");
    };
    let high = histogram.iter().rposition(|&mass| mass > 0).unwrap_or(low);
    let low = u8::try_from(low).unwrap_or_default();
    let high = u8::try_from(high).unwrap_or(low);
    if high - low <= MAX_MONOTONE_SPAN {
        return PaintDecision::Monotone;
    }
    let low_mass: u64 = histogram[usize::from(low)..=usize::from(low + TONE_TOLERANCE)]
        .iter()
        .sum();
    let high_mass: u64 = histogram[usize::from(high - TONE_TOLERANCE)..=usize::from(high)]
        .iter()
        .sum();
    let total: u64 = histogram.iter().sum();
    let high_is_primary = high_mass >= low_mass;
    let level = |index| sample(index).map(intensity);
    if complex_contours(width, height, level, low, high, high_is_primary) {
        return PaintDecision::Original("original-complex-contours");
    }
    if (low_mass + high_mass) * 100 >= total * MIN_DUOTONE_PERCENT {
        return PaintDecision::Duotone {
            low,
            high,
            high_is_primary,
        };
    }

    antialiased_palette(width, height, sample)
        .unwrap_or(PaintDecision::Original("original-multiple-tones"))
}

fn antialiased_palette(
    width: usize,
    height: usize,
    sample: impl Fn(usize) -> Option<[u8; 4]>,
) -> Option<PaintDecision> {
    let mut solid = [0u64; 256];
    for y in 1..height.saturating_sub(1) {
        for x in 1..width.saturating_sub(1) {
            let index = y * width + x;
            let Some(pixel) = sample(index) else { continue };
            let tone = intensity(pixel);
            if neighbourhood(index, width, height, 1).all(|near| {
                sample(near).is_some_and(|other| intensity(other).abs_diff(tone) <= TONE_TOLERANCE)
            }) {
                solid[usize::from(tone)] += u64::from(pixel[3]);
            }
        }
    }
    let low = u8::try_from(solid.iter().position(|&mass| mass > 0)?).ok()?;
    let high = u8::try_from(solid.iter().rposition(|&mass| mass > 0)?).ok()?;
    let monotone = high - low <= MAX_MONOTONE_SPAN;
    let matches_ink =
        |value: u8| value.abs_diff(low) <= TONE_TOLERANCE || value.abs_diff(high) <= TONE_TOLERANCE;
    let solid_total: u64 = solid.iter().sum();
    let solid_matched: u64 = solid
        .iter()
        .enumerate()
        .filter(|(tone, _)| matches_ink(u8::try_from(*tone).unwrap_or_default()))
        .map(|(_, &mass)| mass)
        .sum();
    if solid_matched * 100 < solid_total * MIN_DUOTONE_PERCENT {
        return None;
    }

    let mut total = 0u64;
    let mut matched = 0u64;
    let mut low_mass = 0u64;
    let mut high_mass = 0u64;
    for index in 0..width * height {
        let Some(pixel) = sample(index) else { continue };
        let tone = intensity(pixel);
        let mass = u64::from(pixel[3]);
        total += mass;
        if tone.abs_diff(low) <= TONE_TOLERANCE {
            low_mass += mass;
        }
        if tone.abs_diff(high) <= TONE_TOLERANCE {
            high_mass += mass;
        }
        let mut supported = matches_ink(tone);
        if !supported {
            let x = index % width;
            let y = index / width;
            let mut gap = x < ANTIALIAS_RADIUS
                || y < ANTIALIAS_RADIUS
                || width - 1 - x < ANTIALIAS_RADIUS
                || height - 1 - y < ANTIALIAS_RADIUS;
            let mut near_low = false;
            let mut near_high = false;
            for near in neighbourhood(index, width, height, ANTIALIAS_RADIUS) {
                if let Some(other) = sample(near) {
                    let value = intensity(other);
                    near_low |= value.abs_diff(low) <= TONE_TOLERANCE;
                    near_high |= value.abs_diff(high) <= TONE_TOLERANCE;
                } else {
                    gap = true;
                }
            }
            supported = if monotone {
                gap && (near_low || near_high)
            } else {
                near_low && near_high
            };
        }
        if supported {
            matched += mass;
        }
    }
    if matched * 100 < total * MIN_DUOTONE_PERCENT {
        return None;
    }
    Some(if monotone {
        PaintDecision::Monotone
    } else {
        PaintDecision::Duotone {
            low,
            high,
            high_is_primary: high_mass >= low_mass,
        }
    })
}

fn neighbourhood(
    index: usize,
    width: usize,
    height: usize,
    radius: usize,
) -> impl Iterator<Item = usize> {
    let x = index % width;
    let y = index / width;
    (y.saturating_sub(radius)..=y.saturating_add(radius).min(height - 1)).flat_map(move |y| {
        (x.saturating_sub(radius)..=x.saturating_add(radius).min(width - 1))
            .map(move |x| y * width + x)
    })
}

fn complex_contours(
    width: usize,
    height: usize,
    level: impl Fn(usize) -> Option<u8>,
    low: u8,
    high: u8,
    high_is_primary: bool,
) -> bool {
    let marked = high - low > MAX_SUBTLE_BORDER_SPAN;
    for vertical in [false, true] {
        let (lines, length) = if vertical {
            (width, height)
        } else {
            (height, width)
        };
        let mut visible_lines = 0;
        let mut complex_lines = 0;
        for line in 0..lines {
            let mut first = None;
            let mut previous = None;
            let mut transitions = 0;
            let mut visible = false;
            let mut complex = false;
            for position in 0..=length {
                let value = (position < length)
                    .then(|| {
                        level(if vertical {
                            position * width + line
                        } else {
                            line * width + position
                        })
                    })
                    .flatten();
                let Some(value) = value else {
                    complex |= transitions >= 3
                        || (marked
                            && transitions >= 2
                            && first != Some(high_is_primary)
                            && previous == first);
                    first = None;
                    previous = None;
                    transitions = 0;
                    continue;
                };
                visible = true;
                let tone = if value.abs_diff(low) <= TONE_TOLERANCE {
                    false
                } else if value.abs_diff(high) <= TONE_TOLERANCE {
                    true
                } else {
                    continue;
                };
                first.get_or_insert(tone);
                if previous.is_some_and(|last| last != tone) {
                    transitions += 1;
                }
                previous = Some(tone);
            }
            visible_lines += usize::from(visible);
            complex_lines += usize::from(complex);
        }
        if complex_lines * 100 > visible_lines * MAX_COMPLEX_SCANLINE_PERCENT {
            return true;
        }
    }
    false
}

struct Badge {
    rows: Vec<Option<(usize, usize)>>,
    columns: Vec<Option<(usize, usize)>>,
}

impl Badge {
    fn contains(&self, x: usize, y: usize) -> bool {
        span_covers(self.rows[y], x) && span_covers(self.columns[x], y)
    }

    fn affects_analysis(&self, x: usize, y: usize) -> bool {
        let max_x = x
            .saturating_add(BADGE_ANALYSIS_MARGIN)
            .min(self.columns.len() - 1);
        let max_y = y
            .saturating_add(BADGE_ANALYSIS_MARGIN)
            .min(self.rows.len() - 1);

        (y.saturating_sub(BADGE_ANALYSIS_MARGIN)..=max_y)
            .any(|y| (x.saturating_sub(BADGE_ANALYSIS_MARGIN)..=max_x).any(|x| self.contains(x, y)))
    }
}

fn span_covers(span: Option<(usize, usize)>, value: usize) -> bool {
    span.is_some_and(|(min, max)| (min..=max).contains(&value))
}

fn classify_badge(pixels: &[[u8; 4]], width: usize, height: usize) -> Option<Badge> {
    let mut clear = 0usize;
    let mut visible = 0usize;
    let mut badge_coloured = 0usize;
    let mut rows = vec![None; height];
    let mut columns = vec![None; width];
    let mut artwork = Bounds {
        min_x: width,
        min_y: height,
        max_x: 0,
        max_y: 0,
    };

    for (index, &pixel) in pixels.iter().enumerate() {
        if pixel[3] < MIN_ALPHA {
            clear += 1;
            continue;
        }

        let low = pixel[0].min(pixel[1]).min(pixel[2]);
        let high = pixel[0].max(pixel[1]).max(pixel[2]);
        let chroma = high - low;
        let x = index % width;
        let y = index / width;
        artwork.min_x = artwork.min_x.min(x);
        artwork.min_y = artwork.min_y.min(y);
        artwork.max_x = artwork.max_x.max(x);
        artwork.max_y = artwork.max_y.max(y);
        if chroma > MAX_NEUTRAL_CHROMA {
            badge_coloured += 1;
            rows[y] = Some(include_span(rows[y], x));
            columns[x] = Some(include_span(columns[x], y));
        }
        visible += 1;
    }

    if visible == 0 || clear * 100 < pixels.len() * MIN_CLEAR_PERCENT {
        return None;
    }

    let bounds = Bounds::of(&rows, &columns)?;
    plausible_badge(bounds, artwork, badge_coloured, visible, width, height)
        .then_some(Badge { rows, columns })
}

fn include_span(span: Option<(usize, usize)>, value: usize) -> (usize, usize) {
    span.map_or((value, value), |(min, max)| {
        (min.min(value), max.max(value))
    })
}

#[derive(Clone, Copy)]
struct Bounds {
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
}

impl Bounds {
    fn of(rows: &[Option<(usize, usize)>], columns: &[Option<(usize, usize)>]) -> Option<Self> {
        let occupied = |spans: &[Option<(usize, usize)>]| {
            Some((
                spans.iter().position(Option::is_some)?,
                spans.iter().rposition(Option::is_some)?,
            ))
        };

        let (min_y, max_y) = occupied(rows)?;
        let (min_x, max_x) = occupied(columns)?;
        Some(Self {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    fn width(self) -> usize {
        self.max_x - self.min_x + 1
    }

    fn height(self) -> usize {
        self.max_y - self.min_y + 1
    }
}

fn plausible_badge(
    bounds: Bounds,
    artwork: Bounds,
    coloured: usize,
    visible: usize,
    width: usize,
    height: usize,
) -> bool {
    let area = bounds.width() * bounds.height();
    let edge_margin = artwork.width().max(artwork.height()).div_ceil(16).max(1);
    let at_edge = bounds.min_x <= artwork.min_x + edge_margin
        || bounds.min_y <= artwork.min_y + edge_margin
        || bounds.max_x.saturating_add(edge_margin) >= artwork.max_x
        || bounds.max_y.saturating_add(edge_margin) >= artwork.max_y;

    at_edge
        && area * 100 <= width * height * MAX_BADGE_BOX_PERCENT
        && coloured * 100 <= visible * MAX_BADGE_VISIBLE_PERCENT
        && coloured * 100 >= area * MIN_BADGE_FILL_PERCENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applet::icons::testing::*;

    fn decision(image: &RgbaImage) -> PaintDecision {
        recolour(&mut image.clone(), &light_panel(), false)
    }

    #[test]
    fn a_simple_internal_detail_is_not_a_border_and_repeated_details_remain_complex() {
        let image = pixmap(24, |x, y| {
            if (9..15).contains(&x) && (6..18).contains(&y) {
                [0, 0, 0, 255]
            } else if (3..21).contains(&x) && (3..21).contains(&y) {
                [255; 4]
            } else {
                [0; 4]
            }
        });
        assert!(matches!(decision(&image), PaintDecision::Duotone { .. }));
        let striped = pixmap(24, |x, y| {
            if (3..21).contains(&x) && (3..21).contains(&y) {
                if (6..8).contains(&x) || (11..13).contains(&x) || (16..18).contains(&x) {
                    [0, 0, 0, 255]
                } else {
                    [255; 4]
                }
            } else {
                [0; 4]
            }
        });
        unchanged(&striped, "original-complex-contours");
    }

    #[test]
    fn transparent_padding_does_not_dilute_the_contour_guard() {
        let original = pixmap(24, |x, y| {
            if (3..21).contains(&x) && (3..21).contains(&y) {
                if (6..8).contains(&x) || (11..13).contains(&x) || (16..18).contains(&x) {
                    [0, 0, 0, 255]
                } else {
                    [255; 4]
                }
            } else {
                [0; 4]
            }
        });
        let mut padded = pixmap(48, |_, _| [0; 4]);
        for y in 0..original.height {
            for x in 0..original.width {
                let source = usize::try_from((y * original.width + x) * 4).unwrap();
                let target = usize::try_from(((y + 12) * padded.width + x + 12) * 4).unwrap();
                padded.bytes[target..target + 4]
                    .copy_from_slice(&original.bytes[source..source + 4]);
            }
        }
        assert_eq!(decision(&padded), decision(&original));
    }

    fn unchanged(image: &RgbaImage, reason: &'static str) {
        for theme in [light_panel(), dark_panel(), test_theme([70, 110, 180])] {
            let mut result = image.clone();
            assert_eq!(
                recolour(&mut result, &theme, false),
                PaintDecision::Original(reason)
            );
            assert_eq!(result, *image);
        }
    }

    fn outlined(fill: u8, border: u8, border_alpha: u8) -> RgbaImage {
        pixmap(16, |x, y| {
            if (3..13).contains(&x) && (3..13).contains(&y) {
                [fill, fill, fill, 255]
            } else if (2..14).contains(&x) && (2..14).contains(&y) {
                [border, border, border, border_alpha]
            } else {
                [0; 4]
            }
        })
    }

    #[test]
    fn neutral_monotones_use_exact_theme_ink_and_keep_alpha() {
        for grey in [0, 112, 255] {
            let image = pixmap(10, |x, y| {
                [
                    grey,
                    grey,
                    grey,
                    if y == 0 {
                        0
                    } else if x == 0 {
                        16
                    } else {
                        180
                    },
                ]
            });
            for theme in [light_panel(), dark_panel(), test_theme([70, 110, 180])] {
                let mut result = image.clone();
                assert_eq!(
                    recolour(&mut result, &theme, false),
                    PaintDecision::Monotone
                );
                assert_eq!(alphas(&result), alphas(&image));
                for pixel in result.bytes.as_chunks::<4>().0.iter().filter(|p| p[3] > 0) {
                    assert_eq!(&pixel[..3], &theme.ink);
                }
                assert_eq!(&result.bytes[..40], &image.bytes[..40]);
            }
        }
    }

    #[test]
    fn coloured_bases_remain_original_with_one_or_many_colours() {
        for image in [
            pixmap(16, |_, _| [0, 100, 220, 255]),
            pixmap(16, |x, _| {
                if x < 8 {
                    [220, 20, 20, 255]
                } else {
                    [20, 20, 220, 255]
                }
            }),
        ] {
            unchanged(&image, "original-coloured");
        }
    }

    #[test]
    fn neutral_and_monotone_thresholds_are_inclusive() {
        assert_eq!(
            decision(&pixmap(4, |_, _| [100, 116, 100, 255])),
            PaintDecision::Monotone
        );
        unchanged(&pixmap(4, |_, _| [100, 117, 100, 255]), "original-coloured");
        assert_eq!(
            decision(&pixmap(4, |x, _| if x < 2 {
                [100, 100, 100, 255]
            } else {
                [124, 124, 124, 255]
            })),
            PaintDecision::Monotone
        );
        assert!(matches!(
            decision(&pixmap(4, |x, _| if x < 2 {
                [100, 100, 100, 255]
            } else {
                [125, 125, 125, 255]
            })),
            PaintDecision::Duotone { .. }
        ));
    }

    #[test]
    fn simple_duotones_keep_two_fixed_theme_colours_on_both_panels() {
        let image = pixmap(10, |x, _| {
            if x < 5 {
                [40, 40, 40, 255]
            } else {
                [220, 220, 220, 255]
            }
        });
        for theme in [light_panel(), dark_panel()] {
            let mut result = image.clone();
            assert_eq!(
                recolour(&mut result, &theme, false),
                PaintDecision::Duotone {
                    low: 40,
                    high: 220,
                    high_is_primary: true
                }
            );
            let pixels = result.bytes.as_chunks::<4>().0;
            let secondary: [u8; 3] = std::array::from_fn(|c| {
                u8::try_from(
                    (u16::from(theme.ink[c]) * 7 + u16::from(theme.background[c]) * 3 + 5) / 10,
                )
                .unwrap()
            });
            assert_eq!(&pixels[0][..3], &secondary);
            assert_eq!(&pixels[9][..3], &theme.ink);
            assert_ne!(secondary, theme.ink);
            assert_eq!(alphas(&image), alphas(&result));
        }
    }

    #[test]
    fn dominant_tone_is_weighted_by_alpha_not_pixel_count() {
        let translucent = pixmap(10, |x, _| {
            if x < 7 {
                [40, 40, 40, 16]
            } else {
                [220, 220, 220, 255]
            }
        });
        let opaque = pixmap(10, |x, _| {
            if x < 7 {
                [40, 40, 40, 255]
            } else {
                [220, 220, 220, 255]
            }
        });
        assert!(matches!(
            decision(&translucent),
            PaintDecision::Duotone {
                high_is_primary: true,
                ..
            }
        ));
        assert!(matches!(
            decision(&opaque),
            PaintDecision::Duotone {
                high_is_primary: false,
                ..
            }
        ));
    }

    #[test]
    fn gradients_and_three_significant_tones_stay_original() {
        unchanged(
            &pixmap(32, |x, _| {
                let value = u8::try_from(x * 8).unwrap();
                [value, value, value, 255]
            }),
            "original-multiple-tones",
        );
        unchanged(
            &pixmap(12, |x, _| {
                let value = match x {
                    0..4 => 0,
                    4..8 => 128,
                    _ => 255,
                };
                [value, value, value, 255]
            }),
            "original-multiple-tones",
        );
    }

    #[test]
    fn strong_opaque_and_translucent_outlines_stay_original() {
        for alpha in [16, 128, 255] {
            unchanged(&outlined(255, 0, alpha), "original-complex-contours");
        }
    }

    #[test]
    fn subtle_borders_are_allowed_but_repeated_contours_are_not() {
        assert!(matches!(
            decision(&outlined(100, 148, 255)),
            PaintDecision::Duotone { .. }
        ));
        unchanged(&outlined(100, 149, 255), "original-complex-contours");
        unchanged(
            &pixmap(16, |x, _| {
                let value = if x % 4 < 2 { 100 } else { 140 };
                [value, value, value, 255]
            }),
            "original-complex-contours",
        );
    }

    #[test]
    fn vertical_contours_are_checked_too() {
        unchanged(
            &pixmap(16, |_, y| {
                let value = if y % 4 < 2 { 100 } else { 140 };
                [value, value, value, 255]
            }),
            "original-complex-contours",
        );
    }

    #[test]
    fn transparent_cutouts_break_runs_and_do_not_count_as_contours() {
        let image = pixmap(15, |x, _| {
            if x == 4 || x == 9 {
                [0; 4]
            } else if !(4..=9).contains(&x) {
                [40, 40, 40, 255]
            } else {
                [220, 220, 220, 255]
            }
        });
        assert!(matches!(decision(&image), PaintDecision::Duotone { .. }));
        assert_eq!(decision(&outlined(0, 0, 255)), PaintDecision::Monotone);
    }

    #[test]
    fn antialiasing_does_not_add_transitions_and_is_interpolated() {
        let image = pixmap(100, |x, _| match x {
            0..49 => [0, 0, 0, 255],
            49..51 => [128, 128, 128, 255],
            _ => [255; 4],
        });
        let mut painted = image.clone();
        assert!(matches!(
            recolour(&mut painted, &light_panel(), false),
            PaintDecision::Duotone { .. }
        ));
        let pixels = painted.bytes.as_chunks::<4>().0;
        for ((middle, primary), secondary) in pixels[49][..3]
            .iter()
            .zip(&pixels[99][..3])
            .zip(&pixels[0][..3])
        {
            assert!(middle > primary);
            assert!(middle < secondary);
        }
        let thin_transition = pixmap(100, |x, _| match x {
            0..49 => [0, 0, 0, 255],
            49..52 => [128, 128, 128, 255],
            _ => [255; 4],
        });
        assert!(matches!(
            decision(&thin_transition),
            PaintDecision::Duotone { .. }
        ));
        let too_many = pixmap(100, |x, _| match x {
            0..49 => [0, 0, 0, 255],
            49..57 => [128, 128, 128, 255],
            _ => [255; 4],
        });
        unchanged(&too_many, "original-multiple-tones");
    }

    fn badged(padding: u32) -> RgbaImage {
        pixmap(16 + padding * 2, |x, y| {
            let Some(x) = x.checked_sub(padding) else {
                return [0; 4];
            };
            let Some(y) = y.checked_sub(padding) else {
                return [0; 4];
            };
            match (x, y) {
                (12, 3) => [255; 4],
                (10..15, 1..6) => [224, 30, 90, 255],
                (2..14, 2..14) => [200, 200, 200, 255],
                _ => [0; 4],
            }
        })
    }

    #[test]
    fn badges_keep_their_rgba_and_neutral_details_without_disqualifying_the_base() {
        for padding in [0, 8] {
            let image = badged(padding);
            for theme in [light_panel(), dark_panel()] {
                let (width, height, pixels) = raster::pixels(&image).unwrap();
                let badge = classify_badge(pixels, width, height).unwrap();
                let mut painted = image.clone();
                assert_eq!(
                    recolour(&mut painted, &theme, false),
                    PaintDecision::Monotone
                );
                for (index, pixel) in painted.bytes.as_chunks::<4>().0.iter().enumerate() {
                    if badge.contains(index % width, index / width) || pixel[3] == 0 {
                        assert_eq!(*pixel, pixels[index]);
                    } else {
                        assert_eq!(&pixel[..3], &theme.ink);
                    }
                }
                assert_eq!(alphas(&painted), alphas(&image));
            }
        }
    }

    #[test]
    fn badge_analysis_margin_excludes_neutral_outline_without_painting_it_as_badge() {
        let mut image = badged(0);
        image.bytes[3 * 16 * 4 + 9 * 4..3 * 16 * 4 + 9 * 4 + 4].copy_from_slice(&[0, 0, 0, 255]);
        assert_eq!(decision(&image), PaintDecision::Monotone);
    }

    #[test]
    fn an_uncertain_coloured_region_preserves_the_whole_image() {
        let image = pixmap(16, |x, y| match (x, y) {
            (6..10, 6..10) => [200, 0, 0, 255],
            (2..14, 2..14) => [200, 200, 200, 255],
            _ => [0; 4],
        });
        unchanged(&image, "original-coloured");
    }

    #[test]
    fn explicit_symbolics_override_the_toggle_and_colour_gate_but_keep_badges() {
        let mut coloured = pixmap(16, |_, _| [20, 100, 200, 255]);
        assert_eq!(
            recolour(&mut coloured, &original_icons(), true),
            PaintDecision::Symbolic
        );
        assert_eq!(&coloured.bytes[..3], &original_icons().ink);
        let image = badged(0);
        let mut painted = image.clone();
        assert_eq!(
            recolour(&mut painted, &original_icons(), true),
            PaintDecision::Symbolic
        );
        assert_eq!(
            &painted.bytes[(3 * 16 + 11) * 4..(3 * 16 + 11) * 4 + 4],
            &[224, 30, 90, 255]
        );
        assert_eq!(alphas(&painted), alphas(&image));
    }

    #[test]
    fn disabled_empty_and_invalid_inputs_are_unchanged() {
        let mut neutral = pixmap(16, |_, _| [200; 4]);
        let original = neutral.clone();
        assert_eq!(
            recolour(&mut neutral, &original_icons(), false),
            PaintDecision::Original("original-disabled")
        );
        assert_eq!(neutral, original);
        unchanged(&pixmap(4, |_, _| [255, 0, 0, 15]), "original-empty");
        unchanged(
            &RgbaImage {
                width: 0,
                height: 0,
                bytes: vec![],
            },
            "original-invalid",
        );
        unchanged(
            &RgbaImage {
                width: 1,
                height: 1,
                bytes: vec![0; 3],
            },
            "original-invalid",
        );
        unchanged(
            &RgbaImage {
                width: u32::MAX,
                height: u32::MAX,
                bytes: vec![],
            },
            "original-invalid",
        );
    }

    #[test]
    fn classification_happens_before_resizing_can_hide_thin_contours() {
        let image = pixmap(100, |x, y| {
            if (2..98).contains(&x) && (2..98).contains(&y) {
                [255; 4]
            } else if (1..99).contains(&x) && (1..99).contains(&y) {
                [0, 0, 0, 128]
            } else {
                [0; 4]
            }
        });
        unchanged(&image, "original-complex-contours");
    }
}
