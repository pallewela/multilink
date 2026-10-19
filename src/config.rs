use crate::balance::Policy;
use ipnet::IpNet;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;

/// `--config <file>` (TOML). Every field is optional; command-line flags win over the file.
/// See multilink.example.toml.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub policy: Option<Policy>,
    pub probe: Option<SocketAddr>,
    /// Status page address ("127.0.0.1:7878") or "off".
    pub status: Option<String>,
    /// Use exactly these interfaces. Empty = auto-detect.
    pub links: Vec<String>,
    /// Never use these interfaces when auto-detecting.
    pub exclude: Vec<String>,
    /// Relative share of new destinations per link under the sticky policy. Default 1.
    pub weights: HashMap<String, f64>,
    /// Send destinations in a range to a preferred link while it is healthy.
    pub pin: Vec<Pin>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub dst: IpNet,
    pub link: String,
}

pub fn load(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    parse(&text).map_err(|e| format!("{path}: {e}"))
}

pub(crate) fn parse(text: &str) -> Result<Config, String> {
    let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    if let Some((name, w)) = c.weights.iter().find(|(_, w)| !(w.is_finite() && **w > 0.0)) {
        return Err(format!("weight for {name} must be a positive number, got {w}"));
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_file_parses_and_bad_input_is_rejected() {
        let c = parse(include_str!("../multilink.example.toml")).unwrap();
        assert!(matches!(c.policy, Some(Policy::Sticky)));
        assert_eq!(c.weights["en0"], 3.0);
        assert_eq!(c.pin[0].dst, "17.0.0.0/8".parse::<IpNet>().unwrap());
        assert!(parse("").is_ok());
        assert!(parse("polcy = 'sticky'").unwrap_err().contains("unknown field"));
        assert!(parse("policy = 'fastest'").is_err());
        assert!(parse("[weights]\nen0 = 0").unwrap_err().contains("positive"));
        assert!(parse("[[pin]]\ndst = '10.0.0.0/33'\nlink = 'en0'").is_err());
    }
}
