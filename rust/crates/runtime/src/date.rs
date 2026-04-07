#[must_use]
pub fn current_local_date_string() -> String {
    let now = time::OffsetDateTime::now_utc();
    let now = time::UtcOffset::current_local_offset()
        .ok()
        .map_or(now, |offset| now.to_offset(offset));
    let date = now.date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

#[cfg(test)]
mod tests {
    use super::current_local_date_string;

    #[test]
    fn current_date_string_uses_iso_format() {
        let date = current_local_date_string();
        assert_eq!(date.len(), 10);
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
        assert!(date.chars().enumerate().all(|(index, ch)| match index {
            4 | 7 => ch == '-',
            _ => ch.is_ascii_digit(),
        }));
    }
}
