//! Host-supplied ROM catalog.
//!
//! The library never embeds or auto-discovers a `roms.json`.  The host loads
//! the catalog from its own config and passes it into prepare/download helpers
//! and `crate::create_emulator`.

use std::path::Path;

/// Parsed ROM set catalog (JSON object keyed by set name).
#[derive(Debug, Clone)]
pub struct RomCatalog {
    root: serde_json::Value,
}

impl RomCatalog {
    /// Parse catalog JSON text supplied by the host.
    pub fn parse(json: &str) -> Result<Self, String> {
        let root: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("cannot parse ROM catalog JSON: {e}"))?;
        if !root.is_object() {
            return Err("ROM catalog root must be a JSON object".into());
        }
        Ok(Self { root })
    }

    /// Read catalog JSON from a host path (e.g. `./roms.json`).
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Look up the `platform` field for a set name (`"cps1"`, `"neogeo"`, …).
    pub fn platform_for(&self, name: &str) -> Option<&str> {
        self.root
            .get(name)?
            .get("platform")?
            .as_str()
    }

    /// True if the catalog has an entry for `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.root.get(name).is_some()
    }

    /// Borrow the root object map.
    pub fn entries(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.root.as_object()
    }

    /// Borrow a single entry object.
    pub fn entry(&self, name: &str) -> Option<&serde_json::Value> {
        self.root.get(name)
    }
}
