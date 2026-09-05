//! Text/JSON output for every command.
//!
//! Each command returns a serializable value that also knows how to render
//! itself as text, so `-o json` is uniform rather than bolted onto
//! whichever commands remembered it

use clap::ValueEnum;

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Debug, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

pub trait CommandOutput: serde::Serialize {
    fn to_text(&self) -> String;

    fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| {
            // Serializing our own owned types cannot realistically fail,
            // but swallowing it silently would print an empty object and
            // look like a successful empty result.
            format!("{{\"error\":\"failed to serialize output: {e}\"}}")
        })
    }

    fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.to_text(),
            OutputFormat::Json => self.to_json(),
        }
    }
}
