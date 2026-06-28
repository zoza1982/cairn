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
    /// An unrecognized argument was supplied.
    Unknown(String),
}

/// Parse an iterator of arguments (excluding the program name) into an [`Invocation`].
pub(crate) fn parse<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    match args.into_iter().next() {
        None => Invocation::Run,
        Some(arg) => match arg.as_str() {
            "-V" | "--version" => Invocation::Version,
            "-h" | "--help" => Invocation::Help,
            other => Invocation::Unknown(other.to_owned()),
        },
    }
}

/// The `--help` text.
#[must_use]
pub(crate) fn help_text() -> String {
    format!(
        "{name} {version}\nA modern terminal file manager for every filesystem.\n\n\
         USAGE:\n    {name} [OPTIONS]\n\n\
         OPTIONS:\n    -h, --help       Print this help\n    -V, --version    Print version\n",
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
    fn help_text_mentions_name() {
        assert!(help_text().contains(crate::APP_NAME));
    }
}
