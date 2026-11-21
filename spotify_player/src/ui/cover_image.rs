use std::io::Write;

use anyhow::{Context, Result};
use base64::Engine;
use image::DynamicImage;
use ratatui::{buffer::CellDiffOption, layout::Rect, Frame};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    Image, Resize,
};

/// A cover image prepared for a fixed render area. Construct it once per `(url, area)` and reuse
/// it across frames.
pub enum CoverImage {
    /// Rendered through `ratatui-image` as a widget (kitty / sixel / halfblocks).
    Widget(Box<Protocol>),
    /// A cursor-anchored iTerm2 inline-image escape, written directly to the terminal.
    /// `drawn` tracks whether it has been emitted yet — being grid-anchored, it only needs
    /// to be emitted once.
    Iterm2 { escape: String, drawn: bool },
}

impl CoverImage {
    /// Prepare `img` for rendering into `area` using the protocol selected by `picker`.
    pub fn new(picker: &Picker, img: &DynamicImage, area: Rect) -> Result<Self> {
        if picker.protocol_type() == ProtocolType::Iterm2 {
            Ok(Self::Iterm2 {
                escape: encode_iterm2(&scale_to(picker, img, area), area)?,
                drawn: false,
            })
        } else {
            // `Scale`, not `Fit`: a pixelated cover is smaller than its area and has to grow, with
            // nearest-neighbor, `Scale`'s default filter
            let protocol = picker
                .new_protocol(img.clone(), area.into(), Resize::Scale(None))
                .context("encode cover image protocol")?;
            Ok(Self::Widget(Box::new(protocol)))
        }
    }

    /// Render the cover image into `area`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Widget(protocol) => frame.render_widget(Image::new(protocol.as_ref()), area),
            Self::Iterm2 { escape, drawn } => {
                reserve_area(frame, area);
                if !*drawn {
                    if let Err(err) = write_iterm2(escape, area) {
                        tracing::error!("Failed to draw iTerm2 cover image: {err:#}");
                    } else {
                        *drawn = true;
                    }
                }
            }
        }
    }
}

/// Mark every cell in `area` as skipped so `ratatui`'s renderer leaves it untouched.
fn reserve_area(frame: &mut Frame, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

/// Encode `img` as a cursor-anchored, cell-sized iTerm2 inline-image escape sequence.
/// A cover smaller than `area`, such as a pixelated one, scaled up to it with nearest-neighbor.
/// iTerm2 scales an image to its cells smoothly, which would blur the blocks.
fn scale_to<'a>(
    picker: &Picker,
    img: &'a DynamicImage,
    area: Rect,
) -> std::borrow::Cow<'a, DynamicImage> {
    let font = picker.font_size();
    let (width, height) = (
        u32::from(area.width) * u32::from(font.width),
        u32::from(area.height) * u32::from(font.height),
    );
    if img.width() < width && img.height() < height {
        std::borrow::Cow::Owned(img.resize(width, height, image::imageops::FilterType::Nearest))
    } else {
        std::borrow::Cow::Borrowed(img)
    }
}

fn encode_iterm2(img: &DynamicImage, area: Rect) -> Result<String> {
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encode cover image to PNG")?;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(format!(
        "\x1b]1337;File=inline=1;preserveAspectRatio=1;size={};width={};height={}:{data}\x07",
        png.len(),
        area.width,
        area.height,
    ))
}

/// Write a prepared iTerm2 image escape at `area`'s top-left, erasing the cell box first (so
/// letterboxing doesn't reveal stale content) and restoring the cursor afterwards so
/// `ratatui`'s own rendering is unaffected.
fn write_iterm2(escape: &str, area: Rect) -> std::io::Result<()> {
    // `area` is always a sub-rectangle of the screen, so the cursor-anchored image fits and
    // does not scroll the alternate screen.
    let mut out = std::io::stdout().lock();
    out.write_all(b"\x1b7")?; // DEC save cursor
    for row in area.top()..area.bottom() {
        // move to the start of the row (1-based) and erase `width` cells
        write!(out, "\x1b[{};{}H\x1b[{}X", row + 1, area.x + 1, area.width)?;
    }
    // position at the image origin and draw it
    write!(out, "\x1b[{};{}H{escape}", area.y + 1, area.x + 1)?;
    out.write_all(b"\x1b8")?; // DEC restore cursor
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::scale_to;
    use image::{DynamicImage, GenericImageView};
    use ratatui::layout::Rect;
    use ratatui_image::picker::Picker;
    use std::borrow::Cow;

    #[test]
    fn a_pixelated_cover_is_scaled_up_to_its_area_with_sharp_blocks() {
        // cells of 10 x 20 px: 10 x 5 cells are 100 x 100 px
        let picker = Picker::halfblocks();
        let area = Rect::new(0, 0, 10, 5);
        let mut small = DynamicImage::new_rgb8(4, 4);
        small
            .as_mut_rgb8()
            .unwrap()
            .put_pixel(0, 0, image::Rgb([255, 0, 0]));

        let scaled = scale_to(&picker, &small, area);
        assert_eq!(scaled.dimensions(), (100, 100));
        // nearest-neighbor: the first block is all red, its neighbor untouched
        assert_eq!(scaled.get_pixel(24, 24).0, [255, 0, 0, 255]);
        assert_eq!(scaled.get_pixel(25, 24).0, [0, 0, 0, 255]);

        let large = DynamicImage::new_rgb8(640, 640);
        assert!(matches!(scale_to(&picker, &large, area), Cow::Borrowed(_)));
    }
}
