use std::sync::Arc;
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use tracing::info;

use crate::config::AppState;

/// Режим 1: На странице найдено слово "рисунок".
/// Отправляем страницу (и/или её изображения) + извлечённый текст страницы в VLM,
/// чтобы нейросеть распознала рисунок и вставила его описание ровно в нужное место текста страницы.
pub async fn process_page_with_figure_in_vlm(
    state: &Arc<AppState>,
    page_images: &[Vec<u8>],
    raw_page_text: &str,
    filename: &str,
    page_num: u32,
) -> Result<String> {
    let prompt = format!(
        "Перед тобой страница №{} из документа '{}'.\n\
         На этой странице упоминается рисунок/иллюстрация.\n\
         Ниже приведён извлечённый текстовый слой этой страницы:\n\
         --- НАЧАЛО ТЕКСТА СТРАНИЦЫ ---\n\
         {}\n\
         --- КОНЕЦ ТЕКСТА СТРАНИЦЫ ---\n\n\
         Твоя задача:\n\
         1. Внимательно изучи приложенное изображение страницы (или рисунков с неё).\n\
         2. Распознай всё, что изображено на рисунке/схеме/таблице (весь текст, подписи, цифры, связи элементов).\n\
         3. Верни ПОЛНЫЙ текст этой страницы в формате Markdown, вставив подробное описание распознанного рисунка \
         ровно в то место страницы, где этот рисунок расположен по смыслу (рядом с его подписью или ссылкой на него).\n\
         4. Не удаляй исходный текст страницы и не пиши вводных фраз от себя.",
        page_num,
        filename,
        raw_page_text.trim()
    );

    call_vlm_multimodal(state, page_images, &prompt, page_num, "full-page-figure").await
}

/// Режим 2: На странице НЕТ слова "рисунок", но есть встроенное фото.
/// Распознаём само фото для добавления к тексту страницы.
pub async fn describe_standalone_photo_in_vlm(
    state: &Arc<AppState>,
    png_bytes: &[u8],
    filename: &str,
    page_num: u32,
    photo_num: usize,
) -> Result<String> {
    let prompt = format!(
        "Это фотография / изображение №{} со страницы {} документа '{}'.\n\
         Извлеки весь видимый текст, цифры, таблицы и подробно опиши, что изображено на фото, \
         в формате Markdown. Не пиши вводных слов, сразу выдавай результат.",
        photo_num, page_num, filename
    );

    call_vlm_multimodal(
        state,
        &[png_bytes.to_vec()],
        &prompt,
        page_num,
        &format!("photo-{}", photo_num),
    )
    .await
}

/// Режим 3: Вся страница PDF (или весь загруженный файл) является фото-сканом без текстового слоя.
pub async fn ocr_photo_page_in_vlm(
    state: &Arc<AppState>,
    page_images: &[Vec<u8>],
    filename: &str,
    page_num: u32,
) -> Result<String> {
    let prompt = format!(
        "Это страница №{} из документа '{}' (представляет собой фото или скан).\n\
         Полностью распознай весь текст на этой странице с сохранением структуры абзацев и таблиц в Markdown. \
         Если на странице есть фотографии, печати, графики или схемы — подробно опиши их прямо на тех местах, \
         где они находятся. Не пиши вводных комментариев.",
        page_num, filename
    );

    call_vlm_multimodal(state, page_images, &prompt, page_num, "scan-page").await
}

/// Базовая функция отправки одного или нескольких PNG-изображений и текстового промпта в VLM
async fn call_vlm_multimodal(
    state: &Arc<AppState>,
    images: &[Vec<u8>],
    prompt_text: &str,
    page_num: u32,
    tag: &str,
) -> Result<String> {
    if images.is_empty() {
        return Err(anyhow!("Список изображений для VLM пуст"));
    }

    // Вычисляем общий хеш blake3 от промпта и всех переданных картинок для кэша
    let mut hasher = blake3::Hasher::new();
    hasher.update(prompt_text.as_bytes());
    for img in images {
        hasher.update(img);
    }
    let cache_key = hasher.finalize().to_hex().to_string();

    if let Some(cached) = state.vlm_cache.get(&cache_key).await {
        info!(
            "⚡ Стр. {} ({}): результат взят из кэша blake3 ({})",
            page_num,
            tag,
            &cache_key[..10]
        );
        return Ok(cached);
    }

    let _permit = state
        .vlm_semaphore
        .acquire()
        .await
        .map_err(|_| anyhow!("Семафор VLM закрыт"))?;

    // Собираем массив content: сначала текст промпта, затем все изображения в Base64
    let mut content_array = Vec::with_capacity(1 + images.len());
    content_array.push(json!({
        "type": "text",
        "text": prompt_text
    }));

    for img_bytes in images {
        let b64 = BASE64.encode(img_bytes);
        content_array.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:image/png;base64,{}", b64)
            }
        }));
    }

    let payload = json!({
        "model": state.config.vlm_model,
        "temperature": 0.1,
        "max_tokens": 3072,
        "reasoning_effort": "none",
        "chat_template_kwargs": {
            "enable_thinking": false,
            "thinking": false
        },
        "messages": [
            {
                "role": "user",
                "content": content_array
            }
        ]
    });

    let mut req = state
        .http_client
        .post(&state.config.vlm_api_url)
        .json(&payload);

    if !state.config.vlm_api_key.is_empty() && state.config.vlm_api_key != "EMPTY" {
        req = req.bearer_auth(&state.config.vlm_api_key);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("Не удалось достучаться до VLM по адресу {}", state.config.vlm_api_url))?;

    let status = resp.status();
    let resp_text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!(
            "VLM вернула ошибку HTTP {}: {}",
            status,
            resp_text
        ));
    }

    let parsed: Value =
        serde_json::from_str(&resp_text).context("Ответ от VLM не является валидным JSON")?;

    let raw_answer = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("В ответе VLM нет поля choices[0].message.content"))?;

    let clean_answer = strip_think_tags(raw_answer);

    state
        .vlm_cache
        .insert(cache_key, clean_answer.clone())
        .await;

    Ok(clean_answer)
}

/// Очищает ответ от тегов <think>...</think>, если модель их сгенерировала
fn strip_think_tags(input: &str) -> String {
    if let Some(end_idx) = input.find("</think>") {
        input[end_idx + "</think>".len()..].trim().to_string()
    } else {
        input.trim().to_string()
    }
}
