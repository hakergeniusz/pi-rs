//! Script-visible identifier normalization.
//!
//! Ground truth: `packages/codemode/src/identifier.ts:1-12`. Ports
//! `toCodemodeIdentifier` exactly: characters that are not valid in a
//! JavaScript identifier become `_`, and an empty result becomes `_`.

/// Returns the identifier a script uses for a tool.
///
/// `mcp__docs__search` is unchanged; `my-tool` becomes `my_tool`. The first
/// character follows the stricter `IdentifierStart` rule (`A-Z`, `a-z`, `_`,
/// `$`); subsequent characters also allow digits.
#[must_use]
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character.is_ascii_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        };
        identifier.push(if valid { character } else { '_' });
    }
    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_valid_identifiers_unchanged() {
        assert_eq!(
            to_codemode_identifier("mcp__docs__search"),
            "mcp__docs__search"
        );
        assert_eq!(to_codemode_identifier("$read_2"), "$read_2");
    }

    #[test]
    fn replaces_invalid_characters_and_leading_digits() {
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("2fa.name"), "_fa_name");
        assert_eq!(to_codemode_identifier(""), "_");
        assert_eq!(to_codemode_identifier("**"), "__");
    }

    #[test]
    fn works_on_unicode_names() {
        assert_eq!(to_codemode_identifier(" Größer "), "_Gr__er_");
    }
}
