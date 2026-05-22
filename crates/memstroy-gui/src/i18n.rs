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
        "Render full clip..." => "Рендер всего ролика…",
        "\u{1F3A5} Render full clip..." => "\u{1F3A5} Рендер всего ролика…",

        // Tools menu
        "Tools" => "Инструменты",
        "\u{1F9E0} Tools" => "\u{1F9E0} Инструменты",
        "Skeleton Constructor..." => "Конструктор скелета…",
        "\u{1F9B4} Skeleton Constructor..." => "\u{1F9B4} Конструктор скелета…",

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
        "Scale" => "Масштаб",
        "Stretch Y" => "Растяжение Y",
        "Rotation" => "Поворот",
        "Opacity" => "Прозрачность",
        "Flip X" => "Отражение X",
        "Flip Y" => "Отражение Y",
        "Visible" => "Видимо",
        "Source" => "Источник",
        "Color correction" => "Цветокоррекция",
        "Brightness" => "Яркость",
        "Contrast" => "Контраст",
        "Saturation" => "Насыщенность",
        "Temperature" => "Температура",
        "Chroma key" => "Хромакей",
        "Similarity" => "Допуск",
        "Blend" => "Смягчение",
        "Spill" => "Подавление цвета",
        "Toggle animation for this parameter" =>
            "Переключить анимацию параметра",
        "+ kf at playhead" => "+ кадр на позиции",
        "Add a keyframe at the current playhead" =>
            "Добавить ключевой кадр на текущей позиции",
        "Clear kfs" => "Очистить кадры",

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
        "Stop" => "Стоп",
        "Loop" => "Зацикливание",
        "\u{25B6} Playing" => "\u{25B6} Воспроизведение",
        "\u{23F8} Paused" => "\u{23F8} Пауза",
        "Snap" => "Привязка",
        "Razor" => "Лезвие",
        "Split at playhead" => "Разрезать в точке",
        "Merge with next" => "Объединить со следующим",

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
