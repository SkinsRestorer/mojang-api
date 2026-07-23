use uuid::Uuid;

#[must_use]
pub fn is_valid_minecraft_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 16
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[must_use]
pub fn parse_minecraft_uuid(value: &str) -> Option<Uuid> {
    let valid_shape = match value.len() {
        32 => value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        36 => value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };

    valid_shape.then(|| Uuid::parse_str(value).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::{is_valid_minecraft_username, parse_minecraft_uuid};

    #[test]
    fn validates_supported_minecraft_usernames() {
        assert!(is_valid_minecraft_username("Pistonmaster"));
        assert!(is_valid_minecraft_username("old-name"));
        assert!(is_valid_minecraft_username("_"));
        assert!(!is_valid_minecraft_username(""));
        assert!(!is_valid_minecraft_username("seventeen_chars_x"));
        assert!(!is_valid_minecraft_username("invalid name"));
        assert!(!is_valid_minecraft_username("name!"));
    }

    #[test]
    fn parses_only_dashed_and_compact_uuids() {
        let dashed = "b1ae0778-4817-436c-96a3-a72c67cda060";
        let compact = "b1ae07784817436c96a3a72c67cda060";

        assert_eq!(parse_minecraft_uuid(dashed), parse_minecraft_uuid(compact));
        assert!(parse_minecraft_uuid("{b1ae0778-4817-436c-96a3-a72c67cda060}").is_none());
        assert!(parse_minecraft_uuid("not-a-uuid").is_none());
    }
}
