//! Reads the `--log-format internal-json` output of `nix build`, which ties
//! every build log line and every resource report to one build by activity
//! id, where the plain-text log only prefixes lines with a name that several
//! derivations share.

/// Nix's `actBuild` activity, one per derivation it builds.
const ACT_BUILD: u64 = 105;
/// A line of a build's log.
const RES_BUILD_LOG_LINE: u64 = 101;
/// The CPU time a finished builder used, which Nix sends only with
/// `log-profiling` enabled.
const RES_BUILD_RESOURCES: u64 = 1001;
/// The most verbose level that `nix build` prints by default.
const LVL_INFO: u64 = 3;

/// One line of the JSON log.  Times are in seconds, from Nix's `ts` field
/// where it has one.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Lines the plain-text log would print.
    Text(Vec<String>),
    BuildStarted {
        id: u64,
        drv_path: String,
        text: String,
        at: Option<f64>,
    },
    BuildLog {
        id: u64,
        line: String,
        at: Option<f64>,
    },
    BuildResources {
        id: u64,
        cpu_secs: f64,
    },
    Stopped {
        id: u64,
        at: Option<f64>,
    },
    Ignored,
}

/// Parses one line of `nix build --log-format internal-json`'s `stderr`.
/// A line without the `@nix ` marker, which Nix writes before its logger
/// starts, is kept as text.
pub fn parse(line: &str) -> Event {
    let Some(json) = line.strip_prefix("@nix ") else {
        return Event::Text(vec![line.to_string()]);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Event::Ignored;
    };
    let id = value["id"].as_u64().unwrap_or(0);
    let ty = value["type"].as_u64();
    let at = value["ts"].as_u64().map(|us| us as f64 / 1e6);
    let level = value["level"].as_u64().unwrap_or(0);
    let fields = &value["fields"];
    match value["action"].as_str() {
        Some("msg") if level <= LVL_INFO => Event::Text(
            value["msg"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
        ),
        Some("start") if ty == Some(ACT_BUILD) => Event::BuildStarted {
            id,
            drv_path: fields[0].as_str().unwrap_or_default().to_string(),
            text: value["text"].as_str().unwrap_or_default().to_string(),
            at,
        },
        Some("start") if level <= LVL_INFO => match value["text"].as_str() {
            Some(text) if !text.is_empty() => Event::Text(vec![text.to_string()]),
            _ => Event::Ignored,
        },
        Some("result") if ty == Some(RES_BUILD_LOG_LINE) => Event::BuildLog {
            id,
            line: fields[0].as_str().unwrap_or_default().to_string(),
            at,
        },
        Some("result") if ty == Some(RES_BUILD_RESOURCES) => {
            let us = |key: &str| field_after(fields, key).unwrap_or(0);
            Event::BuildResources {
                id,
                cpu_secs: (us("cpu-user-us") + us("cpu-system-us")) as f64 / 1e6,
            }
        }
        Some("stop") => Event::Stopped { id, at },
        _ => Event::Ignored,
    }
}

/// The number after `key` in a flat `[key, value, key, value, ...]` list.
fn field_after(fields: &serde_json::Value, key: &str) -> Option<u64> {
    let fields = fields.as_array()?;
    let pos = fields.iter().position(|f| f.as_str() == Some(key))?;
    fields.get(pos + 1)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_events_carry_their_activity_and_time() {
        let started = r#"@nix {"action":"start","fields":["/nix/store/h-a.drv","",1,1],"id":7,"level":3,"parent":6,"text":"building '/nix/store/h-a.drv'","ts":2000000,"type":105}"#;
        assert_eq!(
            parse(started),
            Event::BuildStarted {
                id: 7,
                drv_path: "/nix/store/h-a.drv".into(),
                text: "building '/nix/store/h-a.drv'".into(),
                at: Some(2.0),
            }
        );
        let log =
            r#"@nix {"action":"result","fields":["line-one"],"id":7,"ts":2500000,"type":101}"#;
        assert_eq!(
            parse(log),
            Event::BuildLog {
                id: 7,
                line: "line-one".into(),
                at: Some(2.5)
            }
        );
        let resources = r#"@nix {"action":"result","fields":["cpu-user-us",355201,"cpu-system-us",3947,"max-rss-bytes",29544448],"id":7,"ts":2600000,"type":1001}"#;
        assert_eq!(
            parse(resources),
            Event::BuildResources {
                id: 7,
                cpu_secs: 0.359148
            }
        );
        let stopped = r#"@nix {"action":"stop","id":7,"ts":2700000}"#;
        assert_eq!(
            parse(stopped),
            Event::Stopped {
                id: 7,
                at: Some(2.7)
            }
        );
    }

    #[test]
    fn messages_and_activities_up_to_info_level_are_text() {
        let msg = r#"@nix {"action":"msg","level":3,"msg":"these 2 derivations will be built:\n  /nix/store/h-a.drv"}"#;
        assert_eq!(
            parse(msg),
            Event::Text(vec![
                "these 2 derivations will be built:".into(),
                "  /nix/store/h-a.drv".into()
            ])
        );
        let copying = r#"@nix {"action":"start","id":8,"level":3,"text":"copying path '/nix/store/h-b'","type":100}"#;
        assert_eq!(
            parse(copying),
            Event::Text(vec!["copying path '/nix/store/h-b'".into()])
        );
        let debug = r#"@nix {"action":"msg","level":5,"msg":"acquiring lock"}"#;
        assert_eq!(parse(debug), Event::Ignored);
        assert_eq!(
            parse("warning: unknown setting"),
            Event::Text(vec!["warning: unknown setting".into()])
        );
    }
}
