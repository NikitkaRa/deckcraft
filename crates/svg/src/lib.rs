//! SVG pictures, rasterized for the slide and for the PNG fallback stored next to an icon.
//!
//! An SVG is not allowed to read a file or a URL. Embedded data-URL images are drawn. Text is
//! drawn only with fonts carried in the file; system fonts are not loaded.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::io::Cursor;

/// Longest side when the caller does not ask for one.
pub const DEFAULT_SIDE: u32 = 1024;

/// Refuse an SVG larger than this. A picture does not need a multi-megabyte document.
const MAX_BYTES: usize = 16 << 20;
const MAX_SIDE: u32 = 2048;

/// The bytes look like an SVG document (an optional BOM and XML declaration, then `<svg`).
pub fn is_svg(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let n = bytes.len().min(1024);
    let head = &bytes[..n];
    let mut i = 0;
    while i < head.len() && head[i].is_ascii_whitespace() {
        i += 1;
    }
    if head[i..].starts_with(b"<?xml")
        && let Some(rel) = head[i..].windows(2).position(|w| w == b"?>")
    {
        i += rel + 2;
    }
    while i < head.len() && head[i].is_ascii_whitespace() {
        i += 1;
    }
    let rest = &head[i..];
    rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"<svg")
}

/// Gzip-compressed SVG (`.svgz`).
pub fn is_svgz(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1F, 0x8B])
}

/// Width and height in pixels (96 dpi), from the document size.
pub fn pixel_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let size = parse(bytes)?.size();
    let (w, h) = (size.width(), size.height());
    if w < 1.0 || h < 1.0 {
        return None;
    }
    Some((w.round().clamp(1.0, MAX_SIDE as f32) as u32, h.round().clamp(1.0, MAX_SIDE as f32) as u32))
}

/// Straight RGBA, with the longer side `side` pixels.
pub fn rasterize(bytes: &[u8], side: u32) -> Option<image::RgbaImage> {
    let tree = parse(bytes)?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if !(sw.is_finite() && sh.is_finite()) || sw < 1.0 || sh < 1.0 {
        return None;
    }
    let side = side.clamp(1, MAX_SIDE);
    let scale = side as f32 / sw.max(sh);
    let w = (sw * scale).ceil().clamp(1.0, MAX_SIDE as f32) as u32;
    let h = (sh * scale).ceil().clamp(1.0, MAX_SIDE as f32) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let src = pixmap.data();
    if src.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut img = image::RgbaImage::new(w, h);
    for (dst, px) in img.pixels_mut().zip(src.as_chunks::<4>().0) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else if a == 255 {
            (r, g, b)
        } else {
            let up = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            (up(r), up(g), up(b))
        };
        *dst = image::Rgba([r, g, b, a]);
    }
    Some(img)
}

/// A PNG of [`rasterize`], for the fallback picture PowerPoint stores beside an SVG icon.
pub fn rasterize_png(bytes: &[u8], side: u32) -> Option<Vec<u8>> {
    let img = rasterize(bytes, side)?;
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn parse(bytes: &[u8]) -> Option<resvg::usvg::Tree> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    if !is_svg(bytes) && !is_svgz(bytes) {
        return None;
    }
    let mut opt = resvg::usvg::Options::default();
    // The default resolver opens a path in the href. Pictures must not read the disk.
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    resvg::usvg::Tree::from_data(bytes, &opt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(fill: &str) -> Vec<u8> {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="{fill}"/></svg>"#).into_bytes()
    }

    #[test]
    fn a_rectangle_keeps_its_colour_and_size() {
        let img = rasterize(&icon("red"), 20).unwrap();
        assert_eq!(img.dimensions(), (20, 10));
        let p = img.get_pixel(1, 1).0;
        assert!(p[0] > 200 && p[1] < 30 && p[2] < 30 && p[3] == 255, "{p:?}");
        let (w, h) = pixel_size(&icon("#00ff00")).unwrap();
        assert_eq!((w, h), (20, 10));
    }

    #[test]
    fn a_view_box_sets_the_size() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 20"><rect width="40" height="20" fill="blue"/></svg>"#;
        assert!(is_svg(svg));
        let (w, h) = pixel_size(svg).unwrap();
        assert_eq!((w, h), (40, 20));
        let img = rasterize(svg, 40).unwrap();
        let p = img.get_pixel(0, 0).0;
        assert!(p[2] > 200 && p[0] < 30, "{p:?}");
    }

    #[test]
    fn an_image_href_does_not_read_a_file() {
        let dir = std::env::temp_dir().join(format!("deckcraft-svg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("green.png");
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([0, 255, 0])))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&png, bytes.into_inner()).unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="red"/><image href="{}" width="4" height="4"/></svg>"#,
            png.display()
        );
        let img = rasterize(svg.as_bytes(), 4).unwrap();
        let p = img.get_pixel(0, 0).0;
        assert!(p[0] > 200 && p[1] < 30, "the linked file was drawn: {p:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_broken_and_oversized_documents_are_refused() {
        assert!(rasterize(b"", 32).is_none());
        assert!(rasterize(b"<svg", 32).is_none());
        assert!(!is_svg(b"\x89PNG\r\n"));
        assert!(rasterize(&vec![b'x'; MAX_BYTES + 1], 32).is_none());
        let png = rasterize_png(&icon("black"), 16).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
}
