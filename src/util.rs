/// Shared utility functions.

/// Expand `~` prefix to `$HOME` in a path string.
pub fn expand_tilde(path: &str) -> String {
    expand_tilde_with_home(path, std::env::var("HOME").ok().as_deref())
}

/// Expand `~` prefix using the given home directory value.
fn expand_tilde_with_home(path: &str, home: Option<&str>) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = home {
            format!("{home}{rest}")
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn expand_tilde_with_custom_home() {
        assert_eq!(expand_tilde_with_home("~/config", Some("/home/user")), "/home/user/config");
        assert_eq!(expand_tilde_with_home("~/.config/mish", Some("/home/user")), "/home/user/.config/mish");
        assert_eq!(expand_tilde_with_home("~", Some("/home/user")), "/home/user");
        assert_eq!(expand_tilde_with_home("~/foo", None), "~/foo"); // no HOME
    }
}
