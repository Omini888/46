use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use image::ImageFormat;
use lopdf::Document;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::AppState;
use crate::pdf_images::extract_embedded_photos_by_page;
use crate::pdf_renderer::render_page_pure_rust;
use crate::vlm::{
    describe_standalone_photo_in_vlm, ocr_photo_page_in_vlm, process_page_with_figure_in_vlm,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenWebUiDocument {
    pub page_content: String,
    pub metadata: Value,
}

/// Обработка одиночного файла-картинки (.png, .jpg, .webp)
pub async fn process_single_image_file(
    state: Arc<AppState>,
    filename: &str,
    content_type: &str,
    body: Bytes,
) -> Result<Vec<OpenWebUiDocument>> {
    let img = image::load_from_memory(&body).context("Не удалось декодировать изображение")?;
    let mut png_bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)?;

    let recognized = ocr_photo_page_in_vlm(&state, &[png_bytes], filename, 1).await?;
    let page_text = append_page_number(&recognized, 1, 1);

    Ok(vec![OpenWebUiDocument {
        page_content: page_text,
        metadata: json!({
            "source": filename,
            "content_type": content_type,
            "page": 1,
            "total_pages": 1,
            "mode": "direct_image_vlm"
        }),
    }])
}

/// Главный конвейер постраничной обработки PDF-документа на чистом Rust
pub async fn process_pdf_document(
    state: Arc<AppState>,
    filename: &str,
    content_type: &str,
    pdf_bytes: Bytes,
) -> Result<Vec<OpenWebUiDocument>> {
    let pdf_clone = pdf_bytes.clone();
    let min_dim = state.config.min_image_dim;
    let min_text_chars = state.config.min_text_chars_per_page;
    let figure_regex = state.figure_regex.clone();

    // Выполняем весь разбор PDF и чисто-растовский рендер нужных страниц за один проход в пуле потоков
    let (extracted_texts, mut embedded_photos_map, mut rendered_full_pages_map, total_pages) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let texts = match pdf_extract::extract_text_from_mem_by_pages(&pdf_clone) {
                Ok(pages) => pages,
                Err(e) => {
                    warn!(
                        "pdf-extract не смог разбить по страницам ({}), пробуем целиком",
                        e
                    );
                    let single = pdf_extract::extract_text_from_mem(&pdf_clone).unwrap_or_default();
                    vec![single]
                }
            };

            let mut full_pages_png: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
            let (photos_map, lopdf_page_cnt) = match Document::load_mem(&pdf_clone) {
                Ok(doc) => {
                    let cnt = doc.get_pages().len();
                    let map = extract_embedded_photos_by_page(&doc, min_dim);
                    let total = texts.len().max(cnt).max(1);

                    // Рендерим страницу целиком на чистом Rust (tiny-skia + lopdf), если:
                    // 1) На странице есть слово "рисунок" / "рис."
                    // 2) Или на странице нет текстового слоя (страница-скан / фото)
                    for idx in 0..total {
                        let page_num = (idx + 1) as u32;
                        let page_text = texts.get(idx).map(|s| s.as_str()).unwrap_or("");
                        let non_ws = page_text.chars().filter(|c| !c.is_whitespace()).count();
                        let needs_full_render =
                            figure_regex.is_match(page_text) || non_ws < min_text_chars;

                        if needs_full_render {
                            if let Some(png) = render_page_pure_rust(&doc, page_num, 150.0) {
                                full_pages_png.insert(page_num, png);
                            }
                        }
                    }

                    (map, cnt)
                }
                Err(e) => {
                    warn!("Не удалось разобрать PDF через lopdf: {}", e);
                    (BTreeMap::new(), 0)
                }
            };

            let total = texts.len().max(lopdf_page_cnt).max(1);
            Ok((texts, photos_map, full_pages_png, total))
        })
        .await??;

    info!(
        "📘 Файл '{}': всего страниц = {}, отрендерено полных страниц = {}, страниц с отд. фото = {}",
        filename,
        total_pages,
        rendered_full_pages_map.len(),
        embedded_photos_map.len()
    );

    let mut output_pages = Vec::with_capacity(total_pages);

    for page_idx in 0..total_pages {
        let page_num = (page_idx + 1) as u32;
        let raw_text = extracted_texts
            .get(page_idx)
            .cloned()
            .unwrap_or_default();

        let non_ws_chars = raw_text.chars().filter(|c| !c.is_whitespace()).count();
        let has_meaningful_text = non_ws_chars >= state.config.min_text_chars_per_page;
        let has_word_risunok = state.figure_regex.is_match(&raw_text);

        let embedded_photos = embedded_photos_map.remove(&page_num).unwrap_or_default();
        let rendered_full_page = rendered_full_pages_map.remove(&page_num);

        let mut page_body_markdown: String;
        let page_mode: &'static str;

        // ====================================================================
        // СЦЕНАРИЙ 1: На странице есть слово "рисунок" / "рис."
        // Отдельные фото НЕ берём — вся страница целиком (отрендеренная нашим
        // Rust-рендерером) уходит в VLM вместе с текстом страницы.
        // ====================================================================
        if has_word_risunok {
            page_mode = "full_page_figure_vlm";
            info!(
                "🖼 Стр. {}/{}: найдено слово 'рисунок'. Отправляем отрендеренную страницу в VLM...",
                page_num, total_pages
            );

            let images_for_vlm: Vec<Vec<u8>> = if let Some(full_png) = rendered_full_page {
                vec![full_png]
            } else {
                embedded_photos
            };

            if !images_for_vlm.is_empty() {
                match process_page_with_figure_in_vlm(
                    &state,
                    &images_for_vlm,
                    &raw_text,
                    filename,
                    page_num,
                )
                .await
                {
                    Ok(vlm_merged_page) if !vlm_merged_page.trim().is_empty() => {
                        page_body_markdown = vlm_merged_page;
                    }
                    Ok(_) => {
                        page_body_markdown = raw_text.trim().to_string();
                    }
                    Err(e) => {
                        warn!(
                            "⚠️ Ошибка VLM на стр. {} со словом 'рисунок': {:#}. Оставляем исходный текст.",
                            page_num, e
                        );
                        page_body_markdown = raw_text.trim().to_string();
                    }
                }
            } else {
                page_body_markdown = raw_text.trim().to_string();
            }
        }
        // ====================================================================
        // СЦЕНАРИЙ 2: Страница не содержит текста (страница-фото / скан)
        // ====================================================================
        else if !has_meaningful_text {
            page_mode = "photo_scan_page_vlm";
            info!(
                "📷 Стр. {}/{}: текстовый слой пуст ({} симв.) — распознаём страницу как фото в VLM...",
                page_num, total_pages, non_ws_chars
            );

            let images_for_ocr: Vec<Vec<u8>> = if !embedded_photos.is_empty() {
                embedded_photos
            } else if let Some(full_png) = rendered_full_page {
                vec![full_png]
            } else {
                Vec::new()
            };

            if !images_for_ocr.is_empty() {
                match ocr_photo_page_in_vlm(&state, &images_for_ocr, filename, page_num).await {
                    Ok(ocr_text) => {
                        page_body_markdown = ocr_text;
                    }
                    Err(e) => {
                        warn!("⚠️ Ошибка VLM на фото-странице {}: {:#}", page_num, e);
                        page_body_markdown = raw_text.trim().to_string();
                    }
                }
            } else {
                page_body_markdown = raw_text.trim().to_string();
            }
        }
        // ====================================================================
        // СЦЕНАРИЙ 3: Обычная страница БЕЗ слова "рисунок".
        // Если на ней есть просто фотографии — отправляем каждое фото в VLM.
        // ====================================================================
        else {
            page_mode = if embedded_photos.is_empty() {
                "text_only"
            } else {
                "text_plus_standalone_photos_vlm"
            };

            page_body_markdown = raw_text.trim().to_string();

            if !embedded_photos.is_empty() {
                info!(
                    "📸 Стр. {}/{}: слова 'рисунок' нет, но найдено {} фото. Отправляем фото в VLM...",
                    page_num,
                    total_pages,
                    embedded_photos.len()
                );

                let mut tasks = Vec::with_capacity(embedded_photos.len());
                for (idx, photo_png) in embedded_photos.into_iter().enumerate() {
                    let st = state.clone();
                    let fname = filename.to_string();
                    let photo_num = idx + 1;

                    tasks.push(tokio::spawn(async move {
                        let res = describe_standalone_photo_in_vlm(
                            &st, &photo_png, &fname, page_num, photo_num,
                        )
                        .await;
                        (photo_num, res)
                    }));
                }

                for task in tasks {
                    match task.await {
                        Ok((photo_num, Ok(desc))) => {
                            if !desc.trim().is_empty() {
                                page_body_markdown.push_str(&format!(
                                    "\n\n#### Распознанное фото {} (Страница {})\n\n{}\n",
                                    photo_num,
                                    page_num,
                                    desc.trim()
                                ));
                            }
                        }
                        Ok((photo_num, Err(e))) => {
                            warn!(
                                "⚠️ Ошибка VLM на стр. {} (фото {}): {:#}",
                                page_num, photo_num, e
                            );
                        }
                        Err(join_err) => {
                            warn!(
                                "⚠️ Ошибка потока Tokio на стр. {}: {:#}",
                                page_num, join_err
                            );
                        }
                    }
                }
            }
        }

        let final_page_text_with_number =
            append_page_number(&page_body_markdown, page_num, total_pages as u32);

        output_pages.push(OpenWebUiDocument {
            page_content: final_page_text_with_number,
            metadata: json!({
                "source": filename,
                "content_type": content_type,
                "page": page_num,
                "total_pages": total_pages,
                "processing_mode": page_mode
            }),
        });
    }

    info!("✅ Обработка файла '{}' завершена ({} стр.)", filename, total_pages);
    Ok(output_pages)
}

fn append_page_number(text: &str, page_num: u32, total_pages: u32) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        format!("*(Пустая страница)*\n\n[Страница {} из {}]", page_num, total_pages)
    } else {
        format!("{}\n\n[Страница {} из {}]", trimmed, page_num, total_pages)
    }
}
