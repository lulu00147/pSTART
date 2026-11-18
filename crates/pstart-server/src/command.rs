//! Command-line modes for the server executable.

/// Requested server mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerCommand {
    /// Run under the Windows Service Control Manager.
    Service,
    /// Run in the current console until Enter is pressed.
    Console,
    /// Install the Windows Service.
    Install,
    /// Remove the Windows Service.
    Uninstall,
    /// Print usage.
    Help,
    /// Print the server version.
    Version,
}

/// Parse arguments after the executable name.
pub fn parse_command(arguments: impl IntoIterator<Item = String>) -> Result<ServerCommand, String> {
    let mut arguments = arguments.into_iter();
    let first = arguments.next();
    if arguments.next().is_some() {
        return Err("unexpected extra arguments".to_owned());
    }
    match first.as_deref() {
        None | Some("service") => Ok(ServerCommand::Service),
        Some("console") => Ok(ServerCommand::Console),
        Some("install") => Ok(ServerCommand::Install),
        Some("uninstall") => Ok(ServerCommand::Uninstall),
        Some("help" | "--help" | "-h") => Ok(ServerCommand::Help),
        Some("version" | "--version" | "-V") => Ok(ServerCommand::Version),
        Some(other) => Err(format!("unknown command: {other}")),
    }
}

/// Usage text.
#[must_use]
pub fn usage() -> &'static str {
    "pstart-server [service|console|install|uninstall|version|help]"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> Result<ServerCommand, String> {
        parse_command(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn commands_are_parsed() {
        assert_eq!(parse(&[]).unwrap(), ServerCommand::Service);
        assert_eq!(parse(&["service"]).unwrap(), ServerCommand::Service);
        assert_eq!(parse(&["console"]).unwrap(), ServerCommand::Console);
        assert_eq!(parse(&["install"]).unwrap(), ServerCommand::Install);
        assert_eq!(parse(&["uninstall"]).unwrap(), ServerCommand::Uninstall);
        assert_eq!(parse(&["help"]).unwrap(), ServerCommand::Help);
        assert_eq!(parse(&["--help"]).unwrap(), ServerCommand::Help);
        assert_eq!(parse(&["-h"]).unwrap(), ServerCommand::Help);
        assert_eq!(parse(&["version"]).unwrap(), ServerCommand::Version);
        assert_eq!(parse(&["--version"]).unwrap(), ServerCommand::Version);
        assert_eq!(parse(&["-V"]).unwrap(), ServerCommand::Version);
    }

    #[test]
    fn invalid_commands_are_rejected() {
        assert!(parse(&["bogus"]).is_err());
        assert!(parse(&["console", "extra"]).is_err());
        assert!(usage().contains("console"));
    }
}
