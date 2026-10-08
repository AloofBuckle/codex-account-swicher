use std::sync::OnceLock;

static UI_IS_CHINESE: OnceLock<bool> = OnceLock::new();

pub fn ui_is_chinese() -> bool {
    *UI_IS_CHINESE.get_or_init(|| {
        sys_locale::get_locale()
            .as_deref()
            .is_some_and(locale_is_chinese)
    })
}

fn locale_is_chinese(locale: &str) -> bool {
    let locale = locale.trim().to_ascii_lowercase();
    locale == "zh"
        || locale.starts_with("zh-")
        || locale.starts_with("zh_")
        || locale.starts_with("zh.")
        || locale.starts_with("zh@")
}

#[cfg(test)]
mod tests {
    use super::locale_is_chinese;

    #[test]
    fn detects_chinese_locale_family() {
        for locale in ["zh", "zh-CN", "zh_CN.UTF-8", "zh-Hans", "zh-Hant-TW"] {
            assert!(locale_is_chinese(locale), "{locale}");
        }
        for locale in ["en-US", "ja-JP", "C", "POSIX"] {
            assert!(!locale_is_chinese(locale), "{locale}");
        }
    }
}
