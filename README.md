# Memstroy-inator

ОСНОВНОЙ ТГК: https://t.me/memstroy_inator

Десктоп-редактор для сборки коротких вертикальных мемов в стиле
Mellstroy (Shorts / Reels / TikTok). Workspace и имена крейтов
(`memstroy-core`, `memstroy-gui` и т. д.) сохранены как
`memstroy_generator` ради совместимости со скриптами и существующими
проектами; в UI и в описаниях Cargo продукт называется
**Memstroy-inator**.

Интерфейс полностью двуязычный — русский и английский. Переключение
языка: **Settings → Language**.

## Что умеет

1. **Работает с общей библиотекой ассетов**: клипы, видео, картинки,
   звуки, частицы и текстовые сниппеты берутся из локального каталога
   или удалённого assets-server.
2. **Размечает якоря** на скачанных клипах через pose-модель — чтобы
   реквизит (кепки, очки, оружие) держался за тело.
3. **Редактирует** мем в GUI: таймлайн, фоновые дорожки,
   chroma-key актёров, прикреплённый реквизит, текст, движения камеры.
4. **Рендерит** результат в MP4 1080×1920 / 60 fps.

## Структура репозитория

```
memstroy_generator/
├── Cargo.toml                       # корень workspace
├── crates/
│   ├── memstroy-core/               # модель сцены и анимации (serde)
│   ├── memstroy-tg/                 # парсер канала и загрузчик клипов
│   ├── memstroy-vision/             # chroma-key (CPU) и pose-якоря
│   ├── memstroy-render/              # сцена → FFmpeg filtergraph → MP4
│   ├── memstroy-cli/                # бинарь `memstroy`
│   ├── memstroy-assets-server/      # HTTP-сервер библиотеки ассетов
│   └── memstroy-gui/                # редактор `memstroy-gui` (egui/eframe)
├── examples/
│   └── scene.yaml                   # стартовая сцена
└── scripts/
    ├── package-client.{sh,ps1}      # сборка клиентского бандла
    ├── make-installer.{sh,ps1}      # сборка установщика
    └── start-server.{sh,ps1}        # запуск standalone-бэкенда
```

## Что нужно для сборки

- **Rust stable** (`rustup install stable`). Канал зафиксирован в
  `rust-toolchain.toml`, обычно достаточно `rustup show`.
- C-тулчейн (`gcc`, `pkg-config`) и `openssl-devel` (или
  эквивалент в вашем дистрибутиве) для зависимостей.
- На Linux — заголовки **ALSA** (`alsa-lib-devel` на Fedora,
  `libasound2-dev` на Debian/Ubuntu): `rodio` линкуется к ALSA.
- **FFmpeg 6+** в `$PATH` или путь в переменной
  `MEMSTROY_FFMPEG`. Используется и рендером, и GUI для извлечения
  превью-кадров.
- **Linux GUI**: X11- или Wayland-сессия и `libxkbcommon`. На сервере
  без дисплея используйте CLI.

## Архитектура: GUI ↔ бэкенд

Редактор (`memstroy-gui`) и headless-рендер (`memstroy-render`)
обращаются к небольшому HTTP-бэкенду (`memstroy-assets-server`),
который владеет библиотекой ассетов на диске (клипы, видео, картинки,
звуки, частицы, текстовые сниппеты).

По умолчанию GUI **сам поднимает** этот бэкенд внутри своего
Tokio-рантайма на `127.0.0.1:8765` и индексирует каталог `./assets/`
относительно директории запуска. В большинстве случаев запускать
сервер вручную не нужно — достаточно открыть `memstroy-gui`.

Отдельный сервер полезен, когда:

- несколько разработчиков делят одну библиотеку ассетов по LAN;
- ферма рендеринга тянет клипы без запуска GUI;
- вы деплоите общую библиотеку на Railway Volume и добавляете ресурсы
  через админский API.

В этих случаях запустите бинарь напрямую (см. раздел *Бэкенд*) и
укажите адрес в **Settings → Server URL**.

## GUI

```bash
cargo run -p memstroy-gui --release
```

Окно делится на пять зон:

- **Верхнее меню** — File / Render / View.
- **Библиотека (слева)** — вкладки `Clips`, `Videos`, `Sounds`,
  `Images`, `Particles`. Вкладка Clips живёт через бэкенд, остальные
  напрямую читают `assets/<kind>/`. Перетаскиванием из библиотеки на
  холст или таймлайн добавляются объекты в сцену.
- **Превью (центр)** — холст 9:16 в выходном соотношении: ручки
  трансформации, пипетка chroma-key, стек эффектов.
- **Инспектор (справа)** — параметры выбранного актёра / оверлея /
  фона / аудио-дорожки. Здесь же ключевые кадры, модификаторы
  (wobble / shake / pulse / spin / walk) и панель цветокоррекции
  (lift / gamma / gain + master/R/G/B-кривые).
- **Таймлайн (снизу)** — много-дорожечный, с резаком, привязкой,
  лупом, кнопками создания видео-/аудио-слоёв и клавишами
  ключевых кадров для каждого параметра.

Язык интерфейса переключается в **Settings → Language**.

## CLI

```bash
# 1. Скачать все «Имба»-клипы в ./assets/mellstroy
cargo run -p memstroy-cli --release -- download

# 2. Создать стартовый файл сцены
cargo run -p memstroy-cli --release -- new my_scene.yaml

# 3. Отрендерить в MP4 (пути ассетов резолвятся относительно --assets)
cargo run -p memstroy-cli --release -- render my_scene.yaml \
    -o out.mp4 --assets .

# 4. Снять одно превью-PNG в момент t=2с
cargo run -p memstroy-cli --release -- preview my_scene.yaml \
    -o frame.png -t 2.0
```

`memstroy --help` и `memstroy <команда> --help` показывают все
флаги: фильтр, лимит страниц, конкуррентность, режим
catalog-only, путь до ML-модели маттинга и т. п.

## Бэкенд (assets-server)

Через готовый скрипт-лаунчер:

```bash
# Linux / macOS — по умолчанию: 0.0.0.0:8765, корень = ./assets
scripts/start-server.sh

# Свой адрес и корень ассетов
scripts/start-server.sh --addr 127.0.0.1:9000 --root /var/lib/memstroy/assets

# Windows PowerShell
pwsh scripts/start-server.ps1 -Addr 127.0.0.1:9000 -Root C:\memstroy\assets
```

Или напрямую через `cargo`:

```bash
cargo run -p memstroy-assets-server --release -- \
    --addr 0.0.0.0:8765 \
    --root ./assets
```

При старте сервер создаёт недостающие подкаталоги (`clips/`,
`videos/`, `images/`, `sounds/`, `particles/`, `text/`) и индексирует
уже сохранённые файлы. Он ничего не удаляет при рестарте: для Railway
корень должен указывать на mounted Volume. Если `--root` не передан, сервер
использует `ASSETS_ROOT`, а на Railway проверяет его относительно
`RAILWAY_VOLUME_MOUNT_PATH` и не пишет в путь вне volume.

Пользовательские клиенты читают каталог через:

- `GET /api/assets?kind=clip&q=zapros&limit=100&offset=0` — список с
  пагинацией и fuzzy-поиском по Левенштейну;
- `GET /api/assets/:id/download` — потоковая отдача файла;
- `GET /api/assets/:id/preview` — превью.

Админское десктопное приложение может добавлять ресурсы через
`POST /api/admin/assets` (`multipart/form-data`): `kind`, файл `asset`,
`description`, опционально `id`, `label`, `tags`, `thumbnail`. Для
клипов контракт выглядит как `kind=clip`, видеофайл в `asset` и
описание в `description`. Если переменная `ADMIN_TOKEN` задана, запрос
должен передать `Authorization: Bearer <token>` или `X-Admin-Token`.

Уровень логов по умолчанию — `info`. Для отладки сервера:
`RUST_LOG=memstroy_assets_server=debug`.

## Сборка клиентского релиза

Клиентские бандлы предназначены для распространения. Они подключаются
к удалённому `memstroy-assets-server` оператора. Особенности:

- Собираются с защищённым `[profile.release]` (без символов и
  debug-info, `panic = abort`, без incremental).
- URL бэкенда **зашивается в бинарь** на этапе компиляции и
  оборачивается через `obfstr`, чтобы его не было видно в
  `strings(1)`.
- **Не содержат каталог `assets/`** — каждый клип / картинка / звук
  тянется с сервера по требованию и кэшируется в
  `~/.memstroy/cache/` (или `%USERPROFILE%\.memstroy\cache\`).
- **Не содержат `memstroy-assets-server`** — бэкенд должен
  работать отдельно на стороне оператора.

```bash
# Linux / macOS
scripts/package-client.sh --server-url https://assets.your-domain.example
# → dist/Memstroy-inator-<os>-<arch>-<ver>/

# Windows PowerShell
pwsh scripts/package-client.ps1 -ServerUrl https://assets.your-domain.example
# → dist\Memstroy-inator-windows-<arch>-<ver>\
```

Скрипты по умолчанию **отказываются** работать с loopback-адресом
(`127.0.0.1`, `localhost`, `::1`); для staging-сборок используйте
`--allow-loopback` / `-AllowLoopback`.

Полезные флаги:

- `--out <path>` / `-Out <path>` — куда сложить бандл;
- `--name <name>` / `-Name <name>` — имя директории бандла;
- `--zip` / `-Zip` — дополнительно собрать `<bundle-name>.zip`.

В бандл попадают:

1. `bin/memstroy-gui` (плюс `bin/memstroy` CLI на поддерживаемых ОС);
2. `examples/*.yaml` и `README.md`;
3. Лаунчер верхнего уровня (`Memstroy-inator.sh` /
   `Memstroy-inator.bat`), запускающий GUI.

## Сборка установщика «в один файл»

Скрипты выше создают *папку-бандл*. Чтобы получить установщик «двойным
кликом», с ярлыком в меню «Пуск», иконкой на рабочем столе и
зарегистрированным деинсталлятором — используйте `make-installer`.
Скрипт сам соберёт (или возьмёт готовый) бандл и упакует его.

```bash
# Linux: само-распаковывающийся .run, без внешних зависимостей
scripts/make-installer.sh --server-url https://assets.your-domain.example
# → dist/Memstroy-inator-linux-<arch>-<ver>.run

# Windows PowerShell: требуется Inno Setup 6 (https://jrsoftware.org/isinfo.php)
pwsh scripts/make-installer.ps1 -ServerUrl https://assets.your-domain.example
# → dist\Memstroy-inator-windows-<arch>-<ver>-Setup.exe
```

В конце работы оба скрипта печатают абсолютный путь до получившегося
установщика. Этот файл и есть то, что раздавать пользователям.

Если бандл уже готов и нужно только обернуть его без пересборки:

```bash
scripts/make-installer.sh --bundle-dir dist/Memstroy-inator-linux-x86_64-0.1.0
pwsh scripts/make-installer.ps1 -BundleDir .\dist\Memstroy-inator-windows-amd64-0.1.0
```

Куда установщики кладут файлы по умолчанию:

- **Windows.** `%ProgramFiles%\Memstroy-inator\` (системная установка)
  или `%LocalAppData%\Programs\Memstroy-inator\` (для текущего
  пользователя). Группа в меню «Пуск» — **Memstroy-inator** —
  с редактором и деинсталлятором, ярлык на рабочем столе и запись в
  *Параметры → Приложения → Установленные приложения*.
- **Linux.** `/opt/Memstroy-inator/` при запуске под `sudo`, иначе
  `~/.local/share/Memstroy-inator/`. `.desktop`-файл в меню
  приложений, ярлык на `~/Desktop/` (только при пользовательской
  установке), симлинк `memstroy-gui` в `PATH` и скрипт
  `uninstall.sh` в каталоге установки.

## Формат сцены (фрагмент)

Сцена — YAML или JSON с `format_version: 1`. Полный пример —
`examples/scene.yaml`. Все анимируемые величины описываются одинаково:

```yaml
layout:
  - t: 0.0
    value: { pos: [0.5, 0.7], scale: 1.0 }
    easing: linear
  - t: 1.5
    value: { pos: [0.3, 0.5], scale: 1.2 }
    easing: ease_out
```

`pos` хранится в нормализованных координатах сцены (`[0, 1]`),
поэтому одна и та же сцена корректно рендерится в любом разрешении.

## Лицензия

MIT. Каноническая декларация — в `Cargo.toml` каждого крейта.
