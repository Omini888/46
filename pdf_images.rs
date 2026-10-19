use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::process::Command;

use flate2::read::ZlibDecoder;
use image::{DynamicImage, GrayImage, ImageFormat, Rgb, RgbImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use tempfile::Builder;
use tracing::{debug, warn};

/// Извлекает все встроенные растровые фото из PDF, сгруппированные по номеру страницы (начиная с 1)
pub fn extract_embedded_photos_by_page(
    doc: &Document,
    min_dim: u32,
) -> BTreeMap<u32, Vec<Vec<u8>>> {
    let mut pages_map = BTreeMap::new();

    for (page_num, page_id) in doc.get_pages() {
        let mut png_images = Vec::new();
        collect_images_from_object(doc, page_id, min_dim, 0, &mut png_images);
        if !png_images.is_empty() {
            pages_map.insert(page_num, png_images);
        }
    }

    pages_map
}

/// Пытается отрендерить конкретную страницу PDF (page_num, начиная с 1) в единый PNG-снимок
/// через системный `pdftoppm` (если он установлен на Linux VM).
pub fn render_full_page_to_png(pdf_bytes: &[u8], page_num: u32) -> Option<Vec<u8>> {
    let mut tmp_pdf = Builder::new().suffix(".pdf").tempfile().ok()?;
    tmp_pdf.write_all(pdf_bytes).ok()?;
    let pdf_path = tmp_pdf.path().to_path_buf();

    let page_str = page_num.to_string();

    // pdftoppm -png -r 150 -f <page> -l <page> <input.pdf> (выводит PNG прямо в stdout)
    let output = Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            "150",
            "-f",
            &page_str,
            "-l",
            &page_str,
            pdf_path.to_str()?,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            debug!(
                "Страница {} успешно отрендерена целиком через pdftoppm ({} байт)",
                page_num,
                out.stdout.len()
            );
            Some(out.stdout)
        }
        Ok(out) => {
            warn!(
                "pdftoppm вернул ненулевой код на стр. {}: {}",
                page_num,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(_) => {
            // pdftoppm не установлен — используем встроенные изображения из lopdf
            None
        }
    }
}

fn collect_images_from_object(
    doc: &Document,
    obj_id: ObjectId,
    min_dim: u32,
    depth: u8,
    out_pngs: &mut Vec<Vec<u8>>,
) {
    if depth > 6 {
        return;
    }

    let dict = match doc.get_object(obj_id) {
        Ok(Object::Dictionary(d)) => d,
        Ok(Object::Stream(s)) => &s.dict,
        _ => return,
    };

    let resources_dict = match dict.get(b"Resources") {
        Ok(Object::Reference(res_id)) => doc.get_dictionary(*res_id).ok(),
        Ok(Object::Dictionary(d)) => Some(d),
        _ => None,
    };

    let Some(res) = resources_dict else {
        return;
    };

    let xobject_dict = match res.get(b"XObject") {
        Ok(Object::Reference(xobj_id)) => doc.get_dictionary(*xobj_id).ok(),
        Ok(Object::Dictionary(d)) => Some(d),
        _ => None,
    };

    let Some(xobjects) = xobject_dict else {
        return;
    };

    for (_name, xobj_ref) in xobjects.iter() {
        let target_id = match xobj_ref {
            Object::Reference(id) => *id,
            _ => continue,
        };

        let Ok(Object::Stream(stream)) = doc.get_object(target_id) else {
            continue;
        };

        let subtype = stream
            .dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .unwrap_or(b"");

        if subtype == b"Image" {
            if let Some(png_bytes) = decode_pdf_image_stream(doc, stream, min_dim) {
                out_pngs.push(png_bytes);
            }
        } else if subtype == b"Form" {
            collect_images_from_object(doc, target_id, min_dim, depth + 1, out_pngs);
        }
    }
}

fn decode_pdf_image_stream(doc: &Document, stream: &Stream, min_dim: u32) -> Option<Vec<u8>> {
    let width = stream.dict.get(b"Width").and_then(|o| o.as_i64()).ok()? as u32;
    let height = stream.dict.get(b"Height").and_then(|o| o.as_i64()).ok()? as u32;

    if width < min_dim || height < min_dim {
        return None;
    }

    let filter_name = extract_filter_name(doc, &stream.dict);

    // 1. Встроенный JPEG (DCTDecode)
    if filter_name.contains("DCTDecode") {
        if let Ok(img) = image::load_from_memory(&stream.content) {
            let mut png_buf = Vec::new();
            if img
                .write_to(&mut Cursor::new(&mut png_buf), ImageFormat::Png)
                .is_ok()
            {
                return Some(png_buf);
            }
        }
    }

    // 2. Сжатый растровый поток (FlateDecode или Raw)
    let raw_pixels = if let Ok(decompressed) = stream.decompressed_content() {
        decompressed
    } else {
        let mut decoder = ZlibDecoder::new(&stream.content[..]);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).ok()?;
        buf
    };

    let bits = stream
        .dict
        .get(b"BitsPerComponent")
        .and_then(|o| o.as_i64())
        .unwrap_or(8);

    let color_space = extract_color_space_name(doc, &stream.dict);

    let dyn_image: DynamicImage = if bits == 8 {
        let expected_rgb = (width * height * 3) as usize;
        let expected_gray = (width * height) as usize;
        let expected_cmyk = (width * height * 4) as usize;

        if color_space.contains("Gray") || raw_pixels.len() == expected_gray {
            let gray = GrayImage::from_raw(width, height, raw_pixels)?;
            DynamicImage::ImageLuma8(gray)
        } else if color_space.contains("CMYK") && raw_pixels.len() >= expected_cmyk {
            let mut rgb_img = RgbImage::new(width, height);
            for (i, pixel) in rgb_img.pixels_mut().enumerate() {
                let idx = i * 4;
                let c = raw_pixels[idx] as f32 / 255.0;
                let m = raw_pixels[idx + 1] as f32 / 255.0;
                let y = raw_pixels[idx + 2] as f32 / 255.0;
                let k = raw_pixels[idx + 3] as f32 / 255.0;

                let r = ((1.0 - c) * (1.0 - k) * 255.0) as u8;
                let g = ((1.0 - m) * (1.0 - k) * 255.0) as u8;
                let b = ((1.0 - y) * (1.0 - k) * 255.0) as u8;
                *pixel = Rgb([r, g, b]);
            }
            DynamicImage::ImageRgb8(rgb_img)
        } else if raw_pixels.len() >= expected_rgb {
            let trimmed = raw_pixels.into_iter().take(expected_rgb).collect();
            let rgb = RgbImage::from_raw(width, height, trimmed)?;
            DynamicImage::ImageRgb8(rgb)
        } else {
            return None;
        }
    } else if bits == 1 {
        let row_bytes = ((width + 7) / 8) as usize;
        if raw_pixels.len() < row_bytes * (height as usize) {
            return None;
        }
        let mut gray = GrayImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let byte_idx = (y as usize) * row_bytes + (x as usize) / 8;
                let bit_offset = 7 - ((x as usize) % 8);
                let bit = (raw_pixels[byte_idx] >> bit_offset) & 1;
                let luma = if bit == 1 { 255u8 } else { 0u8 };
                gray.put_pixel(x, y, image::Luma([luma]));
            }
        }
        DynamicImage::ImageLuma8(gray)
    } else {
        return None;
    };

    let mut png_buf = Vec::new();
    dyn_image
        .write_to(&mut Cursor::new(&mut png_buf), ImageFormat::Png)
        .ok()?;

    Some(png_buf)
}

fn extract_filter_name(doc: &Document, dict: &Dictionary) -> String {
    let Ok(filter_obj) = dict.get(b"Filter") else {
        return String::new();
    };
    let resolved = doc
        .dereference(filter_obj)
        .map(|(_, o)| o)
        .unwrap_or(filter_obj);
    match resolved {
        Object::Name(n) => String::from_utf8_lossy(n).to_string(),
        Object::Array(arr) => arr
            .iter()
            .filter_map(|item| item.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).to_string())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    }
}

fn extract_color_space_name(doc: &Document, dict: &Dictionary) -> String {
    let Ok(cs_obj) = dict.get(b"ColorSpace") else {
        return "DeviceRGB".to_string();
    };
    let resolved = doc.dereference(cs_obj).map(|(_, o)| o).unwrap_or(cs_obj);
    match resolved {
        Object::Name(n) => String::from_utf8_lossy(n).to_string(),
        Object::Array(arr) => arr
            .first()
            .and_then(|first| first.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).to_string())
            .unwrap_or_else(|| "DeviceRGB".to_string()),
        _ => "DeviceRGB".to_string(),
    }
}
