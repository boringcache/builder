use std::env;
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

fn colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if env::var_os("CLICOLOR_FORCE")
            .or_else(|| env::var_os("FORCE_COLOR"))
            .is_some_and(|value| value != "0")
        {
            return true;
        }
        env::var_os("TERM").is_none_or(|term| term != "dumb") && io::stdout().is_terminal()
    })
}

fn paint_with(text: impl AsRef<str>, code: &str, enabled: bool) -> String {
    let text = text.as_ref();
    if enabled {
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn paint(text: impl AsRef<str>, code: &str) -> String {
    paint_with(text, code, colors_enabled())
}

pub fn prefix() -> String {
    paint("==>", "1;36")
}

pub fn bold(text: impl AsRef<str>) -> String {
    paint(text, "1")
}

pub fn accent(text: impl AsRef<str>) -> String {
    paint(text, "1;36")
}

pub fn success(text: impl AsRef<str>) -> String {
    paint(text, "1;32")
}

pub fn warning(text: impl AsRef<str>) -> String {
    paint(text, "1;33")
}

pub fn dim(text: impl AsRef<str>) -> String {
    paint(text, "2")
}

pub fn section(title: &str) -> String {
    accent(format!("--- {title} ---"))
}

pub fn print_status(message: impl AsRef<str>) {
    println!("{} {}", prefix(), message.as_ref());
}

pub fn print_step(current: usize, total: usize, label: &str, cached: bool) {
    let counter = dim(format!("[{current} / {total}]"));
    if cached {
        println!(
            "{} {} {} {}",
            prefix(),
            counter,
            bold(label),
            success("(cached)")
        );
    } else {
        println!("{} {} {}", prefix(), counter, bold(label));
    }
}

pub fn print_detail(message: impl AsRef<str>) {
    println!("    {}", message.as_ref());
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, paint_with};

    #[test]
    fn leaves_text_plain_when_color_is_disabled() {
        assert_eq!(paint_with("hello", "1;34", false), "hello");
    }

    #[test]
    fn wraps_text_when_color_is_enabled() {
        assert_eq!(
            paint_with("hello", "1;34", true),
            "\u{1b}[1;34mhello\u{1b}[0m"
        );
    }

    #[test]
    fn formats_bytes_human_readably() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
