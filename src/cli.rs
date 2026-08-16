use anyhow::{anyhow, Result};

const HELP: &str = "wluma - automatic brightness adjustment

Usage:
  wluma [COMMAND] [OPTIONS]

Commands:
  daemon
  status [--json]
  get [brightness|dim|temperature] OUTPUT
  set [brightness|dim|temperature] OUTPUT VALUE
  pause [OUTPUT | --all] [--for DURATION]
  resume [OUTPUT | --all]
  toggle [OUTPUT | --all]
  watch [--json]
  version
  help

Examples:
  wluma set brightness eDP-1 +5%
  wluma set dim DP-1 20%
  wluma set temperature DP-1 4500K
  wluma set temperature DP-1 neutral
  wluma pause --all --for 3h";

pub enum Mode {
    Daemon,
    Command { request: String, stream: bool },
    Print(String),
}

pub fn parse() -> Result<Mode> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(Mode::Daemon),
        [command] if command == "daemon" => Ok(Mode::Daemon),
        [command] if command == "help" || command == "--help" || command == "-h" => {
            Ok(Mode::Print(HELP.to_string()))
        }
        [command] if command == "version" || command == "--version" || command == "-V" => {
            Ok(Mode::Print(format!("wluma {}", crate::VERSION)))
        }
        [command] if command == "status" => command_mode("status", false),
        [command, flag] if command == "status" && flag == "--json" => {
            command_mode("status-json", false)
        }
        [command, output] if command == "get" => {
            command_mode(&format!("get\tbrightness\t{output}"), false)
        }
        [command, property, output] if command == "get" => {
            crate::control::parse_property(property)?;
            command_mode(&format!("get\t{property}\t{output}"), false)
        }
        [command, output, value] if command == "set" => {
            crate::control::parse_value_adjustment(crate::control::Property::Brightness, value)?;
            command_mode(&format!("set\tbrightness\t{output}\t{value}"), false)
        }
        [command, property, output, value] if command == "set" => {
            let property = crate::control::parse_property(property)?;
            crate::control::parse_value_adjustment(property, value)?;
            command_mode(
                &format!("set\t{}\t{output}\t{value}", property.as_str()),
                false,
            )
        }
        [command] if command == "pause" => command_mode("pause\t*\t-", false),
        [command, target] if command == "pause" && target == "--all" => {
            command_mode("pause\t*\t-", false)
        }
        [command, target] if command == "pause" => {
            command_mode(&format!("pause\t{target}\t-"), false)
        }
        [command, target, duration] if command == "pause" && target == "--for" => {
            command_mode(&format!("pause\t*\t{}", parse_duration(duration)?), false)
        }
        [command, target, flag, duration] if command == "pause" && flag == "--for" => {
            let target = if target == "--all" { "*" } else { target };
            command_mode(
                &format!("pause\t{target}\t{}", parse_duration(duration)?),
                false,
            )
        }
        [command] if command == "resume" => command_mode("resume\t*", false),
        [command, target] if command == "resume" => {
            let target = if target == "--all" { "*" } else { target };
            command_mode(&format!("resume\t{target}"), false)
        }
        [command] if command == "toggle" => command_mode("toggle\t*", false),
        [command, target] if command == "toggle" => {
            let target = if target == "--all" { "*" } else { target };
            command_mode(&format!("toggle\t{target}"), false)
        }
        [command] if command == "watch" => command_mode("watch", true),
        [command, flag] if command == "watch" && flag == "--json" => {
            command_mode("watch-json", true)
        }
        _ => Err(anyhow!("invalid arguments\n\n{HELP}")),
    }
}

fn command_mode(request: &str, stream: bool) -> Result<Mode> {
    Ok(Mode::Command {
        request: request.to_string(),
        stream,
    })
}

fn parse_duration(value: &str) -> Result<u64> {
    let (number, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 60 * 60),
        _ => return Err(anyhow!("duration must end in s, m, or h")),
    };
    let number = number.parse::<u64>()?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("duration is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("45m").unwrap(), 2700);
        assert_eq!(parse_duration("3h").unwrap(), 10800);
        assert!(parse_duration("3").is_err());
    }
}
