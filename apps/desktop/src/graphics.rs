//! Generated images: the refresh icon, tray icon, rounded backdrop, and provider logos.

use super::*;

/// The refresh icon in the theme's text color, drawn once per color.
pub(super) fn refresh_icon_handle(theme: &'static ThemeDefinition) -> image::Handle {
    let color = theme.colors.text;
    let mut handles = REFRESH_ICON_HANDLES
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some((_, handle)) = handles.iter().find(|(drawn, _)| *drawn == color) {
        return handle.clone();
    }
    let handle = render_refresh_icon(color);
    handles.push((color, handle.clone()));
    handle
}

pub(super) fn render_refresh_icon(color: [u8; 3]) -> image::Handle {
    const SIZE: u32 = 96;
    const CENTER: (f32, f32) = (12.0, 12.0);
    const RADIUS: f32 = 8.0;
    const STROKE_WIDTH: f32 = 2.0;

    let scale = SIZE as f32 / 24.0;
    let mut pixels = vec![0; (SIZE * SIZE * 4) as usize];
    for end_angle in [35.0, -145.0] {
        draw_refresh_arc(
            &mut pixels,
            SIZE,
            color,
            CENTER,
            RADIUS,
            STROKE_WIDTH,
            end_angle + 160.0,
            end_angle,
            scale,
        );
        draw_refresh_arrowhead(
            &mut pixels,
            SIZE,
            color,
            CENTER,
            RADIUS,
            STROKE_WIDTH,
            end_angle,
            scale,
        );
    }

    image::Handle::from_rgba(SIZE, SIZE, pixels)
}

pub(super) fn draw_refresh_arc(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    center: (f32, f32),
    radius: f32,
    stroke_width: f32,
    start_angle: f32,
    end_angle: f32,
    scale: f32,
) {
    const SEGMENTS: usize = 48;
    let angle_span = (start_angle - end_angle).rem_euclid(360.0);
    let mut previous = point_on_circle(center, radius, start_angle);

    for segment in 1..=SEGMENTS {
        let progress = segment as f32 / SEGMENTS as f32;
        let angle = start_angle - angle_span * progress;
        let next = point_on_circle(center, radius, angle);
        draw_refresh_line(pixels, size, color, previous, next, stroke_width, scale);
        previous = next;
    }
}

pub(super) fn draw_refresh_arrowhead(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    center: (f32, f32),
    radius: f32,
    stroke_width: f32,
    angle_degrees: f32,
    scale: f32,
) {
    let angle = angle_degrees.to_radians();
    let tip = (
        center.0 + radius * angle.cos(),
        center.1 + radius * angle.sin(),
    );
    let tangent = (angle.sin(), -angle.cos());
    let base = (tip.0 - tangent.0 * 3.2, tip.1 - tangent.1 * 3.2);
    let perpendicular = (-tangent.1 * 2.2, tangent.0 * 2.2);

    for wing in [1.0, -1.0] {
        draw_refresh_line(
            pixels,
            size,
            color,
            tip,
            (
                base.0 + perpendicular.0 * wing,
                base.1 + perpendicular.1 * wing,
            ),
            stroke_width,
            scale,
        );
    }
}

pub(super) fn point_on_circle(center: (f32, f32), radius: f32, angle_degrees: f32) -> (f32, f32) {
    let angle = angle_degrees.to_radians();
    (
        center.0 + radius * angle.cos(),
        center.1 + radius * angle.sin(),
    )
}

pub(super) fn draw_refresh_line(
    pixels: &mut [u8],
    size: u32,
    color: [u8; 3],
    start: (f32, f32),
    end: (f32, f32),
    stroke_width: f32,
    scale: f32,
) {
    let start = (start.0 * scale, start.1 * scale);
    let end = (end.0 * scale, end.1 * scale);
    let radius = stroke_width * scale / 2.0;
    let delta = (end.0 - start.0, end.1 - start.1);
    let length_squared = delta.0 * delta.0 + delta.1 * delta.1;
    let min_x = (start.0.min(end.0) - radius - 1.0).floor().max(0.0) as u32;
    let max_x = (start.0.max(end.0) + radius + 1.0)
        .ceil()
        .min(size as f32 - 1.0) as u32;
    let min_y = (start.1.min(end.1) - radius - 1.0).floor().max(0.0) as u32;
    let max_y = (start.1.max(end.1) + radius + 1.0)
        .ceil()
        .min(size as f32 - 1.0) as u32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let point = (x as f32 + 0.5, y as f32 + 0.5);
            let projection = if length_squared == 0.0 {
                0.0
            } else {
                (((point.0 - start.0) * delta.0 + (point.1 - start.1) * delta.1) / length_squared)
                    .clamp(0.0, 1.0)
            };
            let closest = (
                start.0 + projection * delta.0,
                start.1 + projection * delta.1,
            );
            let distance = ((point.0 - closest.0).powi(2) + (point.1 - closest.1).powi(2)).sqrt();
            let alpha = ((radius + 0.5 - distance).clamp(0.0, 1.0) * 255.0) as u8;
            let pixel_index = ((y * size + x) * 4) as usize;
            if alpha > pixels[pixel_index + 3] {
                pixels[pixel_index..pixel_index + 4]
                    .copy_from_slice(&[color[0], color[1], color[2], alpha]);
            }
        }
    }
}

/// The tray icon: the app icon at 32 px, as RGBA.
pub(super) fn icon_pixels() -> Vec<u8> {
    ::image::load_from_memory(include_bytes!("../assets/icon/tray-32.png"))
        .map(|icon| icon.to_rgba8().into_raw())
        .unwrap_or_else(|_| vec![0; 32 * 32 * 4])
}

pub(super) fn backdrop_image_handle(theme_id: ThemeId) -> Option<image::Handle> {
    let backdrop = theme_id.definition().backdrop?;
    // The custom theme's picture is the one the user saved.
    let custom_bytes;
    let bytes = if theme_id == ThemeId::Custom {
        custom_bytes = custom_theme::image_bytes()?;
        custom_bytes.as_slice()
    } else {
        backdrop.image_bytes
    };

    match rounded_backdrop_image(bytes, backdrop.image_scale) {
        Ok(image) => Some(image),
        Err(error) => {
            preview_log(format!("theme backdrop preparation failed: {error}"));
            None
        }
    }
}

pub(super) fn rounded_backdrop_image(
    image_bytes: &[u8],
    image_scale: f32,
) -> Result<image::Handle, String> {
    let decoded = ::image::load_from_memory(image_bytes)
        .map_err(|error| format!("could not decode image: {error}"))?
        .to_rgba8();
    let cover_scale =
        (WINDOW_WIDTH / decoded.width() as f32).max(WINDOW_HEIGHT / decoded.height() as f32);
    let image_scale = image_scale.max(0.01);
    let crop_width = (WINDOW_WIDTH / (cover_scale * image_scale))
        .round()
        .clamp(1.0, decoded.width() as f32) as u32;
    let crop_height = (WINDOW_HEIGHT / (cover_scale * image_scale))
        .round()
        .clamp(1.0, decoded.height() as f32) as u32;
    let cropped = ::image::imageops::crop_imm(
        &decoded,
        (decoded.width() - crop_width) / 2,
        (decoded.height() - crop_height) / 2,
        crop_width,
        crop_height,
    )
    .to_image();

    let output_width = WINDOW_WIDTH as u32 * BACKDROP_PIXEL_SCALE;
    let output_height = WINDOW_HEIGHT as u32 * BACKDROP_PIXEL_SCALE;
    let mut output = ::image::imageops::resize(
        &cropped,
        output_width,
        output_height,
        ::image::imageops::FilterType::Lanczos3,
    );
    let radius = WINDOW_FRAME_RADIUS * BACKDROP_PIXEL_SCALE as f32;

    for (x, y, pixel) in output.enumerate_pixels_mut() {
        let coverage = rounded_rectangle_coverage(
            x as f32 + 0.5,
            y as f32 + 0.5,
            output_width as f32,
            output_height as f32,
            radius,
        );
        pixel.0[3] = (f32::from(pixel.0[3]) * coverage).round() as u8;
    }

    Ok(image::Handle::from_rgba(
        output_width,
        output_height,
        output.into_raw(),
    ))
}

pub(super) fn rounded_rectangle_coverage(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> f32 {
    let corner_x = (x - width / 2.0).abs() - (width / 2.0 - radius);
    let corner_y = (y - height / 2.0).abs() - (height / 2.0 - radius);
    let outside_distance = corner_x.max(0.0).hypot(corner_y.max(0.0));
    let inside_distance = corner_x.max(corner_y).min(0.0);
    let signed_distance = outside_distance + inside_distance - radius;

    (0.5 - signed_distance).clamp(0.0, 1.0)
}

pub(super) fn decode_provider_logo(bytes: &[u8], black_foreground: bool) -> image::Handle {
    const MAX_LOGO_DIMENSION: u32 = 192;

    let decoded = ::image::load_from_memory(bytes)
        .expect("embedded provider logo must be a valid image")
        .to_rgba8();
    let max_dimension = decoded.width().max(decoded.height());
    let mut pixels = if max_dimension > MAX_LOGO_DIMENSION {
        let scale = MAX_LOGO_DIMENSION as f32 / max_dimension as f32;
        let width = (decoded.width() as f32 * scale).round().max(1.0) as u32;
        let height = (decoded.height() as f32 * scale).round().max(1.0) as u32;
        ::image::imageops::resize(
            &decoded,
            width,
            height,
            ::image::imageops::FilterType::Lanczos3,
        )
    } else {
        decoded
    };

    if black_foreground {
        for pixel in pixels.pixels_mut() {
            if pixel[3] > 0 {
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 0;
            }
        }
    }

    image::Handle::from_rgba(pixels.width(), pixels.height(), pixels.into_raw())
}
