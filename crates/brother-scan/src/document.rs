use std::{io::Cursor, path::Path};

use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref};

use crate::{Capture, Result, protocol};

/// Validate every page and assemble a PDF without resampling its JPEG contents.
/// Call on a blocking worker. The destination must not already exist.
pub fn assemble_pdf(capture: &Capture, destination: &Path) -> Result<()> {
    if capture.pages.is_empty() || capture.pages.len() > 1000 || capture.dpi == 0 {
        return Err(protocol("invalid capture metadata"));
    }
    let mut pdf = Pdf::new();
    let catalog = Ref::new(1);
    let tree = Ref::new(2);
    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree)
        .kids((0..capture.pages.len()).map(|i| Ref::new(3 + i as i32 * 3)))
        .count(capture.pages.len() as i32);
    for (index, page) in capture.pages.iter().enumerate() {
        let mut jpeg = std::fs::read(&page.file)?;
        correct_height(&mut jpeg, page.lines)?;
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&jpeg));
        decoder.set_max_decoding_buffer_size(32 * 1024 * 1024);
        decoder
            .decode()
            .map_err(|e| protocol(format!("invalid JPEG: {e}")))?;
        let info = decoder
            .info()
            .ok_or_else(|| protocol("missing JPEG dimensions"))?;
        if ![
            jpeg_decoder::PixelFormat::L8,
            jpeg_decoder::PixelFormat::RGB24,
        ]
        .contains(&info.pixel_format)
            || info.height as u32 != page.lines
        {
            return Err(protocol("unexpected JPEG color space or height"));
        }
        let width = f32::from(info.width) * 72.0 / f32::from(capture.dpi);
        let height = f32::from(info.height) * 72.0 / f32::from(capture.dpi);
        let page_id = Ref::new(3 + index as i32 * 3);
        let image_id = Ref::new(4 + index as i32 * 3);
        let content_id = Ref::new(5 + index as i32 * 3);
        let mut page_writer = pdf.page(page_id);
        page_writer
            .parent(tree)
            .media_box(Rect::new(0.0, 0.0, width, height))
            .contents(content_id);
        page_writer
            .resources()
            .x_objects()
            .pair(Name(b"Scan"), image_id);
        drop(page_writer);
        let mut image = pdf.image_xobject(image_id, &jpeg);
        image.filter(Filter::DctDecode);
        image
            .width(i32::from(info.width))
            .height(i32::from(info.height));
        // CLR describes the request; the device can still emit a three-component JPEG.
        match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => {
                image.color_space().device_gray();
            }
            jpeg_decoder::PixelFormat::RGB24 => {
                image.color_space().device_rgb();
            }
            _ => unreachable!(),
        }
        image.bits_per_component(8);
        drop(image);
        let mut content = Content::new();
        content
            .save_state()
            .transform([width, 0.0, 0.0, height, 0.0, 0.0])
            .x_object(Name(b"Scan"))
            .restore_state();
        pdf.stream(content_id, &content.finish());
    }
    use std::io::Write;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    output.write_all(&pdf.finish())?;
    output.sync_all()?;
    Ok(())
}

fn correct_height(data: &mut [u8], lines: u32) -> Result<()> {
    if !data.starts_with(&[0xff, 0xd8]) || !data.ends_with(&[0xff, 0xd9]) {
        return Err(protocol("incomplete JPEG"));
    }
    let mut position = 2;
    while position + 4 <= data.len() {
        if data[position] != 0xff {
            return Err(protocol("invalid JPEG marker"));
        }
        let marker = data[position + 1];
        let length = usize::from(u16::from_be_bytes([data[position + 2], data[position + 3]]));
        if length < 2 || position + 2 + length > data.len() {
            return Err(protocol("invalid JPEG segment"));
        }
        if [0xc0, 0xc1, 0xc2].contains(&marker) {
            if length < 8 {
                return Err(protocol("short JPEG frame"));
            }
            let height = u16::from_be_bytes([data[position + 5], data[position + 6]]);
            let width = u16::from_be_bytes([data[position + 7], data[position + 8]]);
            if lines == 0 || lines > u32::from(height) || lines > 6000 || width == 0 || width > 4096
            {
                return Err(protocol("JPEG dimensions outside supported bounds"));
            }
            // The device reports cumulative acquired lines in the final image record.
            // Alter metadata only: compressed samples, including width padding, stay intact.
            data[position + 5..position + 7].copy_from_slice(&(lines as u16).to_be_bytes());
            return Ok(());
        }
        if marker == 0xda {
            return Err(protocol("JPEG scan precedes frame"));
        }
        position += 2 + length;
    }
    Err(protocol("missing JPEG frame"))
}
