use std::path::PathBuf;
use std::time::Duration;

use crate::glitch::HoldTimes;

pub const USAGE: &str = "\
usage: glitchlock [OPTION]...

  --image PATH       PNG shown on every output, scaled to cover it
  --color RRGGBB     background when no image is given or it fails to load
  --pam SERVICE      PAM service to authenticate with, may be repeated to run
                     several services in parallel (default: glitchlock)
  --typing-ms MS     how long a glitch frame stays after a key press (default: 100)
  --reject-ms MS     how long the glitch stays after a rejection (default: 600)
  --clear-ms MS      how long the glitch stays after Escape (default: 250)";

const DEFAULT_COLOR: u32 = 0x0d0d0a;
const DEFAULT_HOLD_TIMES: HoldTimes = HoldTimes {
    typing: Duration::from_millis(100),
    rejected: Duration::from_millis(600),
    cleared: Duration::from_millis(250),
};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub image: Option<PathBuf>,
    /// Background color as `0xRRGGBB`.
    pub color: u32,
    pub pam_services: Vec<String>,
    pub hold_times: HoldTimes,
}

impl Config {
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut config = Config {
            image: None,
            color: DEFAULT_COLOR,
            pam_services: Vec::new(),
            hold_times: DEFAULT_HOLD_TIMES,
        };

        let mut arguments = arguments.iter();
        while let Some(flag) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--image" => config.image = Some(PathBuf::from(value()?)),
                "--color" => config.color = parse_color(value()?)?,
                "--pam" => config.pam_services.push(value()?.clone()),
                "--typing-ms" => config.hold_times.typing = parse_milliseconds(value()?)?,
                "--reject-ms" => config.hold_times.rejected = parse_milliseconds(value()?)?,
                "--clear-ms" => config.hold_times.cleared = parse_milliseconds(value()?)?,
                _ => return Err(format!("unknown argument {flag}")),
            }
        }

        if config.pam_services.is_empty() {
            config.pam_services.push(String::from("glitchlock"));
        }
        Ok(config)
    }
}

fn parse_color(text: &str) -> Result<u32, String> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    let is_valid = digits.len() == 6 && digits.chars().all(|digit| digit.is_ascii_hexdigit());
    if !is_valid {
        return Err(format!("invalid color {text}, expected RRGGBB"));
    }
    u32::from_str_radix(digits, 16).map_err(|error| error.to_string())
}

fn parse_milliseconds(text: &str) -> Result<Duration, String> {
    text.parse()
        .map(Duration::from_millis)
        .map_err(|_| format!("invalid duration {text}, expected milliseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Config, String> {
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect();
        Config::parse(&arguments)
    }

    #[test]
    fn defaults_to_the_glitchlock_service() {
        assert_eq!(
            parse(&[]),
            Ok(Config {
                image: None,
                color: DEFAULT_COLOR,
                pam_services: vec![String::from("glitchlock")],
                hold_times: DEFAULT_HOLD_TIMES,
            })
        );
    }

    #[test]
    fn parses_all_flags() {
        let config = parse(&[
            "--image",
            "/a.png",
            "--color",
            "#FF8000",
            "--pam",
            "a",
            "--pam",
            "b",
            "--typing-ms",
            "80",
            "--reject-ms",
            "1000",
            "--clear-ms",
            "300",
        ]);

        assert_eq!(
            config,
            Ok(Config {
                image: Some(PathBuf::from("/a.png")),
                color: 0xff8000,
                pam_services: vec![String::from("a"), String::from("b")],
                hold_times: HoldTimes {
                    typing: Duration::from_millis(80),
                    rejected: Duration::from_secs(1),
                    cleared: Duration::from_millis(300),
                },
            })
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&["--typing-ms", "-1"]).is_err());
        assert!(parse(&["--color", "12345"]).is_err());
        assert!(parse(&["--color", "12345g"]).is_err());
        assert!(parse(&["--image"]).is_err());
        assert!(parse(&["--unknown"]).is_err());
    }
}
