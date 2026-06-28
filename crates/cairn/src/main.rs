//! Cairn — a modern terminal file manager for every filesystem.
//!
//! This is the application entry point. Cairn is currently in its design phase; see
//! `docs/PRD.md` for the product requirements and `docs/` for design documentation.

/// Application name, surfaced in the UI and `--version` output.
pub const APP_NAME: &str = "cairn";

fn main() {
    println!(
        "{APP_NAME} {} — pre-alpha (design phase). See docs/PRD.md.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_is_set() {
        assert_eq!(APP_NAME, "cairn");
    }
}
