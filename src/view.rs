//! Stored images and PDF pages made ready to look at: an image no larger
//! than a calling agent can take in, and a PDF page rasterized to PNG.
//!
//! PDF pages are drawn by hayro, a PDF rasterizer written in Rust, so the
//! image needs no system library.

use std::io::Cursor;

use hayro::{
    PixmapSettings, RenderCache, RenderSettings,
    hayro_interpret::InterpreterSettings,
    hayro_syntax::{LoadPdfError, Pdf},
    vello_cpu::color::palette::css::WHITE,
};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder, imageops::FilterType};

/// The long side of any image get_file returns, in pixels.
pub const MAX_VIEW_PX: u32 = 2000;
/// The most a PDF page is enlarged to reach `MAX_VIEW_PX`: four times its
/// size at 72 dots per inch, which is 288 dpi.
const MAX_PDF_SCALE: f32 = 4.0;
/// Images larger than this on either side are not decoded.
const MAX_SOURCE_PX: u32 = 30_000;

/// An image ready to return.
#[derive(Debug)]
pub struct Viewable {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// An image as stored, or scaled down to `MAX_VIEW_PX` on its long side
/// when it is larger: a JPEG stays a JPEG, anything else becomes a PNG.
/// Returns the image and the stored image's width and height.
pub fn image(bytes: &[u8], media_type: &str) -> Result<(Viewable, (u32, u32)), String> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        other => return Err(format!("{other} is not an image this server shows")),
    };
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| format!("not a readable image: {e}"))?;
    if width.max(height) <= MAX_VIEW_PX {
        let media_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            _ => "image/webp",
        };
        let viewable = Viewable {
            bytes: bytes.to_vec(),
            media_type,
            width,
            height,
        };
        return Ok((viewable, (width, height)));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_PX);
    limits.max_image_height = Some(MAX_SOURCE_PX);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| format!("not a readable image: {e}"))?;
    let scaled = decoded.resize(MAX_VIEW_PX, MAX_VIEW_PX, FilterType::Triangle);
    let mut out = Vec::new();
    let media_type = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(scaled.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 85))
            .map_err(|e| format!("encoding the scaled image: {e}"))?;
        "image/jpeg"
    } else {
        scaled
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| format!("encoding the scaled image: {e}"))?;
        "image/png"
    };
    let viewable = Viewable {
        bytes: out,
        media_type,
        width: scaled.width(),
        height: scaled.height(),
    };
    Ok((viewable, (width, height)))
}

#[derive(Debug)]
pub enum PdfError {
    /// Not a PDF this renderer reads, or one it cannot decrypt.
    Unreadable(String),
    /// The page asked for is not in the document, which has this many.
    NoSuchPage(usize),
}

/// Page `page` (from 1) of a PDF as a PNG, scaled to `MAX_VIEW_PX` on its long
/// side but never more than `MAX_PDF_SCALE` times, and the number of pages.
pub fn pdf_page(bytes: Vec<u8>, page: usize) -> Result<(Viewable, usize), PdfError> {
    let pdf = Pdf::new(bytes).map_err(|e| {
        PdfError::Unreadable(match e {
            LoadPdfError::Decryption(_) => "an encrypted PDF, which this server cannot open".to_owned(),
            _ => "not a readable PDF".to_owned(),
        })
    })?;
    let pages = pdf.pages();
    let count = pages.len();
    if page == 0 || page > count {
        return Err(PdfError::NoSuchPage(count));
    }
    let chosen = &pages[page - 1];
    let (width, height) = chosen.render_dimensions();
    let long = width.max(height);
    if !(long.is_finite() && long > 0.0) {
        return Err(PdfError::Unreadable(format!("page {page} has no size")));
    }
    let scale = (MAX_VIEW_PX as f32 / long).min(MAX_PDF_SCALE);
    let pixmap = hayro::render(
        chosen,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    let (width, height) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
    let bytes = pixmap
        .into_png()
        .map_err(|e| PdfError::Unreadable(format!("encoding page {page}: {e}")))?;
    let viewable = Viewable {
        bytes,
        media_type: "image/png",
        width,
        height,
    };
    Ok((viewable, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::new_rgb8(width, height);
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .expect("png");
        out
    }

    #[test]
    fn a_small_image_is_returned_as_stored() {
        let stored = png(300, 200);
        let (viewable, size) = image(&stored, "image/png").expect("image");
        assert_eq!(viewable.bytes, stored);
        assert_eq!((viewable.width, viewable.height, size), (300, 200, (300, 200)));
    }

    #[test]
    fn a_large_image_is_scaled_to_the_long_side() {
        let (viewable, size) = image(&png(4000, 1000), "image/png").expect("image");
        assert_eq!(size, (4000, 1000));
        assert_eq!((viewable.width, viewable.height), (2000, 500));
        assert_eq!(viewable.media_type, "image/png");
    }

    #[test]
    fn garbage_is_not_an_image_or_a_pdf() {
        assert!(image(b"not an image", "image/png").is_err());
        assert!(matches!(
            pdf_page(b"not a pdf".to_vec(), 1),
            Err(PdfError::Unreadable(_))
        ));
    }
}
