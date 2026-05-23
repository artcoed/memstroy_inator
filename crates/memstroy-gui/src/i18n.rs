//! Internationalisation (English / Russian) for the editor UI.
//!
//! ## Design
//!
//! The translation table uses **English source strings as keys**, so
//! existing code can be migrated incrementally — wrap a label in
//! `t("Open scene...")` and it Just Works. Strings without a Russian
//! entry fall back to English so partial coverage stays visible
//! (instead of showing `???` placeholders).
//!
//! Two access patterns are supported:
//!
//! - `t(key)` reads the global atomic language flag. Use this from
//!   any UI code that doesn't already hold an `EditorState`. The flag
//!   is updated by the settings dialog whenever the user picks a
//!   different language.
//! - `Lang::lookup(key)` is the lower-level entry point used by the
//!   global `t()` and by tests.
//!
//! Keep the table sorted **by category** (menu, dialogs, inspector,
//! timeline, ...) — every new UI string just needs an entry in the
//! corresponding section.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

/// Supported languages for the editor UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Lang {
    #[default]
    En,
    Ru,
}

impl Lang {
    /// Human-friendly name shown in the language picker.
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }

    /// Ordered list of every supported language. Stable so the settings
    /// dialog can iterate without code duplication.
    pub fn all() -> &'static [Lang] {
        &[Lang::En, Lang::Ru]
    }

    /// Translate a key into this language. Returns the English source
    /// string when no Russian translation is registered.
    pub fn lookup(self, key: &'static str) -> &'static str {
        match self {
            Lang::En => key,
            Lang::Ru => ru_lookup(key).unwrap_or(key),
        }
    }
}

// ─── Global flag ─────────────────────────────────────────────────────

static GLOBAL_LANG: AtomicU8 = AtomicU8::new(0);

/// Update the globally-active language. Call this once at startup
/// (after loading settings) and whenever the user changes the language
/// in the settings dialog.
pub fn set_global(lang: Lang) {
    GLOBAL_LANG.store(lang as u8, Ordering::Relaxed);
}

/// Read the globally-active language.
pub fn current() -> Lang {
    match GLOBAL_LANG.load(Ordering::Relaxed) {
        1 => Lang::Ru,
        _ => Lang::En,
    }
}

/// Translate a key using the globally-active language. The most
/// ergonomic entry point for UI code:
///
/// ```ignore
/// ui.label(t("Inspector"));
/// ```
pub fn t(key: &'static str) -> &'static str {
    current().lookup(key)
}

// ─── Russian translation table ───────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn ru_lookup(key: &str) -> Option<&'static str> {
    Some(match key {
        // ── Menu bar / File ─────────────────────────────────────
        "File" => "Файл",
        "\u{1F4C1} File" => "\u{1F4C1} Файл",
        "New scene" => "Новая сцена",
        "\u{2728} New scene" => "\u{2728} Новая сцена",
        "Open scene..." => "Открыть сцену…",
        "\u{1F4C2} Open scene..." => "\u{1F4C2} Открыть сцену…",
        "Save scene" => "Сохранить сцену",
        "\u{1F4BE} Save scene" => "\u{1F4BE} Сохранить сцену",
        "Save scene as..." => "Сохранить сцену как…",
        "\u{1F4BE} Save scene as..." => "\u{1F4BE} Сохранить сцену как…",
        "Settings" => "Настройки",
        "Settings..." => "Настройки…",
        "\u{2699} Settings..." => "\u{2699} Настройки…",
        "Exit" => "Выход",
        "\u{1F6AA} Exit" => "\u{1F6AA} Выход",

        // Render menu
        "Render" => "Рендер",
        "\u{1F3AC} Render" => "\u{1F3AC} Рендер",
        "Render full clip..." => "Рендер ролика…",
        "\u{1F3A5} Render full clip..." => "\u{1F3A5} Рендер ролика…",

        // Tools menu was removed; the entries that used to live there
        // (Skeleton Constructor, Web Image Search) are now reachable
        // exclusively via the View menu. Strings kept below in case
        // a stray translation lookup hits them.
        "Skeleton Constructor..." => "Скелет…",
        "\u{1F9B4} Skeleton Constructor..." => "\u{1F9B4} Скелет…",
        "\u{2692} Skeleton Constructor..." => "\u{2692} Скелет…",
        "Web Image Search..." => "Поиск картинок…",
        "\u{1F310} Web Image Search..." => "\u{1F310} Поиск картинок…",

        // ── View menu ───────────────────────────────────────────
        "View" => "Вид",
        "\u{1F441} View" => "\u{1F441} Вид",
        "Web Image Search" => "Поиск картинок",
        "\u{1F310} Web Image Search" => "\u{1F310} Поиск картинок",
        "Search images..." => "Поиск картинок…",
        "Found" => "Найдено",
        "Type a query and press Enter. Click a result to drop it on the canvas at the playhead, or drag it onto the canvas / timeline." =>
            "Введите запрос и нажмите Enter. Кликните по результату, чтобы добавить картинку на холст в позиции плейхеда, или перетащите её на холст / таймлайн.",
        "click an image to add it to the project, or drag it onto the canvas." =>
            "кликните по картинке, чтобы добавить её в проект, или перетащите на холст.",
        "No results yet." => "Пока ничего не найдено.",
        // Curve editor strings
        "+ Key" => "+ Кейфрейм",
        "Add keyframe at playhead" => "Добавить кейфрейм в позиции плейхеда",
        "Toggle whether this parameter is animatable (changes will create keyframes)" =>
            "Переключить — параметр станет анимируемым (изменения создадут кейфреймы)",
        "Animated" => "Анимирован",
        "Static" => "Статичен",
        "Element" => "Элемент",
        "Select an actor, overlay or audio layer to edit its curves." =>
            "Выберите актёра, оверлей или аудио-слой, чтобы редактировать его кривые.",
        "Transition into kf:" => "Переход в кейфрейм:",
        "Transition into kf" => "Переход в кейфрейм",
        "Interpolation" => "Интерполяция",
        "Linear" => "Линейный",
        "Ease in" => "Замедление в начале",
        "Ease out" => "Замедление в конце",
        "Ease in/out" => "Замедление в начале и конце",
        "Step (hold)" => "Скачок (удержание)",
        "Step (instant)" => "Мгновенный",
        "Cubic" => "Кубический",
        "Delete keyframe" => "Удалить кейфрейм",
        "Overlay" => "Слой",
        "Pos X" => "Поз. X",
        "Pos Y" => "Поз. Y",
        // Mask add buttons
        "\u{25AD} Rectangle / Crop" => "\u{25AD} Прямоугольник / Кроп",
        // Per-param row context menu
        "Curve Editor" => "Редактор кривых",
        "\u{1F4C8} Curve Editor" => "\u{1F4C8} Редактор кривых",
        "Image Editor" => "Редактор изображений",
        "\u{1F5BC} Image Editor" => "\u{1F5BC} Редактор изображений",
        "Untitled clip" => "Без названия",
        "Masks" => "Маски",
        "Masks hide / reveal parts of the layer. Pick a shape, then drag on the canvas to paint it." =>
            "Маски скрывают / показывают части слоя. Выберите форму и протяните по холсту, чтобы её нарисовать.",
        "No masks yet." => "Масок пока нет.",
        "Add mask" => "Добавить маску",
        "Rectangle mask" => "Прямоугольная маска",
        "Ellipse mask" => "Эллипс-маска",
        "Freehand mask" => "Произвольная маска",
        "Crop" => "Кадрирование",
        "Reset crop" => "Сбросить кадр",
        "Repaint" => "Перерисовать",
        "Drawing" => "Рисую",
        "Feather" => "Размытие краёв",
        "Invert (hide inside)" => "Инвертировать (скрывать внутри)",
        "\u{25AD} Rectangle" => "\u{25AD} Прямоугольник",
        "\u{2B2D} Ellipse" => "\u{2B2D} Эллипс",
        "\u{270D} Freehand" => "\u{270D} От руки",
        "\u{2702} Crop" => "\u{2702} Кадр",
        "Bottom" => "Снизу",
        "Hue \u{00B0}" => "Оттенок \u{00B0}",
        "Quick colour" => "Быстрая цветокоррекция",
        "Vignette" => "Виньетка",
        "Blur" => "Размытие",
        "Sharpen" => "Резкость",
        "Glow" => "Свечение",
        "Noise" => "Шум",
        "Clear all effects" => "Очистить все эффекты",
        "Source:" => "Источник:",
        "Select an image overlay to edit it." => "Выберите слой-картинку, чтобы редактировать.",
        "This panel exposes image-only editing tools (crop, quick colour, filters)." =>
            "Эта панель содержит инструменты только для изображений (кадр, цвет, фильтры).",
        "Preview will appear after canvas renders the image once." =>
            "Превью появится, как только холст хоть раз отрисует это изображение.",
        "Skeleton Editor" => "Редактор скелета",
        "\u{2692} Skeleton Editor" => "\u{2692} Редактор скелета",

        // ── Top-bar status ──────────────────────────────────────
        "refreshing..." => "обновление…",
        "Ready" => "Готово",

        // ── Tabs ────────────────────────────────────────────────
        "New scene tab" => "Новая вкладка",
        "Close tab" => "Закрыть вкладку",
        "Reset to a fresh untitled scene" => "Сбросить до пустой сцены",
        "Untitled" => "Без названия",

        // ── Settings dialog ─────────────────────────────────────
        "Editor settings" => "Настройки редактора",
        "Language" => "Язык",
        "Master volume" => "Общая громкость",
        "Auto-save interval" => "Интервал автосохранения",
        "Snap on timeline" => "Привязка на таймлайне",
        "Apply" => "Применить",
        "Cancel" => "Отмена",
        "Close" => "Закрыть",
        "Done" => "Готово",
        "Reset to defaults" => "Сбросить по умолчанию",
        "Restart not required — changes apply immediately." =>
            "Перезапуск не требуется — изменения применяются сразу.",

        // ── Recovery dialog ─────────────────────────────────────
        "Recover scene?" => "Восстановить сцену?",
        "\u{26A0} A recovered scene was found." =>
            "\u{26A0} Найдена резервная копия сцены.",
        "Restore the auto-saved scene?" =>
            "Восстановить автосохранённую сцену?",
        "Yes, restore" => "Да, восстановить",
        "No, discard" => "Нет, отбросить",
        "Later" => "Позже",

        // ── Title picker ────────────────────────────────────────
        "Add Title" => "Добавить заголовок",
        "Pick a title template" => "Выберите шаблон заголовка",
        "Adds a 3-second text overlay at the playhead. \
                        Edit text/style afterwards in the Inspector." =>
            "Добавляет 3-секундный текстовый слой на позиции воспроизведения. \
             Текст и стиль редактируются в Инспекторе.",

        // ── Inspector (general) ─────────────────────────────────
        "Inspector" => "Инспектор",
        "Select a clip on the timeline" => "Выберите клип на таймлайне",
        "Transform" => "Трансформация",
        "Timing" => "Тайминг",
        "Effects" => "Эффекты",
        "Position" => "Позиция",
        "Position X" => "Позиция X",
        "Position Y" => "Позиция Y",
        "Position & Scale" => "Позиция и масштаб",
        "Scale" => "Масштаб",
        "Scale X:" => "Масштаб X:",
        "Scale Y:" => "Масштаб Y:",
        "Scale (multiplier)" => "Масштаб (×)",
        "Stretch Y" => "Растяжение Y",
        "Rotation" => "Поворот",
        "Opacity" => "Прозрачность",
        "Opacity:" => "Прозрачность:",
        "Flip X" => "Отражение X",
        "Flip Y" => "Отражение Y",
        "Flip X:" => "Отр. X:",
        "Flip Y:" => "Отр. Y:",
        "Flip X all" => "Отр. X у всех",
        "Flip Y all" => "Отр. Y у всех",
        "Toggle horizontal flip on every selected element" =>
            "Переключить горизонт. отражение для всех выбранных",
        "Visible" => "Видимо",
        "Source" => "Источник",
        "Color correction" => "Цветокоррекция",
        "Color Correction" => "Цветокоррекция",
        "Brightness" => "Яркость",
        "Contrast" => "Контраст",
        "Saturation" => "Насыщенность",
        "Temperature" => "Температура",
        "Chroma key" => "Хромакей",
        "Chroma Key" => "Хромакей",
        "Similarity" => "Допуск",
        "Blend" => "Смягчение",
        "Spill" => "Подавление цвета",
        "Eyedropper" => "Пипетка",
        "Pick color from preview" => "Взять цвет с превью",
        "Click preview to pick color..." =>
            "Кликните по превью, чтобы выбрать цвет…",
        "Key:" => "Цвет:",
        "Toggle animation for this parameter" =>
            "Переключить анимацию параметра",
        "+ kf at playhead" => "+ кадр на позиции",
        "Add a keyframe at the current playhead" =>
            "Добавить ключевой кадр на текущей позиции",
        "Clear kfs" => "Очистить кадры",
        "Static value (no keyframes yet)" =>
            "Статическое значение (нет ключей)",
        "Reset" => "Сброс",
        "Reset all" => "Сбросить всё",
        "Reset to 1.0x" => "Сбросить до 1.0x",
        "Output" => "Вывод",
        "Output resolution" => "Разрешение вывода",
        "Render Frame" => "Кадр рендера",
        "The output region. Move/resize/rotate it like any element." =>
            "Область вывода. Двигайте/масштабируйте/поворачивайте как любой элемент.",
        "Render frame has no keyframes." => "У кадра рендера нет ключей.",
        "Camera editing coming soon." => "Редактирование камеры скоро.",
        "elements selected" => "выбрано элементов",
        "Edits below are applied as deltas to every element." =>
            "Изменения применяются как дельта ко всем выделенным.",

        // ── Color Correction inspector ─────────────────────────
        "Basic" => "Базовые",
        "Wheels" => "Колёса",
        "Curves" => "Кривые",
        "Master" => "Общий",
        "Red" => "Красный",
        "Green" => "Зелёный",
        "Blue" => "Синий",
        "Lift" => "Тени",
        "Gamma" => "Гамма",
        "Gain" => "Света",
        "Click empty area: add point  •  Drag: move  •  Right-click: remove" =>
            "Клик: добавить точку  •  Перетаскивание: двигать  •  ПКМ: удалить",

        // ── Modifiers ──────────────────────────────────────────
        "Animation Modifiers" => "Модификаторы анимации",
        "No modifiers. Add one to perturb the animation \
                 (wobble/shake/pulse/spin)." =>
            "Нет модификаторов. Добавьте один, чтобы оживить анимацию (покач./тряска/пульс/вращ.).",
        "Range" => "Диапазон",
        "Always active" => "Всегда активно",
        "Remove modifier" => "Удалить модификатор",
        "+ Wobble" => "+ Покач.",
        "+ Shake" => "+ Тряска",
        "+ Pulse" => "+ Пульс",
        "+ Spin" => "+ Вращ.",
        "+ Walk" => "+ Шаг",
        "Smooth sinusoidal sway" => "Плавное синусоидальное покачивание",
        "High-frequency jitter" => "Высокочастотная тряска",
        "Periodic scale breathing" => "Периодическое дыхание масштаба",
        "Continuous rotation" => "Непрерывное вращение",
        "Pendulum rotation imitating a walking gait (rocks left/right around upright)" =>
            "Маятниковое покачивание, имитирующее шаги (наклоны влево/вправо)",
        "Freq Hz" => "Частота, Гц",
        "Cadence Hz" => "Шаг, Гц",
        "Amp X (px)" => "Амп. X (px)",
        "Amp Y (px)" => "Амп. Y (px)",
        "Amp Rot \u{00B0}" => "Амп. поворота \u{00B0}",
        "Amp Scale" => "Амп. масштаба",
        "Phase" => "Фаза",
        "Seed" => "Сид",
        "Speed \u{00B0}/s" => "Скорость \u{00B0}/с",
        "Sway \u{00B0}" => "Покач. \u{00B0}",
        "Bob Y (px)" => "Подскок Y (px)",

        // ── Effect stack ───────────────────────────────────────
        "+ Add effect:" => "+ Добавить эффект:",
        "No parameters." => "Без параметров.",
        "Remove effect" => "Удалить эффект",
        "Move up" => "Выше",
        "Move down" => "Ниже",
        "Remove all effects" => "Удалить все эффекты",
        "clear" => "очистить",

        // ── Text overlay inspector ─────────────────────────────
        "Text:" => "Текст:",
        "Font" => "Шрифт",
        "Family:" => "Семейство:",
        "Size:" => "Размер:",
        "Color:" => "Цвет:",
        "Width:" => "Толщина:",
        "Bold" => "Жирный",
        "Italic" => "Курсив",
        "Align:" => "Выравн.:",
        "Left" => "Слева",
        "Center" => "По центру",
        "Right" => "Справа",
        "Stroke" => "Обводка",
        "Stroke text" => "Обводка текста",
        "Background plate" => "Подложка",
        "Enable plate" => "Включить подложку",
        "Type:" => "Тип:",
        "Solid" => "Сплошная",
        "Gradient" => "Градиент",
        "Outline only" => "Только рамка",
        "None (text only)" => "Нет (только текст)",
        "Padding" => "Отступ",
        "Corner radius" => "Скругление",
        "Asymmetric width (px)" => "Асимм. ширина (px)",
        "Extra left" => "Доб. слева",
        "Extra right" => "Доб. справа",
        "Plate border" => "Рамка подложки",
        "Gradient end:" => "Конец градиента:",
        "Decrease (-4)" => "Уменьшить (-4)",
        "Increase (+4)" => "Увеличить (+4)",
        "Synthesised on the bundled font by repainting glyphs \
                     with sub-pixel offsets" =>
            "Синтез жирного на встроенном шрифте через сдвиг подпикселей",
        "Slants glyphs ~13° to the right" => "Наклон глифов ~13° вправо",

        // ── Render frame / output ──────────────────────────────
        "X:" => "X:",
        "Y:" => "Y:",
        "W:" => "Ш:",
        "H:" => "В:",
        "In:" => "Вход:",
        "Out:" => "Выход:",
        "Start:" => "Старт:",
        "Duration:" => "Длит.:",
        "Actor" => "Актёр",
        "Image" => "Картинка",
        "Video" => "Видео",
        "Background" => "Фон",

        // ── Audio inspector ────────────────────────────────────
        "Audio" => "Аудио",
        "Volume" => "Громкость",
        "Speed" => "Скорость",
        "Pitch (semitones)" => "Высота (полутона)",
        "Pan" => "Стерео-баланс",
        "Mute" => "Без звука",
        "Loop source" => "Зациклить источник",
        "Fade in (s)" => "Нарастание (с)",
        "Fade out (s)" => "Затухание (с)",
        "Reverb" => "Реверберация",
        "Low-pass cutoff (Hz)" => "Срез ВЧ (Гц)",
        "High-pass cutoff (Hz)" => "Срез НЧ (Гц)",
        "Enable low-pass filter" => "Включить фильтр НЧ",
        "Enable high-pass filter" => "Включить фильтр ВЧ",
        "Audio effects" => "Аудиоэффекты",
        "Filters" => "Фильтры",
        "Bound to an actor — moves and trims with its parent clip." =>
            "Привязано к актёру — перемещается и обрезается вместе с клипом.",
        "Standalone music — independent of any actor." =>
            "Самостоятельная дорожка — независима от актёров.",
        "Reset audio effects" => "Сбросить аудиоэффекты",
        "Pitch shifts the sound up or down without changing the timeline placement." =>
            "Сдвиг высоты тона вверх или вниз без изменения положения на таймлайне.",
        "Pan: -1 = full left, 0 = centre, +1 = full right." =>
            "Баланс: -1 — полностью слева, 0 — по центру, +1 — справа.",

        // ── Timeline / playback ────────────────────────────────
        "Timeline" => "Таймлайн",
        "Play" => "Воспроизвести",
        "Pause" => "Пауза",
        "Play (Space)" => "Воспроизвести (Space)",
        "Pause (Space)" => "Пауза (Space)",
        "Stop" => "Стоп",
        "Loop" => "Цикл",
        "\u{25B6} Playing" => "\u{25B6} Воспроизведение",
        "\u{23F8} Paused" => "\u{23F8} Пауза",
        "Snap" => "Привязка",
        "Razor" => "Лезвие",
        "Split at playhead" => "Разрезать в точке",
        "Split tool: click anywhere on a clip to cut it at that position" =>
            "Лезвие: кликните по клипу, чтобы разрезать его в этой точке",
        "Add text overlay at playhead" => "Добавить текст на позиции воспроизведения",
        "Loop preview: Shift+click on the ruler to set loop start, Shift+click again for end. \
                Shift+drag = define a region." =>
            "Зацикл. превью: Shift+клик по линейке — старт, ещё раз — конец. \
             Shift+drag — выделить регион.",
        "Merge with next" => "Объединить со следующим",
        "+ V Layer" => "+ V-слой",
        "+ A Layer" => "+ A-слой",
        "Add a new empty video layer at the top of the panel" =>
            "Добавить пустой видео-слой сверху",
        "Add a new empty audio layer below the existing audio block" =>
            "Добавить пустой аудио-слой снизу",
        "\u{2728} New video layer." => "\u{2728} Новый видео-слой.",
        "\u{2728} New audio layer." => "\u{2728} Новый аудио-слой.",
        "Clear multi-selection" => "Сбросить мульти-выделение",

        // ── Library panel ──────────────────────────────────────
        "Library" => "Библиотека",
        "Search" => "Поиск",
        "Search library..." => "Поиск в библиотеке…",
        "Refresh" => "Обновить",
        "Refresh from Telegram" => "Обновить из Telegram",
        "Clips" => "Клипы",
        "Sounds" => "Звуки",
        "Images" => "Картинки",
        "Particles" => "Частицы",
        "Videos" => "Видео",
        "Local" => "Локальная",
        "Global" => "Общая",
        "Drop files here to import" => "Перетащите файлы сюда для импорта",
        "TG channel" => "TG-канал",
        "Limit" => "Лимит",
        "Drag a clip onto the canvas or timeline. The library auto-updates from the assets-server (which periodically ingests from Telegram)." =>
            "Перетащите клип на холст или таймлайн. Библиотека обновляется с сервера ассетов (он периодически тянет из Telegram).",
        "User-imported videos. Drop a video file from your file manager into this panel to add it. Drag a row onto the canvas or timeline to spawn an actor." =>
            "Импортированные видео. Перетащите видеофайл из проводника в эту панель, чтобы добавить. Затем — на холст или таймлайн, чтобы создать актёра.",
        "Drop a sound onto the timeline to add it as an audio track. Drop audio files from your file manager here to import." =>
            "Перетащите звук на таймлайн, чтобы добавить как аудио-дорожку. Аудиофайлы из проводника также импортируются сюда.",
        "Drag a sticker onto the canvas to add it as an image overlay. Drop image files from your file manager here to import." =>
            "Перетащите стикер на холст — добавится как картинка-оверлей. Файлы из проводника тоже импортируются сюда.",
        "Drag a particle onto the canvas — it spawns with spin + pulse modifiers." =>
            "Перетащите частицу на холст — добавится с модификаторами вращения и пульса.",
        "No clips yet — start typing in the search box or scroll to fetch from the server." =>
            "Клипов пока нет — начните печатать в поиске или прокрутите вниз, чтобы подгрузить с сервера.",
        "No clips yet — click Refresh from Telegram above to fetch the latest ones." =>
            "Клипов пока нет — нажмите «Обновить из Telegram» выше, чтобы загрузить последние.",
        "Fetching clips from the server..." =>
            "Загружаем клипы с сервера…",
        "Local (your imports)" => "Локальная (ваши импорты)",
        "Global (built-in / browser)" => "Общая (встроенная / браузер)",
        "server" => "сервер",

        // ── Misc / status messages ─────────────────────────────
        "\u{1F389} Refresh done!" => "\u{1F389} Обновление выполнено!",
        "\u{2705} Saved." => "\u{2705} Сохранено.",
        "\u{2705} Saved (.memstroy)." => "\u{2705} Сохранено (.memstroy).",
        "\u{2705} Scene loaded." => "\u{2705} Сцена загружена.",
        "\u{2728} New scene created." => "\u{2728} Новая сцена создана.",
        "\u{1F4BE} Auto-saved" => "\u{1F4BE} Автосохранено",

        // Actor / overlay / audio actions
        "\u{1F5D1} Actor deleted." => "\u{1F5D1} Актёр удалён.",
        "\u{1F5D1} Overlay deleted." => "\u{1F5D1} Слой удалён.",
        "\u{1F5D1} Background deleted." => "\u{1F5D1} Фон удалён.",
        "\u{1F5D1} Audio deleted." => "\u{1F5D1} Аудио удалено.",
        "\u{1F4CB} Actor duplicated." => "\u{1F4CB} Актёр продублирован.",
        "\u{1F4CB} Overlay duplicated." => "\u{1F4CB} Слой продублирован.",
        "\u{1F4CB} Background duplicated." => "\u{1F4CB} Фон продублирован.",
        "\u{2702} Actor split at playhead." => "\u{2702} Актёр разрезан.",
        "\u{2702} Overlay split at playhead." => "\u{2702} Слой разрезан.",
        "\u{2702} Background split at playhead." => "\u{2702} Фон разрезан.",
        "\u{1F517} Actors merged." => "\u{1F517} Актёры объединены.",
        "\u{1F517} Overlays merged." => "\u{1F517} Слои объединены.",
        "\u{1F517} Backgrounds merged." => "\u{1F517} Фоны объединены.",
        "\u{21A9} Undo" => "\u{21A9} Отмена",
        "\u{26A0} Select an element to split." =>
            "\u{26A0} Выберите элемент для разрезания.",
        "\u{26A0} Select an element with a next sibling to merge." =>
            "\u{26A0} Выберите элемент со следующим соседом для объединения.",

        _ => return None,
    })
}
