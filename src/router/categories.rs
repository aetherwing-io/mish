/// The six command categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Verbose output -> condensed summary (npm install, cargo build)
    Condense,
    /// Silent commands -> narrated result (cp, mv, mkdir)
    Narrate,
    /// Output verbatim + metadata footer (cat, grep, ls)
    Passthrough,
    /// Machine-readable parse -> formatted view (git status, docker ps)
    Structured,
    /// Transparent passthrough for interactive commands (vim, htop)
    Interactive,
    /// Warn before executing destructive commands (rm -rf, force push)
    Dangerous,
}

impl Default for Category {
    fn default() -> Self {
        Category::Condense
    }
}
