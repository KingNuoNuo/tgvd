pub mod telegram;

pub fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let x = n as f64;
    if x < KB {
        format!("{n} B")
    } else if x < KB * KB {
        format!("{:.1} KB", x / KB)
    } else if x < KB * KB * KB {
        format!("{:.1} MB", x / (KB * KB))
    } else {
        format!("{:.2} GB", x / (KB * KB * KB))
    }
}
