/// Now, in UTC with millisecond precision (`2026-11-04T10:00:00.000Z`), as session and plan files record it.
pub fn iso_ms() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}
