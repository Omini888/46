Подключение в OpenWebUI (Settings -> Admin -> Documents):   Content Extraction Engine: External   External Document Loader URL: [http://127.0.0.1:5000](http://127.0.0.1:5000) (без /process на конце — OpenWebUI подставит /process сам!)   External Document Loader API Key:    my-secret-gost-key-2026Ниже в том же разделе включите Text Splitter: Markdown Header (или укажите это в переменных окружения OpenWebUI), чтобы база знаний резала полученный от нашего Rust-сервера 

2. Как теперь работает логика по вашим правилам (4 сценария)

Для каждой страницы PDF (от 1 до N) сервер проверяет текст и действует строго по вашему алгоритму:

    Сценарий 1: В тексте страницы найдено слово «рисунок» / «рис.»

        Отдельные фотографии с этой страницы не дёргаются.

        Вся страница целиком уходит в VLM вместе с её исходным текстом.

        Промпт заставляет VLM распознать рисунок/схему и вставить подробное описание прямо внутрь текста страницы на то место, где упоминается рисунок, не теряя остальной текст страницы.

    Сценарий 2: Страница PDF — это само фото / скан (текстового слоя нет или меньше 20 символов)

        Страница целиком (или фото-скан с неё) отправляется в VLM в режиме полного распознавания текста и визуальных элементов.

    Сценарий 3: Обычная страница с текстом (слова «рисунок» нет), но на ней есть встроенные фото

        Текст страницы сохраняется как есть, а каждое найденное на странице фото отдельно отправляется в VLM. Ответы нейросети добавляются на эту страницу.

    Сценарий 4: В сам OpenWebUI загрузили просто файл картинки (.png, .jpg, .webp)

        Картинка сразу отправляется в VLM, возвращается распознанный текст с пометкой Страница 1.

    Номера страниц:

        В конце каждой страницы (и после обычного текста, и после ответа VLM, и после распознанного фото) обязательно дописывается блок:
        \n\n---\n📄 **Страница X из Y**\n

3. Структура проекта (разбивка на модули)

Чтобы код был чистым и удобным для поддержки, разделим его на 5 логических файлов внутри src/:
Plaintext

pdf-vlm-loader/
├── Cargo.toml
├── .env
└── src/
    ├── main.rs        # Точка входа, HTTP-роутер Axum, проверка авторизации
    ├── config.rs      # Настройки из .env, состояние сервера, регулярки
    ├── processor.rs   # Главная логика обхода страниц (3 сценария + номера страниц)
    ├── pdf_images.rs  # Вытаскивание фото через lopdf и рендер страницы в PNG
    └── vlm.rs         # Запросы к VLM по IP-адресу + кэш blake3

4. Файл .env (на VM с OpenWebUI, с указанием IP-адреса VM с видеокартами)

Положите этот файл .env рядом со скомпилированным бинарником на виртуальной машине с OpenWebUI (замените 192.168.1.50:8000 на реальный IP и порт вашей VM с видеокартами):
Фрагмент кода

# Слушаем на всех интерфейсах VM с OpenWebUI (чтобы можно было тестить и локально, и с Windows-ПК)
LISTEN_ADDR=0.0.0.0:5000

# Ключ для OpenWebUI (Settings -> Admin -> Documents -> External Document Loader API Key)
LOADER_API_KEY=my-secret-loader-key-2026

# IP-адрес вашей отдельной VM с видеокартами, где крутится VLM (llama.cpp / vLLM)
VLM_API_URL=http://192.168.1.50:8000/v1/chat/completions
VLM_API_KEY=EMPTY
VLM_MODEL=qwen-3.8-27b

# Минимальный размер фото в пикселях
MIN_IMAGE_DIM=80

# Если на странице меньше 25 непробельных символов — считаем её фото-сканом
MIN_TEXT_CHARS_PER_PAGE=25

# Число параллельных запросов к GPU VM
MAX_CONCURRENT_VLM=4

# false = возвращать единый текст со всеми страницами и метками [Страница X из Y]
RETURN_PAGE_ARRAY=false

5. Полный набор команд для проверки (из Linux и из Windows)

Предположим:

    IP вашей VM с OpenWebUI (где запущен этот Rust-сервер): 192.168.1.10 (или 127.0.0.1, если тестируете прямо внутри неё).

    IP вашей VM с видеокартами (где запущена VLM): 192.168.1.50:8000.

Блок А: Проверки внутри самой Linux VM (с OpenWebUI)

1. Проверяем, видит ли VM с OpenWebUI вашу VM с видеокартами по IP:
Bash

curl -s http://192.168.1.50:8000/v1/models

(Должен вернуться JSON со списком запущенной модели).

2. Проверяем /health нашего Rust-сервера:
Bash

curl -s http://127.0.0.1:5000/health

3. Отправляем PDF-документ и сохраняем распознанный текст сразу в читаемый файл output.md (а не просто сырой JSON с \n):
Bash

curl -s -X PUT http://127.0.0.1:5000/process \
  -H "Authorization: Bearer my-secret-loader-key-2026" \
  -H "Content-Type: application/pdf" \
  -H "X-Filename: doc_with_figures.pdf" \
  --data-binary @test.pdf | python3 -c "import sys, json; data=json.load(sys.stdin); print(data.get('page_content', data))" > output.md

cat output.md

(Однострочник на python3 красиво распакует поле page_content с русскими буквами и переносами строк в файл output.md, где вы сразу увидите и вставленные описания рисунков, и метки [Страница 1 из N] в конце каждой страницы).

4. Отправляем обычное фото / скриншот (.png или .jpg) напрямую:
Bash

curl -s -X PUT http://127.0.0.1:5000/process \
  -H "Authorization: Bearer my-secret-loader-key-2026" \
  -H "Content-Type: image/png" \
  -H "X-Filename: photo_page.png" \
  --data-binary @photo.png | python3 -c "import sys, json; print(json.load(sys.stdin)['page_content'])"

Блок Б: Проверки с вашего ПК на Windows (по сети до VM 192.168.1.10:5000)

Откройте PowerShell на Windows. У вас есть два удобных способа тестирования:

Способ 1: Нативный PowerShell (Invoke-RestMethod) — самый удобный на Windows!
Он сам декодирует JSON и UTF-8 кириллицу и сразу выводит чистый распознанный текст в консоль или сохраняет в файл:
PowerShell

# 1. Проверка статуса сервера (/health) с Windows:
Invoke-RestMethod -Uri "http://192.168.1.10:5000/health" -Method Get

# 2. Отправка PDF-файла с Windows и вывод чистого распознанного текста в консоль:
$headers = @{
    "Authorization" = "Bearer my-secret-loader-key-2026"
    "X-Filename"    = "My_Report.pdf"
}
$response = Invoke-RestMethod -Uri "http://192.168.1.10:5000/process" `
    -Method Put `
    -Headers $headers `
    -ContentType "application/pdf" `
    -InFile "C:\test\sample.pdf"

# Выводим распознанный текст со всеми страницами на экран:
Write-Output $response.page_content

# И сохраняем результат в готовый Markdown-файл на рабочем столе Windows:
$response.page_content | Set-Content -Path "C:\test\result.md" -Encoding UTF8

3. Отправка фото (.jpg / .png) из PowerShell:
PowerShell

$headers = @{
    "Authorization" = "Bearer my-secret-loader-key-2026"
    "X-Filename"    = "scan_photo.jpg"
}
$respPhoto = Invoke-RestMethod -Uri "http://192.168.1.10:5000/process" `
    -Method Put `
    -Headers $headers `
    -ContentType "image/jpeg" `
    -InFile "C:\test\scan_photo.jpg"

Write-Output $respPhoto.page_content

Способ 2: Через curl.exe в Windows (обратите внимание: в PowerShell нужно писать именно curl.exe, иначе вызовется алиас PowerShell):
PowerShell

curl.exe -X PUT "http://192.168.1.10:5000/process" `
  -H "Authorization: Bearer my-secret-loader-key-2026" `
  -H "Content-Type: application/pdf" `
  -H "X-Filename: win_test.pdf" `
  --data-binary "@C:\test\sample.pdf" `
  -o "C:\test\raw_response.json"
