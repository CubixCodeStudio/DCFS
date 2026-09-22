//! Base64url-encoded filename bytes for wire format.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use dcfs_core::{NodeName, NodeNameError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Wire type for filenames: raw bytes encoded as unpadded base64url.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NameBytes(pub Vec<u8>);

impl NameBytes {
    pub fn new(bytes: Vec<u8>) -> Result<Self, NodeNameError> {
        // Validate as a proper node name
        let _ = NodeName::new(bytes.clone())?;
        Ok(Self(bytes))
    }

    pub fn from_name(name: &NodeName) -> Self {
        Self(name.as_bytes().to_vec())
    }

    pub fn to_name(&self) -> Result<NodeName, NodeNameError> {
        NodeName::new(self.0.clone())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn to_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    pub fn from_base64(s: &str) -> Result<Self, base64::DecodeError> {
        let bytes = URL_SAFE_NO_PAD.decode(s)?;
        Ok(Self(bytes))
    }
}

impl Serialize for NameBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = URL_SAFE_NO_PAD.encode(&self.0);
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for NameBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(&s)
            .map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}
