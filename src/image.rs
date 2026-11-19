use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;

use windows::Win32::Foundation::{E_FAIL, HGLOBAL};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat1bppIndexed,
    GUID_WICPixelFormat2bppGray, GUID_WICPixelFormat2bppIndexed, GUID_WICPixelFormat4bppGray,
    GUID_WICPixelFormat4bppIndexed, GUID_WICPixelFormat8bppGray, GUID_WICPixelFormat8bppIndexed,
    GUID_WICPixelFormat16bppBGR555, GUID_WICPixelFormat16bppBGR565, GUID_WICPixelFormat16bppGray,
    GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat24bppRGB, GUID_WICPixelFormat32bppBGR,
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppCMYK, GUID_WICPixelFormat32bppRGB,
    GUID_WICPixelFormat48bppBGR, GUID_WICPixelFormat48bppRGB, GUID_WICPixelFormat64bppBGRA,
    GUID_WICPixelFormat64bppPBGRA, GUID_WICPixelFormat64bppPRGBA, GUID_WICPixelFormat64bppRGBA,
    GUID_WICPixelFormatBlackWhite, IWICBitmapFrameDecode, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, STATFLAG_NONAME, STATSTG,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::{Error, GUID};

use crate::winutil::ComGuard;

/// Maximum accepted source file size (64 MiB), matching the C# implementation.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum accepted pixel count (40,000,000 pixels), matching the C# implementation.
pub const MAX_PIXELS: u64 = 40_000_000;

const FILE_SHARE_READ_ONLY: u32 = 1;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Decoded and clipboard-ready PNG and top-down 24-bit BGR DIB representations of an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub png: Vec<u8>,
    pub dib: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Outcome of attempting to read and decode a received transfer file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeOutcome {
    Image(ClipboardImage),
    Retry,
    Unreadable,
}

/// Returns whether a source file length is non-empty and within `MAX_FILE_BYTES`.
pub fn is_valid_file_len(len: u64) -> bool {
    len > 0 && len <= MAX_FILE_BYTES
}

/// Returns whether image dimensions are positive and within `MAX_PIXELS`.
pub fn is_valid_dimensions(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && (width as u64).saturating_mul(height as u64) <= MAX_PIXELS
}

fn is_sharing_or_lock_violation(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

/// Opens `path` with `FILE_SHARE_READ` only, validates size limits, and converts it via WIC.
pub fn decode_file(path: &Path) -> DecodeOutcome {
    let file = match OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if is_sharing_or_lock_violation(&err) => return DecodeOutcome::Retry,
        Err(_) => return DecodeOutcome::Unreadable,
    };

    let meta_len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) if is_sharing_or_lock_violation(&err) => return DecodeOutcome::Retry,
        Err(_) => return DecodeOutcome::Unreadable,
    };

    if !is_valid_file_len(meta_len) {
        return DecodeOutcome::Unreadable;
    }

    let mut bytes = Vec::with_capacity(meta_len as usize);
    if let Err(err) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
        return if is_sharing_or_lock_violation(&err) {
            DecodeOutcome::Retry
        } else {
            DecodeOutcome::Unreadable
        };
    }

    if !is_valid_file_len(bytes.len() as u64) {
        return DecodeOutcome::Unreadable;
    }

    let Ok(_com) = ComGuard::new(COINIT_MULTITHREADED) else {
        return DecodeOutcome::Unreadable;
    };

    match decode_bytes_with_wic(&bytes) {
        Ok(Some(image)) => DecodeOutcome::Image(image),
        Ok(None) | Err(_) => DecodeOutcome::Unreadable,
    }
}

fn decode_bytes_with_wic(bytes: &[u8]) -> windows::core::Result<Option<ClipboardImage>> {
    // SAFETY: COM is initialized on the calling thread; `bytes` remains alive and immutable for `stream`'s lifetime.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;

        let decoder = factory.CreateDecoderFromStream(
            &stream,
            ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )?;
        if decoder.GetFrameCount()? == 0 {
            return Ok(None);
        }

        let frame = decoder.GetFrame(0)?;
        let (mut width, mut height) = (0u32, 0u32);
        frame.GetSize(&mut width, &mut height)?;
        if !is_valid_dimensions(width, height) {
            return Ok(None);
        }

        let png = encode_png(&factory, &frame, width, height)?;
        let dib = encode_dib(&factory, &frame, width, height)?;
        Ok(Some(ClipboardImage {
            png,
            dib,
            width,
            height,
        }))
    }
}

fn is_indexed_png_pixel_format(format: GUID) -> bool {
    format == GUID_WICPixelFormat1bppIndexed
        || format == GUID_WICPixelFormat2bppIndexed
        || format == GUID_WICPixelFormat4bppIndexed
        || format == GUID_WICPixelFormat8bppIndexed
}

fn is_native_non_indexed_png_pixel_format(format: GUID) -> bool {
    format == GUID_WICPixelFormatBlackWhite
        || format == GUID_WICPixelFormat2bppGray
        || format == GUID_WICPixelFormat4bppGray
        || format == GUID_WICPixelFormat8bppGray
        || format == GUID_WICPixelFormat16bppGray
        || format == GUID_WICPixelFormat24bppBGR
        || format == GUID_WICPixelFormat32bppBGRA
        || format == GUID_WICPixelFormat48bppRGB
        || format == GUID_WICPixelFormat64bppRGBA
}

fn fallback_png_pixel_format(source_format: GUID) -> GUID {
    if source_format == GUID_WICPixelFormat48bppBGR {
        GUID_WICPixelFormat48bppRGB
    } else if source_format == GUID_WICPixelFormat64bppBGRA
        || source_format == GUID_WICPixelFormat64bppPBGRA
        || source_format == GUID_WICPixelFormat64bppPRGBA
    {
        GUID_WICPixelFormat64bppRGBA
    } else if source_format == GUID_WICPixelFormat24bppRGB
        || source_format == GUID_WICPixelFormat32bppBGR
        || source_format == GUID_WICPixelFormat32bppRGB
        || source_format == GUID_WICPixelFormat32bppCMYK
        || source_format == GUID_WICPixelFormat16bppBGR555
        || source_format == GUID_WICPixelFormat16bppBGR565
    {
        GUID_WICPixelFormat24bppBGR
    } else {
        GUID_WICPixelFormat32bppBGRA
    }
}

unsafe fn encode_png(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    width: u32,
    height: u32,
) -> windows::core::Result<Vec<u8>> {
    // SAFETY: Caller has initialized COM and holds valid WIC factory and frame interfaces.
    unsafe {
        let out_stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, ptr::null())?;
        encoder.Initialize(&out_stream, WICBitmapEncoderNoCache)?;

        let mut frame_encode = None;
        let mut props = None;
        encoder.CreateNewFrame(&mut frame_encode, &mut props)?;
        let frame_encode = frame_encode.ok_or_else(|| Error::from(E_FAIL))?;
        frame_encode.Initialize(props.as_ref())?;
        frame_encode.SetSize(width, height)?;

        let (mut dpi_x, mut dpi_y) = (0.0f64, 0.0f64);
        if frame.GetResolution(&mut dpi_x, &mut dpi_y).is_ok() && dpi_x > 0.0 && dpi_y > 0.0 {
            let _ = frame_encode.SetResolution(dpi_x, dpi_y);
        }

        // Preserve native PNG formats (including indexed with palette, grayscale, and 16-bit depth);
        // convert non-native formats explicitly so WIC never swaps channels or fails.
        let source_format = frame.GetPixelFormat()?;
        let mut copied_palette = None;
        let mut requested = if is_indexed_png_pixel_format(source_format) {
            if let Ok(palette) = factory.CreatePalette()
                && frame.CopyPalette(&palette).is_ok()
            {
                copied_palette = Some(palette);
                source_format
            } else {
                GUID_WICPixelFormat32bppBGRA
            }
        } else if is_native_non_indexed_png_pixel_format(source_format) {
            source_format
        } else {
            fallback_png_pixel_format(source_format)
        };

        frame_encode.SetPixelFormat(&mut requested)?;
        if is_indexed_png_pixel_format(requested)
            && let Some(ref palette) = copied_palette
        {
            frame_encode.SetPalette(palette)?;
        }

        if source_format == requested {
            frame_encode.WriteSource(frame, ptr::null())?;
        } else {
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                frame,
                &requested,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            frame_encode.WriteSource(&converter, ptr::null())?;
        }

        frame_encode.Commit()?;
        encoder.Commit()?;

        let mut stat = STATSTG::default();
        out_stream.Stat(&mut stat, STATFLAG_NONAME)?;
        let len = usize::try_from(stat.cbSize).map_err(|_| Error::from(E_FAIL))?;
        if len == 0 {
            return Err(Error::from(E_FAIL));
        }

        let hglobal = GetHGlobalFromStream(&out_stream)?;
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return Err(Error::from_win32());
        }
        let result = if GlobalSize(hglobal) >= len {
            Ok(std::slice::from_raw_parts(ptr as *const u8, len).to_vec())
        } else {
            Err(Error::from(E_FAIL))
        };
        let _ = GlobalUnlock(hglobal);
        result
    }
}

unsafe fn encode_dib(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    width: u32,
    height: u32,
) -> windows::core::Result<Vec<u8>> {
    // Top-down, uncompressed 24-bit BGR DIB with 4-byte aligned rows, matching C# EncodeDib.
    const HEADER_SIZE: usize = 40;
    let width_i32 = i32::try_from(width).map_err(|_| Error::from(E_FAIL))?;
    let height_i32 = i32::try_from(height).map_err(|_| Error::from(E_FAIL))?;
    let stride = (width as usize)
        .checked_mul(3)
        .and_then(|v| v.checked_add(3))
        .ok_or_else(|| Error::from(E_FAIL))?
        & !3;
    let stride_u32 = u32::try_from(stride).map_err(|_| Error::from(E_FAIL))?;
    let data_size = stride
        .checked_mul(height as usize)
        .ok_or_else(|| Error::from(E_FAIL))?;
    let data_size_i32 = i32::try_from(data_size).map_err(|_| Error::from(E_FAIL))?;
    let total_size = HEADER_SIZE
        .checked_add(data_size)
        .ok_or_else(|| Error::from(E_FAIL))?;

    let mut dib = vec![0u8; total_size];
    dib[0..4].copy_from_slice(&(HEADER_SIZE as i32).to_le_bytes());
    dib[4..8].copy_from_slice(&width_i32.to_le_bytes());
    dib[8..12].copy_from_slice(&(-height_i32).to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&24u16.to_le_bytes());
    dib[20..24].copy_from_slice(&data_size_i32.to_le_bytes());

    // SAFETY: `converter` writes at most `stride_u32 * height` bytes into `&mut dib[HEADER_SIZE..]`.
    unsafe {
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            frame,
            &GUID_WICPixelFormat24bppBGR,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        converter.CopyPixels(ptr::null(), stride_u32, &mut dib[HEADER_SIZE..])?;
    }

    Ok(dib)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use windows::Win32::Graphics::Imaging::{GUID_WICPixelFormat32bppRGBA, WICBitmapCacheOnLoad};

    pub(crate) fn make_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gch-rust-test-{label}-{}-{}",
            std::process::id(),
            crate::winutil::new_guid_hex().expect("guid")
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Creates a minimal top-down 24-bit BGR BMP file with 3x2 pixels:
    /// Row 0: Red (255,0,0), Green (0,255,0), Blue (0,0,255)
    /// Row 1: White (255,255,255), Black (0,0,0), Yellow (255,255,0)
    pub(crate) fn sample_3x2_bmp_bytes() -> Vec<u8> {
        let width: i32 = 3;
        let height: i32 = 2;
        let stride: usize = 12; // (3 * 3 + 3) & !3
        let pixel_bytes: usize = stride * 2;
        let file_size: u32 = (14 + 40 + pixel_bytes) as u32;
        let mut bmp = vec![0u8; file_size as usize];

        // BITMAPFILEHEADER
        bmp[0..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&file_size.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());

        // BITMAPINFOHEADER (top-down height = -2)
        bmp[14..18].copy_from_slice(&40i32.to_le_bytes());
        bmp[18..22].copy_from_slice(&width.to_le_bytes());
        bmp[22..26].copy_from_slice(&(-height).to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        bmp[34..38].copy_from_slice(&(pixel_bytes as i32).to_le_bytes());

        // Row 0 (BGR)
        let row0 = 54;
        bmp[row0..row0 + 9].copy_from_slice(&[
            0, 0, 255, // Red
            0, 255, 0, // Green
            255, 0, 0, // Blue
        ]);
        // Row 1 (BGR)
        let row1 = 54 + stride;
        bmp[row1..row1 + 9].copy_from_slice(&[
            255, 255, 255, // White
            0, 0, 0, // Black
            0, 255, 255, // Yellow
        ]);

        bmp
    }

    fn encode_rgba_png_with_wic(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let _com = ComGuard::new(COINIT_MULTITHREADED).expect("com");
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .expect("factory");
            let bitmap = factory
                .CreateBitmapFromMemory(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppRGBA,
                    width * 4,
                    rgba,
                )
                .expect("bitmap");
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).expect("stream");
            let encoder = factory
                .CreateEncoder(&GUID_ContainerFormatPng, ptr::null())
                .expect("encoder");
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .expect("enc init");
            let mut frame = None;
            let mut props = None;
            encoder
                .CreateNewFrame(&mut frame, &mut props)
                .expect("new frame");
            let frame = frame.expect("frame");
            frame.Initialize(props.as_ref()).expect("frame init");
            frame.SetSize(width, height).expect("size");
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut fmt).expect("fmt");
            let conv = factory.CreateFormatConverter().expect("conv");
            conv.Initialize(
                &bitmap,
                &fmt,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .expect("conv init");
            frame.WriteSource(&conv, ptr::null()).expect("write");
            frame.Commit().expect("frame commit");
            encoder.Commit().expect("enc commit");
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).expect("stat");
            let hglobal = GetHGlobalFromStream(&stream).expect("hglobal");
            let ptr = GlobalLock(hglobal);
            assert!(!ptr.is_null());
            let bytes = std::slice::from_raw_parts(ptr as *const u8, stat.cbSize as usize).to_vec();
            let _ = GlobalUnlock(hglobal);
            bytes
        }
    }

    fn encode_png_with_wic(
        width: u32,
        height: u32,
        format: &GUID,
        stride: u32,
        pixels: &[u8],
        palette_colors: Option<&[u32]>,
    ) -> Vec<u8> {
        let _com = ComGuard::new(COINIT_MULTITHREADED).expect("com");
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .expect("factory");
            let bitmap = factory
                .CreateBitmapFromMemory(width, height, format, stride, pixels)
                .expect("bitmap");
            let palette = palette_colors.map(|colors| {
                let pal = factory.CreatePalette().expect("palette");
                pal.InitializeCustom(colors).expect("init palette");
                bitmap.SetPalette(&pal).expect("set bitmap palette");
                pal
            });

            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).expect("stream");
            let encoder = factory
                .CreateEncoder(&GUID_ContainerFormatPng, ptr::null())
                .expect("encoder");
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .expect("enc init");
            let mut frame = None;
            let mut props = None;
            encoder
                .CreateNewFrame(&mut frame, &mut props)
                .expect("new frame");
            let frame = frame.expect("frame");
            frame.Initialize(props.as_ref()).expect("frame init");
            frame.SetSize(width, height).expect("size");
            let mut fmt = *format;
            frame.SetPixelFormat(&mut fmt).expect("fmt");
            assert_eq!(fmt, *format, "test helper expects encoder native format");
            if let Some(ref pal) = palette {
                frame.SetPalette(pal).expect("set encode palette");
            }
            frame.WriteSource(&bitmap, ptr::null()).expect("write");
            frame.Commit().expect("frame commit");
            encoder.Commit().expect("enc commit");
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).expect("stat");
            let hglobal = GetHGlobalFromStream(&stream).expect("hglobal");
            let ptr = GlobalLock(hglobal);
            assert!(!ptr.is_null());
            let bytes = std::slice::from_raw_parts(ptr as *const u8, stat.cbSize as usize).to_vec();
            let _ = GlobalUnlock(hglobal);
            bytes
        }
    }

    fn decode_png_pixel_format(png: &[u8]) -> GUID {
        let _com = ComGuard::new(COINIT_MULTITHREADED).expect("com");
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .expect("factory");
            let stream = factory.CreateStream().expect("stream");
            stream.InitializeFromMemory(png).expect("init mem");
            let decoder = factory
                .CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)
                .expect("decoder");
            let frame = decoder.GetFrame(0).expect("frame");
            frame.GetPixelFormat().expect("pixel format")
        }
    }

    fn decode_png_pixels_rgba(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let _com = ComGuard::new(COINIT_MULTITHREADED).expect("com");
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .expect("factory");
            let stream = factory.CreateStream().expect("stream");
            stream.InitializeFromMemory(png).expect("init mem");
            let decoder = factory
                .CreateDecoderFromStream(&stream, ptr::null(), WICDecodeMetadataCacheOnDemand)
                .expect("decoder");
            let frame = decoder.GetFrame(0).expect("frame");
            let (mut w, mut h) = (0, 0);
            frame.GetSize(&mut w, &mut h).expect("size");
            let bitmap = factory
                .CreateBitmapFromSource(&frame, WICBitmapCacheOnLoad)
                .expect("bitmap");
            let conv = factory.CreateFormatConverter().expect("conv");
            conv.Initialize(
                &bitmap,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .expect("conv init");
            let mut out = vec![0u8; (w * h * 4) as usize];
            conv.CopyPixels(ptr::null(), w * 4, &mut out).expect("copy");
            (w, h, out)
        }
    }

    #[test]
    fn decodes_unaligned_bmp_and_produces_exact_dib_and_png() {
        let dir = make_temp_dir("bmp-roundtrip");
        let path = dir.join("sample.bmp");
        fs::write(&path, sample_3x2_bmp_bytes()).expect("write bmp");

        let DecodeOutcome::Image(img) = decode_file(&path) else {
            panic!("expected successful decode");
        };

        assert_eq!(img.width, 3);
        assert_eq!(img.height, 2);
        assert!(img.png.starts_with(b"\x89PNG\r\n\x1a\n"));

        // DIB header checks
        assert_eq!(img.dib.len(), 40 + 24);
        assert_eq!(i32::from_le_bytes(img.dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(img.dib[4..8].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(img.dib[8..12].try_into().unwrap()), -2);
        assert_eq!(u16::from_le_bytes(img.dib[12..14].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(img.dib[14..16].try_into().unwrap()), 24);
        assert_eq!(i32::from_le_bytes(img.dib[16..20].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(img.dib[20..24].try_into().unwrap()), 24);

        // DIB pixel checks (row 0 = Red, Green, Blue; row 1 = White, Black, Yellow)
        assert_eq!(&img.dib[40..49], &[0, 0, 255, 0, 255, 0, 255, 0, 0]);
        assert_eq!(&img.dib[52..61], &[255, 255, 255, 0, 0, 0, 0, 255, 255]);

        let (pw, ph, rgba) = decode_png_pixels_rgba(&img.png);
        assert_eq!((pw, ph), (3, 2));
        assert_eq!(
            rgba,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, //
                255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 0, 255,
            ]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniffs_png_saved_with_jpg_extension_and_preserves_channels_and_alpha() {
        let dir = make_temp_dir("png-as-jpg");
        let path = dir.join("capture_temp.jpg");
        let rgba = vec![
            12, 34, 200, 128, // semi-transparent blueish pixel
            220, 40, 10, 255, // opaque reddish pixel
        ];
        let png_bytes = encode_rgba_png_with_wic(2, 1, &rgba);
        fs::write(&path, &png_bytes).expect("write png with .jpg ext");

        let DecodeOutcome::Image(img) = decode_file(&path) else {
            panic!("expected png decoded regardless of .jpg extension");
        };
        assert_eq!((img.width, img.height), (2, 1));
        let (pw, ph, decoded_rgba) = decode_png_pixels_rgba(&img.png);
        assert_eq!((pw, ph), (2, 1));
        assert_eq!(decoded_rgba, rgba);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_empty_and_non_image_files() {
        let dir = make_temp_dir("invalid-files");

        let empty = dir.join("empty.png");
        fs::write(&empty, b"").expect("write empty");
        assert_eq!(decode_file(&empty), DecodeOutcome::Unreadable);

        let garbage = dir.join("garbage.png");
        fs::write(&garbage, b"not an image at all").expect("write garbage");
        assert_eq!(decode_file(&garbage), DecodeOutcome::Unreadable);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_retry_on_exclusive_sharing_violation() {
        let dir = make_temp_dir("sharing-violation");
        let path = dir.join("locked.bmp");
        fs::write(&path, sample_3x2_bmp_bytes()).expect("write bmp");

        let _lock = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .expect("exclusive lock");

        assert_eq!(decode_file(&path), DecodeOutcome::Retry);
        drop(_lock);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforces_file_size_and_pixel_count_limits() {
        assert!(!is_valid_file_len(0));
        assert!(is_valid_file_len(1));
        assert!(is_valid_file_len(MAX_FILE_BYTES));
        assert!(!is_valid_file_len(MAX_FILE_BYTES + 1));

        assert!(!is_valid_dimensions(0, 100));
        assert!(!is_valid_dimensions(100, 0));
        assert!(is_valid_dimensions(8000, 5000)); // 40,000,000
        assert!(!is_valid_dimensions(8000, 5001)); // 40,008,000
        assert!(!is_valid_dimensions(u32::MAX, 2));
    }

    #[test]
    fn preserves_grayscale_indexed_and_16bit_png_pixel_formats() {
        let dir = make_temp_dir("preserve-png-formats");

        // 1. 8bppGray
        let gray_path = dir.join("gray8.png");
        let gray_png =
            encode_png_with_wic(2, 1, &GUID_WICPixelFormat8bppGray, 2, &[0x20, 0xE0], None);
        fs::write(&gray_path, &gray_png).expect("write gray8");
        let DecodeOutcome::Image(gray_img) = decode_file(&gray_path) else {
            panic!("expected gray8 decode");
        };
        assert_eq!(
            decode_png_pixel_format(&gray_img.png),
            GUID_WICPixelFormat8bppGray
        );
        let (_, _, gray_rgba) = decode_png_pixels_rgba(&gray_img.png);
        assert_eq!(
            gray_rgba,
            vec![0x20, 0x20, 0x20, 255, 0xE0, 0xE0, 0xE0, 255]
        );

        // 2. 8bppIndexed with custom palette (0 = Red, 1 = Semi-transparent Green)
        let indexed_path = dir.join("indexed8.png");
        let palette = [0xFFFF_0000, 0x8000_FF00];
        let indexed_png = encode_png_with_wic(
            2,
            1,
            &GUID_WICPixelFormat8bppIndexed,
            2,
            &[0, 1],
            Some(&palette),
        );
        fs::write(&indexed_path, &indexed_png).expect("write indexed8");
        let DecodeOutcome::Image(indexed_img) = decode_file(&indexed_path) else {
            panic!("expected indexed8 decode");
        };
        assert_eq!(
            decode_png_pixel_format(&indexed_img.png),
            GUID_WICPixelFormat8bppIndexed
        );
        let (_, _, indexed_rgba) = decode_png_pixels_rgba(&indexed_img.png);
        assert_eq!(indexed_rgba, vec![255, 0, 0, 255, 0, 255, 0, 0x80]);

        // 3. 48bppRGB (16-bit per channel)
        let rgb48_path = dir.join("rgb48.png");
        let mut rgb48 = Vec::with_capacity(12);
        for channel in [
            0x1234u16, 0x5678u16, 0x9ABCu16, 0xFFFFu16, 0x0000u16, 0x8000u16,
        ] {
            rgb48.extend_from_slice(&channel.to_le_bytes());
        }
        let rgb48_png = encode_png_with_wic(2, 1, &GUID_WICPixelFormat48bppRGB, 12, &rgb48, None);
        fs::write(&rgb48_path, &rgb48_png).expect("write rgb48");
        let DecodeOutcome::Image(rgb48_img) = decode_file(&rgb48_path) else {
            panic!("expected rgb48 decode");
        };
        assert_eq!(
            decode_png_pixel_format(&rgb48_img.png),
            GUID_WICPixelFormat48bppRGB
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
