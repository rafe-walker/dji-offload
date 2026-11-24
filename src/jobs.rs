//! Who a flight is for. A flight can serve a commissioned **client**, the
//! **library** (already-flown catalog sold later with watermarked samples),
//! **promo** (your own marketing), or any mix.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub client: Option<String>,
    pub library: bool,
    pub promo: bool,
    /// Optional package name, e.g. "Land parcel" or "Aerial Photo + Video".
    pub package: Option<String>,
}

pub const LIBRARY: &str = "Library (catalog)";
pub const PROMO: &str = "Promo";
pub const NEW_CLIENT: &str = "New client…";
const CLIENT_PREFIX: &str = "Client: ";

impl Job {
    pub fn is_empty(&self) -> bool {
        self.client.is_none() && !self.library && !self.promo
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.client {
            parts.push(format!("Client: {c}"));
        }
        if self.library {
            parts.push("Library".into());
        }
        if self.promo {
            parts.push("Promo".into());
        }
        if let Some(p) = &self.package {
            parts.push(format!("Package: {p}"));
        }
        if parts.is_empty() { "Not tagged yet".into() } else { parts.join(" · ") }
    }

    /// Dialog choices that represent this job (for pre-selecting).
    pub fn choices(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.library {
            v.push(LIBRARY.to_string());
        }
        if self.promo {
            v.push(PROMO.to_string());
        }
        if let Some(c) = &self.client {
            v.push(format!("{CLIENT_PREFIX}{c}"));
        }
        v
    }

    /// Build a job from dialog picks. `new_client` is asked for only when
    /// "New client…" was picked.
    pub fn from_picks(picks: &[String], new_client: impl FnOnce() -> Option<String>) -> Job {
        let mut j = Job { library: picks.iter().any(|p| p == LIBRARY), promo: picks.iter().any(|p| p == PROMO), ..Job::default() };
        if picks.iter().any(|p| p == NEW_CLIENT) {
            j.client = new_client().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        } else {
            j.client = picks.iter().find_map(|p| p.strip_prefix(CLIENT_PREFIX).map(str::to_string));
        }
        j
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Ask who a flight is for. Returns `None` for "Later" (or off macOS).
pub fn ask(flight_label: &str, recent_clients: &[String], preset: &Job) -> Option<Job> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut items: Vec<String> = vec![LIBRARY.into(), PROMO.into()];
    items.extend(recent_clients.iter().map(|c| format!("{CLIENT_PREFIX}{c}")));
    items.push(NEW_CLIENT.into());
    let list = items.iter().map(|i| format!("\"{}\"", esc(i))).collect::<Vec<_>>().join(", ");
    let defaults = preset.choices().iter().map(|i| format!("\"{}\"", esc(i))).collect::<Vec<_>>().join(", ");
    let script = format!(
        "set picked to choose from list {{{list}}} with title \"Drone offload\" \
         with prompt \"Who is {} for? Pick any that apply.\" default items {{{defaults}}} \
         with multiple selections allowed and empty selection allowed \
         OK button name \"Save\" cancel button name \"Later\"\n\
         if picked is false then return \"<LATER>\"\n\
         set AppleScript's text item delimiters to \"\\n\"\n\
         return picked as text",
        esc(flight_label)
    );
    let out = std::process::Command::new("osascript").args(["-e", &script]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
    if !out.status.success() || text == "<LATER>" {
        return None;
    }
    let picks: Vec<String> = text.lines().map(str::to_string).filter(|l| !l.is_empty()).collect();
    Some(Job::from_picks(&picks, || {
        let s = "text returned of (display dialog \"Client name:\" default answer \"\" with title \"Drone offload\" buttons {\"Cancel\", \"OK\"} default button \"OK\")";
        let o = std::process::Command::new("osascript").args(["-e", s]).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_round_trip() {
        let picks = vec![LIBRARY.to_string(), "Client: Example Realty, LLC".to_string()];
        let j = Job::from_picks(&picks, || unreachable!());
        assert_eq!(j.client.as_deref(), Some("Example Realty, LLC"));
        assert!(j.library && !j.promo);
        assert_eq!(Job::from_picks(&j.choices(), || unreachable!()), j);
        let n = Job::from_picks(&[NEW_CLIENT.into(), PROMO.into()], || Some("  New Co ".into()));
        assert_eq!((n.client.as_deref(), n.promo), (Some("New Co"), true));
        assert_eq!(Job::default().label(), "Not tagged yet");
    }
}
