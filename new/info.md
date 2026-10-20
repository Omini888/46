Мы полностью отказываемся от внешней системной утилиты ⁠pdftoppm⁠ (⁠poppler-utils⁠) и крейта ⁠tempfile⁠. Вместо них внедряем собственный рендерер страниц PDF на чистом Rust с использованием крейтов ⁠tiny-skia⁠, ⁠lopdf⁠, ⁠image⁠ и ⁠ab_glyph⁠ из локального ⁠vendor/⁠.

Выполни следующие 5 шагов:
В файле Cargo.toml :
• Удали зависимость tempfile =
"3.15").
• Добавь зависимости tiny-skia =
"O.11" n ab_glyph = "0.2").
2. Создай новый файл (src/
pof_renderer. Is :
• Вставь в него полный код модуля (pdf_renderer (реализует разбор MediaBox / СгорВох , матриц трансформации СТМ , стека графического состояния q / Q / ст , векторных
путей т, l, (с), (v, у, h, re, заливок и
обводок S, (s, f, f*, В, В*, b, рекурсивную отрисовку Form XObject, вклейку растровых Image xObject по матрице координат и отрисовку текста через ab_glyph ).


3. В файле src/pdf_images.Is :
• Удали
Функцию render_full_page_to_png И
импорты std: process:: Command, tem pfile::Builder.
• Сделай функцию декодирования потока изображения публичной внутри крейта: pub fn
decode_pdf_image_to_dynamic(doc:
&Document, stream: &stream, min_dim: u32) ->
Option<DynamicImage> , чтобы её могли
использовать
u extract_embedded_photos_by_page,
и новый рендерер src/
pdf_renderer. Is.
4. Вфайле src/processor. Is :
• Замени
импорт render_full_page_to_png
на use
crate:: pdf_renderer:: render_page_pu re_rust; .
• Вфункции procecs_pdf_document
рендери стран (V
• СЛОВОМ
«рисунок» (Сценарий 1) и страницы-



В файле src/processor. Is :
• Замени
импорт render_full_page_to_png
на use
crate:: pdf_renderer:: render_page_pu re_rust;.
• Вфункции process_pdf_document
рендери страницы со словом «рисунок» (Сценарий 1) и страницы-сканы без текста (Сценарий 2)
через render_page _pure_rust (&doc,
page_num, 150.0) прямо внутри (spawn_blocking, используя уже загруженный в память объект lopdf: : Document (чтобы не парсить PDF повторно на каждой странице).
5. В файле src/main.Is :
• Добавь объявление модуля mod pdf_renderer; в начало файла.
