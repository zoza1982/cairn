//! Minimal command-line parsing.
//!
//! Kept dependency-free while the surface is tiny; a richer parser (e.g. `clap`) will be introduced
//! when real subcommands and flags arrive.

/// The parsed result of the command line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// Launch the application normally.
    Run,
    /// Print the version and exit.
    Version,
    /// Print help and exit.
    Help,
    /// Render one UI scenario to stdout and exit (no TTY needed) — a headless way to inspect a
    /// screen. `size` is the terminal grid to render at.
    FrameDump { scenario: String, size: (u16, u16) },
    /// List the available `--frame-dump` scenarios and exit.
    FrameDumpList,
    /// An unrecognized argument was supplied.
    Unknown(String),
}

/// Default frame-dump terminal size (columns × rows).
const DEFAULT_FRAME_SIZE: (u16, u16) = (80, 24);

/// Parse an iterator of arguments (excluding the program name) into an [`Invocation`].
pub(crate) fn parse<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut args = args.into_iter();
    match args.next() {
        None => Invocation::Run,
        Some(arg) => match arg.as_str() {
            "-V" | "--version" => Invocation::Version,
            "-h" | "--help" => Invocation::Help,
            "--frame-dump-list" => Invocation::FrameDumpList,
            // `--frame-dump <scenario> [WxH]`: render one screen headlessly. A missing scenario, or
            // a malformed size, is surfaced as `Unknown` so the caller prints usage rather than
            // silently guessing.
            "--frame-dump" => match args.next() {
                None => Invocation::Unknown("--frame-dump (missing scenario)".to_owned()),
                Some(scenario) => {
                    let size = match args.next() {
                        None => DEFAULT_FRAME_SIZE,
                        Some(dims) => match parse_size(&dims) {
                            Some(size) => size,
                            None => {
                                return Invocation::Unknown(format!(
                                    "--frame-dump size '{dims}' (expected e.g. 80x24)"
                                ))
                            }
                        },
                    };
                    Invocation::FrameDump { scenario, size }
                }
            },
            other => Invocation::Unknown(other.to_owned()),
        },
    }
}

/// Parse a `WxH` size like `80x24` into `(cols, rows)`, clamped to a sane, non-zero range so a
/// downstream render can never be handed a `0`- or absurdly-large grid.
fn parse_size(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let w: u16 = w.trim().parse().ok()?;
    let h: u16 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.min(1000), h.min(1000)))
}

/// The `--help` text.
#[must_use]
pub(crate) fn help_text() -> String {
    format!(
        "{name} {version}\nA modern terminal file manager for every filesystem.\n\n\
         USAGE:\n    {name} [OPTIONS]\n\n\
         OPTIONS:\n    -h, --help       Print this help\n    -V, --version    Print version\n\n\
         DEBUG:\n    \
         --frame-dump <scenario> [WxH]   Render one UI screen to stdout (no TTY); default 80x24\n    \
         --frame-dump-list               List the available --frame-dump scenarios\n",
        name = crate::APP_NAME,
        version = crate::APP_VERSION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Invocation {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_args_runs() {
        assert_eq!(parse_str(&[]), Invocation::Run);
    }

    #[test]
    fn version_flags() {
        assert_eq!(parse_str(&["-V"]), Invocation::Version);
        assert_eq!(parse_str(&["--version"]), Invocation::Version);
    }

    #[test]
    fn help_flags() {
        assert_eq!(parse_str(&["-h"]), Invocation::Help);
        assert_eq!(parse_str(&["--help"]), Invocation::Help);
    }

    #[test]
    fn unknown_arg() {
        assert_eq!(
            parse_str(&["--nope"]),
            Invocation::Unknown("--nope".to_owned())
        );
    }

    #[test]
    fn frame_dump_list() {
        assert_eq!(parse_str(&["--frame-dump-list"]), Invocation::FrameDumpList);
    }

    #[test]
    fn frame_dump_default_size() {
        assert_eq!(
            parse_str(&["--frame-dump", "dual-pane"]),
            Invocation::FrameDump {
                scenario: "dual-pane".to_owned(),
                size: (80, 24),
            }
        );
    }

    #[test]
    fn frame_dump_explicit_size() {
        assert_eq!(
            parse_str(&["--frame-dump", "pager-text", "40x12"]),
            Invocation::FrameDump {
                scenario: "pager-text".to_owned(),
                size: (40, 12),
            }
        );
    }

    #[test]
    fn frame_dump_missing_scenario_is_unknown() {
        assert!(matches!(
            parse_str(&["--frame-dump"]),
            Invocation::Unknown(_)
        ));
    }

    #[test]
    fn frame_dump_bad_size_is_unknown() {
        assert!(matches!(
            parse_str(&["--frame-dump", "dual-pane", "wide"]),
            Invocation::Unknown(_)
        ));
    }

    #[test]
    fn parse_size_accepts_and_clamps() {
        assert_eq!(parse_size("80x24"), Some((80, 24)));
        assert_eq!(parse_size("120X40"), Some((120, 40)));
        assert_eq!(parse_size("5000x5000"), Some((1000, 1000))); // clamped
        assert_eq!(parse_size("0x24"), None); // zero rejected
        assert_eq!(parse_size("80"), None); // no separator
        assert_eq!(parse_size("axb"), None); // non-numeric
        assert_eq!(parse_size("99999x24"), None); // exceeds u16 → rejected, not overflow
        assert_eq!(parse_size("80x24x30"), None); // extra separator rejected
        assert_eq!(parse_size("120X40"), Some((120, 40))); // uppercase X
    }

    #[test]
    fn help_mentions_frame_dump() {
        assert!(help_text().contains("--frame-dump"));
    }

    #[test]
    fn help_text_mentions_name() {
        assert!(help_text().contains(crate::APP_NAME));
    }
}
