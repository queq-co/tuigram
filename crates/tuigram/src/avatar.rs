//! Generated fallback avatar bubble (#201, Stage 4): for a sender with no
//! profile-photo minithumbnail, an accent-color tile carrying their first
//! initial, built to the exact pixel size [`crate::terminal::AvatarSupport::gutter_cols`]
//! reserves so it can be fed through the same `Picker::new_protocol` /
//! avatar-cache path as a real photo — real photos and generated bubbles are
//! indistinguishable to the renderer from that point on.

use ab_glyph::{Font, FontRef, Glyph, PxScale, ScaleFont, point};
use image::{DynamicImage, Rgb, RgbImage};
use imageproc::drawing::{draw_text_mut, text_size};
use ratatui::style::Color;
use ratatui_image::FontSize;
use tuigram_core::model::User;

use crate::conversation::accent_color;

/// `DejaVu` Sans Bold (see `assets/fonts/LICENSE_DEJAVU` — Bitstream Vera terms,
/// free to bundle and redistribute): broad Unicode coverage so a sender's
/// first initial renders correctly across scripts, not just Latin.
static FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");

/// Builds the `gutter_cols`-wide, 2-row-tall fallback bubble for `user`: a
/// solid tile in the user's own accent color (#194's `accent_color`, so the
/// bubble and the header tint agree) with their first display-name character
/// centered on it in white.
pub(crate) fn fallback_bubble(
    font_size: FontSize,
    gutter_cols: usize,
    user: &User,
) -> DynamicImage {
    let width = u32::from(font_size.width) * gutter_cols as u32;
    let height = u32::from(font_size.height) * 2;
    let background = to_rgb(accent_color(user.accent_color_id, user.id));
    let mut image = RgbImage::from_pixel(width.max(1), height.max(1), background);

    let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled font is valid");
    let mut buf = [0u8; 4];
    let text = first_initial(user).encode_utf8(&mut buf);
    let scale = PxScale::from(height as f32 * 0.7);
    let (text_width, text_height) = text_size(scale, &font, text);
    let x = (width as i32 - text_width as i32) / 2;
    // `draw_text_mut` anchors its glyph layout at the font's *ascent* (the
    // line's baseline), not at the ink's own top edge, so naively centering
    // on `(height - text_height) / 2` renders a capital letter noticeably low
    // — confirmed visually via a throwaway render before this fix. Measuring
    // the glyph's own top inset (`ink_top_offset`) and subtracting it here
    // centers the drawn ink itself rather than the invisible baseline box.
    let y = (height as i32 - text_height as i32) / 2 - ink_top_offset(scale, &font, text);
    draw_text_mut(&mut image, Rgb([255, 255, 255]), x, y, scale, &font, text);

    DynamicImage::ImageRgb8(image)
}

/// The y-offset (in pixels, from the font's ascent line) of the topmost inked
/// pixel of `text`'s first glyph — mirrors `imageproc::drawing`'s own
/// (private) glyph layout so its ascent-anchored positioning can be
/// compensated for. `text` here is always exactly one character
/// ([`first_initial`]'s output), so only the first glyph's bounds matter.
fn ink_top_offset(scale: PxScale, font: &FontRef, text: &str) -> i32 {
    let scaled = font.as_scaled(scale);
    let Some(ch) = text.chars().next() else {
        return 0;
    };
    let glyph: Glyph = scaled
        .glyph_id(ch)
        .with_scale_and_position(scale, point(0.0, scaled.ascent()));
    scaled
        .outline_glyph(glyph)
        .map_or(0, |outlined| outlined.px_bounds().min.y.round() as i32)
}

/// The character drawn on a fallback bubble: the first character of the
/// sender's own display name — the header's existing fallback chain (full
/// name, else `@handle`, else "Deleted Account", else `User {id}`) already
/// guarantees a non-empty string — uppercased so it reads consistently
/// regardless of the source casing.
fn first_initial(user: &User) -> char {
    user.display_name()
        .chars()
        .next()
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?')
}

/// Approximates one of [`accent_color`]'s fixed 7-color palette (its hash
/// fallback also only ever resolves to one of these same 7) as concrete RGB
/// for pixel fill. `ratatui::style::Color`'s named variants carry no
/// canonical RGB of their own — a terminal theme is free to remap them — so
/// this is a fixed, reasonable approximation rather than a faithful
/// terminal-theme lookup; the fallback arm is unreachable in practice since
/// `accent_color` never returns any other variant.
fn to_rgb(color: Color) -> Rgb<u8> {
    match color {
        Color::Red => Rgb([205, 0, 0]),
        // A duller, darker amber than terminal "yellow" — full-brightness
        // yellow behind the bubble's white initial reads as low-contrast
        // (confirmed visually via a throwaway render before this fix).
        Color::Yellow => Rgb([173, 129, 0]),
        Color::Magenta => Rgb([205, 0, 205]),
        Color::Green => Rgb([0, 153, 0]),
        Color::Cyan => Rgb([0, 205, 205]),
        Color::Blue => Rgb([0, 0, 238]),
        Color::LightMagenta => Rgb([205, 0, 155]),
        _ => Rgb([128, 128, 128]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::model::UserKind;

    fn sample_user(id: i64, first_name: &str, accent_color_id: i32) -> User {
        User {
            id,
            first_name: first_name.to_owned(),
            last_name: String::new(),
            usernames: Vec::new(),
            phone_number: None,
            is_contact: false,
            kind: UserKind::Regular,
            status: tuigram_core::model::Presence::Offline { was_online: 0 },
            accent_color_id,
            avatar_minithumbnail: None,
        }
    }

    #[test]
    fn fallback_bubble_is_sized_to_the_gutter_in_pixels() {
        let font_size = FontSize::new(10, 20);
        let image = fallback_bubble(font_size, 4, &sample_user(1, "Ada", 0));
        assert_eq!(image.width(), 40);
        assert_eq!(image.height(), 40);
    }

    #[test]
    fn fallback_bubble_uses_the_first_uppercased_initial() {
        assert_eq!(first_initial(&sample_user(1, "ada", 0)), 'A');
        assert_eq!(first_initial(&sample_user(2, "", 0)), 'U'); // "User {id}"
    }

    #[test]
    fn to_rgb_maps_every_accent_palette_color_distinctly() {
        let colors = [
            Color::Red,
            Color::Yellow,
            Color::Magenta,
            Color::Green,
            Color::Cyan,
            Color::Blue,
            Color::LightMagenta,
        ];
        let rgbs: Vec<Rgb<u8>> = colors.into_iter().map(to_rgb).collect();
        for (i, a) in rgbs.iter().enumerate() {
            for b in &rgbs[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
