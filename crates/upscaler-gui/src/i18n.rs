//! Swedish and English. Every visible text goes through [`tr`] with both languages side by side.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(en: bool) {
    ENGLISH.store(en, Ordering::Relaxed);
}

pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// The text in the current language.
pub fn tr(sv: &'static str, en: &'static str) -> &'static str {
    if english() {
        en
    } else {
        sv
    }
}

/// `settings.language` if set, else the system language (Swedish if it starts with "sv").
pub fn apply(language: &str) {
    let code = if language.is_empty() {
        system_language()
    } else {
        language.to_string()
    };
    set_english(!code.to_lowercase().starts_with("sv"));
}

fn system_language() -> String {
    #[cfg(windows)]
    {
        // GetUserDefaultLocaleName through PowerShell would cost a process; the environment
        // and the Windows UI culture variable cover the common cases.
        if let Ok(out) = upscaler_core::process::command("powershell")
            .args(["-NoProfile", "-Command", "(Get-UICulture).Name"])
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        {
            return String::from_utf8_lossy(&out.stdout).replace(['(', ')', '"', ' ', '\n'], "");
        }
    }
    "en".into()
}

/// "1:02:03.45" style time.
pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0);
    let h = (s / 3600.0).floor() as u64;
    let m = ((s % 3600.0) / 60.0).floor() as u64;
    let sec = s % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{sec:05.2}")
    } else {
        format!("{m:02}:{sec:05.2}")
    }
}

/// "3 h 12 min", "4 min 10 s", "12 s".
pub fn format_duration(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if s >= 86400 {
        let d = s / 86400;
        return format!("{d} {} {} h", tr("dygn", "d"), (s % 86400) / 3600);
    }
    if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min {sec} s")
    } else {
        format!("{sec} s")
    }
}

pub fn format_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(format_time(83.5), "01:23.50");
        assert_eq!(format_time(3723.0), "1:02:03.00");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }
}
