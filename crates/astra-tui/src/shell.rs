//! Literal argument quoting for generated shell commands.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_preserves_quotes_and_shell_metacharacters() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b;$()\\c\n"), "'a'\"'\"'b;$()\\c\n'");
    }

    #[test]
    fn fish_escapes_backslashes_and_single_quotes_independently() {
        assert_eq!(fish_quote(""), "''");
        assert_eq!(fish_quote("a'b;$()\\c\n"), "'a\\'b;$()\\\\c\n'");
        assert_eq!(fish_quote("\\'\\"), "'\\\\\\'\\\\'");
        assert_eq!(fish_quote("配置 目录"), "'配置 目录'");
    }
}
