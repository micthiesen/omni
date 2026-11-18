//! Bounded handling of untrusted images.
//!
//! Bytes from mail, the web or a phone are untrusted. The format is decided
//! from magic bytes ([`sniff_image`]); decoding runs in memory with pixel and
//! allocation limits, never through a subprocess or a file; and the async
//! entry points ([`with_decoded`], [`variant`], [`variants`],
//! [`process_image`]) run one decode at a time off the runtime under one
//! deadline. JPEG, PNG, GIF and WebP use the `image` crate's pure-Rust
//! decoders; HEIC uses `heic-rs`.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageEncoder as _, ImageFormat, ImageReader, Limits};
use tokio::sync::Semaphore;

mod rendition;

pub use rendition::{MAX_RETURNED_IMAGE_BYTES, ProcessedImage, process_image};

pub const DEFAULT_MAX_DIMENSION: u32 = 2048;
pub const MIN_MAX_DIMENSION: u32 = 256;
pub const MAX_MAX_DIMENSION: u32 = 8192;
/// Decoded pixels allowed for one image.
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;
/// JPEG quality of a [`Variant`].
pub const VARIANT_JPEG_QUALITY: u8 = 85;

const MAX_DECODE_ALLOC: u64 = 768 * 1024 * 1024;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const LOG: &str = "Image";
const HEIF_BRANDS: [&[u8; 4]; 4] = [b"heic", b"heix", b"mif1", b"msf1"];
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];

/// One decode at a time: a single image may hold several hundred MiB.
static DECODE_SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Gif,
    Webp,
    /// HEIC and HEIF stills (HEVC in ISO-BMFF).
    Heif,
}

impl ImageKind {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Heif => "image/heic",
        }
    }

    /// The format's name in messages ("JPEG", "HEIC").
    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Heif => "HEIC",
        }
    }

    /// The format a declared MIME type names (case-insensitive, with the
    /// common aliases).
    pub fn from_mime(mime_type: &str) -> Option<Self> {
        match mime_type.to_ascii_lowercase().as_str() {
            "image/jpeg" | "image/jpg" | "image/pjpeg" => Some(Self::Jpeg),
            "image/png" | "image/x-png" => Some(Self::Png),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/heic" | "image/heif" | "image/heic-sequence" | "image/heif-sequence" => {
                Some(Self::Heif)
            }
            _ => None,
        }
    }

    /// The format a filename's extension names.
    pub fn from_extension(filename: &str) -> Option<Self> {
        let (_, extension) = filename.rsplit_once('.')?;
        match extension.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "jpe" | "jfif" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::Webp),
            "heic" | "heif" | "hif" => Some(Self::Heif),
            _ => None,
        }
    }
}

/// The image format the leading bytes identify, if any.
pub fn sniff_image(data: &[u8]) -> Option<ImageKind> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(ImageKind::Jpeg);
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(ImageKind::Png);
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(ImageKind::Webp);
    }
    is_heif(data).then_some(ImageKind::Heif)
}

/// A leading `ftyp` box whose major or compatible brands name HEIF and not AVIF.
fn is_heif(data: &[u8]) -> bool {
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    let declared = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if !(16..=4096).contains(&declared) || declared > data.len() {
        return false;
    }
    let brands = || std::iter::once(&data[8..12]).chain(data[16..declared].chunks_exact(4));
    brands().any(|brand| HEIF_BRANDS.iter().any(|known| brand == *known))
        && !brands().any(|brand| AVIF_BRANDS.iter().any(|known| brand == *known))
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("Not a JPEG, PNG, GIF, WebP or HEIC image")]
    Unsupported,
    #[error("Image could not be decoded as {0}")]
    Decode(&'static str),
    #[error("Image exceeds the {} megapixel decode limit", MAX_IMAGE_PIXELS / 1_000_000)]
    TooManyPixels,
    #[error("Image could not be encoded")]
    Encode,
    #[error(
        "Image is still over {} MiB as JPEG quality {}; request a smaller maxDimension",
        MAX_RETURNED_IMAGE_BYTES / (1024 * 1024),
        rendition::JPEG_FALLBACK_QUALITY
    )]
    TooLarge,
    #[error("Image processing timed out")]
    Timeout,
    #[error("Image processing failed")]
    Failed,
}

/// Runs blocking image work off the async runtime, one at a time.
async fn bounded<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ImageError> + Send + 'static,
) -> Result<T, ImageError> {
    // One deadline covers the wait for the slot and the work.
    let run = async {
        let permit = DECODE_SLOT
            .acquire()
            .await
            .map_err(|_| ImageError::Failed)?;
        // The permit moves into the blocking task: a timed-out decode keeps
        // the slot until its thread really finishes.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| ImageError::Failed)?
    };
    match tokio::time::timeout(PROCESS_TIMEOUT, run).await {
        Err(_) => Err(ImageError::Timeout),
        Ok(result) => result,
    }
}

/// Decodes `data` as `kind` and runs `work` on the pixels, off the async
/// runtime: one decode at a time in the process, under the pixel and
/// allocation limits and one 60-second deadline that includes the wait.
pub async fn with_decoded<T: Send + 'static>(
    kind: ImageKind,
    data: Arc<Vec<u8>>,
    work: impl FnOnce(&DecodedImage) -> Result<T, ImageError> + Send + 'static,
) -> Result<T, ImageError> {
    bounded(move || work(&decode(kind, &data)?)).await
}

/// A JPEG rendition of an image, in display orientation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What [`variants`] derives from one decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageVariants {
    /// The format the bytes have.
    pub kind: ImageKind,
    /// The source's size as displayed, after EXIF or HEIF orientation.
    pub width: u32,
    pub height: u32,
    /// `#rrggbb`, see [`DecodedImage::average_color`].
    pub average_color: String,
    /// One per requested edge, in the order given.
    pub variants: Vec<Variant>,
}

/// A JPEG of the image in `data` with EXIF orientation applied, scaled down
/// so its longer edge is at most `max_edge` (never up). The format comes from
/// the magic bytes; anything [`sniff_image`] does not know is
/// [`ImageError::Unsupported`]. Bounded as [`with_decoded`] is.
pub async fn variant(data: Arc<Vec<u8>>, max_edge: u32) -> Result<Variant, ImageError> {
    let kind = sniff_image(&data).ok_or(ImageError::Unsupported)?;
    with_decoded(kind, data, move |image| image.variant(max_edge)).await
}

/// Like [`variant`] for several edges and the average colour, from one decode.
pub async fn variants(
    data: Arc<Vec<u8>>,
    max_edges: Vec<u32>,
) -> Result<ImageVariants, ImageError> {
    let kind = sniff_image(&data).ok_or(ImageError::Unsupported)?;
    with_decoded(kind, data, move |image| {
        Ok(ImageVariants {
            kind,
            width: image.width(),
            height: image.height(),
            average_color: image.average_color(),
            variants: max_edges
                .iter()
                .map(|edge| image.variant(*edge))
                .collect::<Result<_, _>>()?,
        })
    })
    .await
}

/// A synthetic 64x48 gradient encoded by macOS `sips`.
pub const HEIC_SELF_TEST: &[u8] = include_bytes!("../assets/heic-self-test-64x48.heic");

/// Converts the bundled HEIC to JPEG and returns its size, for `omni doctor`:
/// a build whose HEIC decoder does not work fails the image.
pub fn heic_self_test() -> Result<(u32, u32), ImageError> {
    let converted = rendition::render(ImageKind::Heif, HEIC_SELF_TEST, DEFAULT_MAX_DIMENSION)?;
    if converted.mime_type != "image/jpeg" || (converted.width, converted.height) != (64, 48) {
        return Err(ImageError::Failed);
    }
    decode(ImageKind::Jpeg, &converted.data)?;
    Ok((converted.width, converted.height))
}

/// An image decoded to pixels in display orientation.
pub struct DecodedImage {
    image: DynamicImage,
    icc: Option<Vec<u8>>,
    /// The stored pixels were rotated or mirrored to display orientation.
    reoriented: bool,
}

/// Decoder messages describe structure (a marker, a box, a size), never
/// pixel content, so the cause is safe to log.
fn decode_failed(format: &'static str, cause: &dyn std::fmt::Display) -> ImageError {
    tracing::warn!(target: LOG, format, %cause, "Image could not be decoded");
    ImageError::Decode(format)
}

fn check_pixels(width: u32, height: u32) -> Result<(), ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Decode("an image with pixels"));
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(ImageError::TooManyPixels);
    }
    Ok(())
}

/// Decodes `data` as `kind` under the pixel and allocation limits and applies
/// its EXIF orientation. Blocking and memory-heavy: async callers use
/// [`with_decoded`], which also allows one decode at a time.
pub fn decode(kind: ImageKind, data: &[u8]) -> Result<DecodedImage, ImageError> {
    let format = match kind {
        ImageKind::Heif => return decode_heif(data),
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Png => ImageFormat::Png,
        ImageKind::Gif => ImageFormat::Gif,
        ImageKind::Webp => ImageFormat::WebP,
    };
    let failed = |cause: &dyn std::fmt::Display| decode_failed(kind.label(), cause);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| failed(&e))?;
    let (width, height) = decoder.dimensions();
    check_pixels(width, height)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().ok().flatten();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| failed(&e))?;
    image.apply_orientation(orientation);
    Ok(DecodedImage {
        image,
        icc,
        reoriented: orientation != Orientation::NoTransforms,
    })
}

fn decode_heif(data: &[u8]) -> Result<DecodedImage, ImageError> {
    let failed = |cause: &dyn std::fmt::Display| decode_failed("HEIC (HEVC still image)", cause);
    let info = heic_rs::probe(data).map_err(|e| failed(&e))?;
    check_pixels(info.width, info.height)?;
    check_pixels(info.coded_width, info.coded_height)?;
    let layout = if info.has_alpha {
        heic_rs::PixelLayout::Rgba8
    } else {
        heic_rs::PixelLayout::Rgb8
    };
    let options = heic_rs::DecodeOptions::default()
        .with_layout(layout)
        .with_max_pixels(Some(MAX_IMAGE_PIXELS));
    let decoded = heic_rs::decode(data, &options).map_err(|e| failed(&e))?;
    let (width, height) = (decoded.width, decoded.height);
    let image = if info.has_alpha {
        image::RgbaImage::from_raw(width, height, decoded.data).map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(width, height, decoded.data).map(DynamicImage::ImageRgb8)
    }
    .ok_or_else(|| failed(&"decoded size does not match its layout"))?;
    let icc = heic_rs::context::Context::open(data)
        .ok()
        .and_then(|context| {
            let props = context.props(info.primary_item).ok()?;
            context.icc(&props).map(<[u8]>::to_vec)
        });
    Ok(DecodedImage {
        image,
        icc,
        reoriented: false,
    })
}

impl DecodedImage {
    /// Width as displayed.
    pub fn width(&self) -> u32 {
        self.image.width()
    }

    /// Height as displayed.
    pub fn height(&self) -> u32 {
        self.image.height()
    }

    /// A JPEG (quality [`VARIANT_JPEG_QUALITY`]) whose longer edge is at most
    /// `max_edge`; a smaller image keeps its size. Transparency is flattened
    /// onto white, an animation gives its first frame, and an RGB or grey
    /// colour profile is kept.
    pub fn variant(&self, max_edge: u32) -> Result<Variant, ImageError> {
        let max_edge = max_edge.max(1);
        let resized;
        let image = if self.width().max(self.height()) > max_edge {
            resized = self.image.resize(max_edge, max_edge, FilterType::Lanczos3);
            &resized
        } else {
            &self.image
        };
        Ok(Variant {
            jpeg: encode_jpeg(image, self.icc.as_deref(), VARIANT_JPEG_QUALITY)?,
            width: image.width(),
            height: image.height(),
        })
    }

    /// The mean colour as `#rrggbb`, for a placeholder shown while the image
    /// loads. Transparency counts as white.
    pub fn average_color(&self) -> String {
        let small = jpeg_pixels(&self.image.thumbnail(64, 64)).to_rgb8();
        let mut sums = [0u64; 3];
        for pixel in small.pixels() {
            for (sum, channel) in sums.iter_mut().zip(pixel.0) {
                *sum += u64::from(channel);
            }
        }
        let count = u64::from(small.width()) * u64::from(small.height());
        let [r, g, b] = sums.map(|sum| (sum + count / 2).checked_div(count).unwrap_or(0));
        format!("#{r:02x}{g:02x}{b:02x}")
    }
}

/// 8-bit grey or RGB; transparency is flattened onto white.
fn jpeg_pixels(image: &DynamicImage) -> DynamicImage {
    let color = image.color();
    if color.has_alpha() {
        let rgba = image.to_rgba8();
        let mut flat = image::RgbImage::new(rgba.width(), rgba.height());
        for (out, pixel) in flat.pixels_mut().zip(rgba.pixels()) {
            let alpha = u32::from(pixel[3]);
            for channel in 0..3 {
                let value = u32::from(pixel[channel]) * alpha + 255 * (255 - alpha);
                out[channel] = u8::try_from((value + 127) / 255).unwrap_or(u8::MAX);
            }
        }
        DynamicImage::ImageRgb8(flat)
    } else if color.has_color() {
        DynamicImage::ImageRgb8(image.to_rgb8())
    } else {
        DynamicImage::ImageLuma8(image.to_luma8())
    }
}

fn encode_jpeg(
    image: &DynamicImage,
    icc: Option<&[u8]>,
    quality: u8,
) -> Result<Vec<u8>, ImageError> {
    let pixels = jpeg_pixels(image);
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    set_icc(&mut encoder, icc, pixels.color().has_color());
    encoder
        .write_image(
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            pixels.color().into(),
        )
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

fn encode_png(image: &DynamicImage, icc: Option<&[u8]>) -> Result<Vec<u8>, ImageError> {
    let pixels = match image {
        DynamicImage::ImageRgb32F(_) => DynamicImage::ImageRgb16(image.to_rgb16()),
        DynamicImage::ImageRgba32F(_) => DynamicImage::ImageRgba16(image.to_rgba16()),
        other => other.clone(),
    };
    let mut out = Vec::new();
    let mut encoder =
        PngEncoder::new_with_quality(&mut out, CompressionType::Default, PngFilter::Adaptive);
    set_icc(&mut encoder, icc, pixels.color().has_color());
    pixels
        .write_with_encoder(encoder)
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

fn encode_webp(image: &DynamicImage, icc: Option<&[u8]>) -> Result<Vec<u8>, ImageError> {
    let pixels = if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    };
    let mut out = Vec::new();
    let mut encoder = WebPEncoder::new_lossless(&mut out);
    set_icc(&mut encoder, icc, true);
    pixels
        .write_with_encoder(encoder)
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

/// Keeps the colour profile when the encoder can carry one.
/// A profile for another colour model (CMYK, or RGB pixels written as grey)
/// would mislabel the pixels, so it is dropped.
fn set_icc(encoder: &mut impl image::ImageEncoder, icc: Option<&[u8]>, has_color: bool) {
    let space: &[u8] = if has_color { b"RGB " } else { b"GRAY" };
    if let Some(icc) = icc.filter(|icc| icc.get(16..20) == Some(space)) {
        // An encoder that cannot embed a profile still writes correct pixels.
        let _unsupported = encoder.set_icc_profile(icc.to_vec());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn png(image: &DynamicImage) -> Arc<Vec<u8>> {
        Arc::new(encode_png(image, None).unwrap())
    }

    fn solid(width: u32, height: u32, color: [u8; 3]) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb(color),
        ))
    }

    /// `jpeg` with an EXIF segment that sets `orientation`.
    fn with_exif_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0, 0, 0, orientation, 0, 0, 0]);
        exif.extend_from_slice(&[0; 4]);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&[0xff, 0xe1]);
        out.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compatible.len();
        let mut data = u32::try_from(size).unwrap().to_be_bytes().to_vec();
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(major);
        data.extend_from_slice(&[0; 4]);
        for brand in compatible {
            data.extend_from_slice(*brand);
        }
        data.extend_from_slice(&[0; 32]);
        data
    }

    #[test]
    fn sniffs_each_format_by_magic_bytes() {
        assert_eq!(
            sniff_image(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some(ImageKind::Png));
        assert_eq!(sniff_image(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(sniff_image(b"GIF87a...."), Some(ImageKind::Gif));
        assert_eq!(
            sniff_image(b"RIFF\x10\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        assert_eq!(sniff_image(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(sniff_image(b"%PDF-1.7"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn heif_needs_a_heif_brand_and_no_avif_brand() {
        assert_eq!(
            sniff_image(&ftyp(b"heic", &[b"mif1"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(
            sniff_image(&ftyp(b"mif1", &[b"heic"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(
            sniff_image(&ftyp(b"isom", &[b"heix"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(sniff_image(&ftyp(b"msf1", &[])), Some(ImageKind::Heif));
        assert_eq!(sniff_image(&ftyp(b"avif", &[b"mif1", b"miaf"])), None);
        assert_eq!(sniff_image(&ftyp(b"isom", &[b"mp41"])), None);
        let mut truncated = ftyp(b"heic", &[b"mif1"]);
        truncated[3] = 0xff;
        assert_eq!(sniff_image(&truncated), None);
    }

    #[test]
    fn kinds_are_named_by_mime_type_and_extension() {
        assert_eq!(ImageKind::from_mime("IMAGE/JPG"), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::from_mime("image/heif"), Some(ImageKind::Heif));
        assert_eq!(ImageKind::from_mime("image/avif"), None);
        assert_eq!(ImageKind::from_extension("Scan.JPG"), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::from_extension("a.hif"), Some(ImageKind::Heif));
        assert_eq!(ImageKind::from_extension("scan"), None);
    }

    #[tokio::test]
    async fn a_variant_is_a_jpeg_bounded_by_its_longer_edge() {
        let source = png(&solid(400, 200, [200, 40, 40]));
        let thumb = variant(source.clone(), 100).await.unwrap();
        assert_eq!((thumb.width, thumb.height), (100, 50));
        assert_eq!(sniff_image(&thumb.jpeg), Some(ImageKind::Jpeg));
        let decoded = decode(ImageKind::Jpeg, &thumb.jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (100, 50));

        // Never scaled up.
        let full = variant(source, 1600).await.unwrap();
        assert_eq!((full.width, full.height), (400, 200));
    }

    #[tokio::test]
    async fn variants_come_from_one_decode_with_the_average_colour() {
        let mut image = image::RgbImage::from_pixel(300, 600, image::Rgb([0, 0, 0]));
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            if x < 150 {
                *pixel = image::Rgb([255, 255, 255]);
            }
        }
        let derived = variants(png(&DynamicImage::ImageRgb8(image)), vec![480, 60])
            .await
            .unwrap();
        assert_eq!(derived.kind, ImageKind::Png);
        assert_eq!((derived.width, derived.height), (300, 600));
        let sizes: Vec<_> = derived
            .variants
            .iter()
            .map(|v| (v.width, v.height))
            .collect();
        assert_eq!(sizes, [(240, 480), (30, 60)]);
        let grey = u8::from_str_radix(&derived.average_color[1..3], 16).unwrap();
        assert!((120..=135).contains(&grey), "{}", derived.average_color);
        assert_eq!(derived.average_color.len(), 7);
        assert_eq!(&derived.average_color[1..3], &derived.average_color[5..7]);
    }

    #[test]
    fn the_average_colour_is_hex_and_flattens_transparency_onto_white() {
        let red = decode(ImageKind::Png, &png(&solid(8, 8, [255, 0, 0]))).unwrap();
        assert_eq!(red.average_color(), "#ff0000");
        let clear = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([0, 0, 0, 0]),
        ));
        let clear = decode(ImageKind::Png, &png(&clear)).unwrap();
        assert_eq!(clear.average_color(), "#ffffff");
        assert_eq!(
            sniff_image(&clear.variant(8).unwrap().jpeg),
            Some(ImageKind::Jpeg)
        );
    }

    #[tokio::test]
    async fn exif_orientation_is_applied_before_sizing() {
        let jpeg = encode_jpeg(&solid(40, 20, [10, 120, 200]), None, 90).unwrap();
        let rotated = Arc::new(with_exif_orientation(&jpeg, 6));
        let derived = variants(rotated, vec![10]).await.unwrap();
        assert_eq!((derived.width, derived.height), (20, 40));
        assert_eq!(
            (derived.variants[0].width, derived.variants[0].height),
            (5, 10)
        );
    }

    #[tokio::test]
    async fn heic_becomes_a_jpeg_variant() {
        assert_eq!(heic_self_test(), Ok((64, 48)));
        let derived = variants(Arc::new(HEIC_SELF_TEST.to_vec()), vec![32])
            .await
            .unwrap();
        assert_eq!(derived.kind, ImageKind::Heif);
        assert_eq!((derived.width, derived.height), (64, 48));
        assert_eq!(
            (derived.variants[0].width, derived.variants[0].height),
            (32, 24)
        );
    }

    #[tokio::test]
    async fn unknown_and_broken_bytes_are_typed_failures() {
        let html = Arc::new(b"<html>".to_vec());
        assert_eq!(variant(html, 100).await, Err(ImageError::Unsupported));
        let truncated = Arc::new(b"\x89PNG\r\n\x1a\n....".to_vec());
        assert!(matches!(
            variant(truncated, 100).await,
            Err(ImageError::Decode("PNG"))
        ));
        // A PNG header that claims more pixels than the limit is refused
        // before any pixel is decoded.
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            let mut crc = 0xffff_ffffu32;
            for byte in &body {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
                }
            }
            let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
            out.extend_from_slice(&body);
            out.extend_from_slice(&(!crc).to_be_bytes());
            out
        }
        let mut header = 20_000u32.to_be_bytes().to_vec();
        header.extend_from_slice(&20_000u32.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
        huge.extend_from_slice(&chunk(b"IHDR", &header));
        huge.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c]));
        assert_eq!(
            variant(Arc::new(huge), 100).await,
            Err(ImageError::TooManyPixels)
        );
    }
}
