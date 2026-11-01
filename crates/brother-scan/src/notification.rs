use std::collections::BTreeMap;

use crate::{Result, protocol};

/// An identified Scan to PC event. Source-address validation belongs to the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub function: String,
    pub registration_id: String,
    pub sequence: String,
}

impl Notification {
    pub fn parse(packet: &[u8], name: &str, callback: &str) -> Result<Self> {
        if !(5..=4096).contains(&packet.len())
            || packet[0] != 2
            || packet[1] != 0
            || packet[3] != 0x30
        {
            return Err(protocol("invalid notification envelope"));
        }
        // The third byte varies in observed events; it is not a payload length.
        let text = std::str::from_utf8(&packet[4..])
            .map_err(|_| protocol("non-ASCII notification"))?
            .trim_end_matches('\0');
        if !text.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
            return Err(protocol("non-ASCII notification"));
        }
        let mut fields = BTreeMap::new();
        for item in text.split(';').filter(|s| !s.is_empty()) {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| protocol("missing '='"))?;
            let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                &value[1..value.len() - 1]
            } else {
                value
            };
            if value.contains('"') || fields.insert(key, value).is_some() {
                return Err(protocol("duplicate field or malformed quoting"));
            }
        }
        for (key, expected) in [
            ("TYPE", "BR"),
            ("BUTTON", "SCAN"),
            ("USER", name),
            ("HOST", callback),
        ] {
            if fields.get(key) != Some(&expected) {
                return Err(protocol(format!("notification {key} mismatch")));
            }
        }
        let function = fields.get("FUNC").copied().unwrap_or_default();
        if !["FILE", "IMAGE", "OCR", "EMAIL"].contains(&function) {
            return Err(protocol("unsupported scan function"));
        }
        let identity = |key| -> Result<String> {
            let value = fields.get(key).copied().unwrap_or_default();
            if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(protocol("invalid notification identity"));
            }
            Ok(value.to_owned())
        };
        Ok(Self {
            function: function.to_owned(),
            registration_id: identity("REGID")?,
            sequence: identity("SEQ")?,
        })
    }
}
