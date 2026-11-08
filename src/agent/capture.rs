//! A single still of the primary display, as PNG, for the agent's `screenshot` and for the viewer's auto-screenshot
//! path when it asks the host. Independent of the live video pipeline: it grabs one frame and encodes it, so a
//! capture here never disturbs an ongoing session.

use std::io::Cursor;
use std::time::Duration;

use hbb_common::{bail, log, ResultType};
use image::{codecs::png::PngEncoder, ColorType, ImageEncoder};
use scrap::{Capturer, Display, Frame, TraitCapturer, TraitPixelBuffer};

/// The primary display's current frame as a PNG, with its pixel width and height.
pub fn primary_png() -> ResultType<(Vec<u8>, u32, u32)> {
    let display = primary_display()?;
    let mut capturer = Capturer::new(display)?;
    let (w, h) = (capturer.width(), capturer.height());
    // DXGI yields nothing until the desktop next composites a frame; a handful of tries covers a quiet screen.
    for attempt in 0..60 {
        match capturer.frame(Duration::from_millis(50)) {
            Ok(Frame::PixelBuffer(buffer)) => {
                let png = encode(buffer.data(), w, h, buffer.stride().first().copied().unwrap_or(w * 4))?;
                return Ok((png, w as u32, h as u32));
            }
            Ok(Frame::Texture(_)) => bail!("capture returned a GPU texture; the agent path wants CPU frames"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt % 20 == 19 {
                    log::debug!("agent capture still waiting for a frame");
                }
                continue;
            }
            Err(e) => bail!("capture failed: {e}"),
        }
    }
    bail!("capture timed out waiting for a frame")
}

fn primary_display() -> ResultType<Display> {
    let mut all = Display::all()?;
    if all.is_empty() {
        bail!("no display to capture");
    }
    let index = all.iter().position(|d| d.is_primary()).unwrap_or(0);
    Ok(all.remove(index))
}

/// Swap BGRA to RGBA, dropping any row padding, then PNG-encode.
fn encode(bgra: &[u8], w: usize, h: usize, stride: usize) -> ResultType<Vec<u8>> {
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = &bgra[y * stride..y * stride + w * 4];
        let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            dst[x * 4] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4];
            dst[x * 4 + 3] = 255;
        }
    }
    let mut out = Vec::new();
    PngEncoder::new(Cursor::new(&mut out)).write_image(&rgba, w as u32, h as u32, ColorType::Rgba8)?;
    Ok(out)
}
