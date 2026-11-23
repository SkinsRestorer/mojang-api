use uuid::Uuid;

#[must_use]
pub fn is_valid_minecraft_username(username: &str) -> bool {
    // Mojang's bulk endpoints reject a whole batch when one name exceeds 25 characters, so this
    // limit also protects the other names in a batch.
    !username.is_empty()
        && username.len() <= 16
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Parses a UUID in the compact (32 hex digits) or dashed (8-4-4-4-12) form.
#[must_use]
pub fn parse_minecraft_uuid(value: &str) -> Option<Uuid> {
    // `Uuid::try_parse` also accepts the braced and URN forms, which have other lengths.
    matches!(value.len(), 32 | 36)
        .then(|| Uuid::try_parse(value).ok())
        .flatten()
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
        assert!(is_valid_minecraft_username("sixteen_chars_xx"));
        assert!(!is_valid_minecraft_username("Pistonmästr"));
    }

    #[test]
    fn parses_only_dashed_and_compact_uuids() {
        let dashed = "b1ae0778-4817-436c-96a3-a72c67cda060";
        let compact = "b1ae07784817436c96a3a72c67cda060";

        assert_eq!(parse_minecraft_uuid(dashed), parse_minecraft_uuid(compact));
        assert!(parse_minecraft_uuid("{b1ae0778-4817-436c-96a3-a72c67cda060}").is_none());
        assert!(parse_minecraft_uuid("not-a-uuid").is_none());
        assert_eq!(
            parse_minecraft_uuid(&dashed.to_ascii_uppercase()),
            parse_minecraft_uuid(dashed)
        );
        assert_eq!(
            parse_minecraft_uuid("00000000-0000-0000-0000-000000000000"),
            Some(uuid::Uuid::nil())
        );
        assert!(parse_minecraft_uuid("urn:uuid:b1ae0778-4817-436c-96a3-a72c67cda060").is_none());
        assert!(parse_minecraft_uuid("b1ae07784-817-436c-96a3-a72c67cda060").is_none());
        assert!(parse_minecraft_uuid("b1ae0778-4817-436c-96a3-a72c67cda06g").is_none());
        assert!(parse_minecraft_uuid("+1ae07784817436c96a3a72c67cda060").is_none());
    }
}
