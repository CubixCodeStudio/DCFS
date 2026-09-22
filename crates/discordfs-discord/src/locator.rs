//! Discord locator for mapping ObjectIds to Discord message/attachment IDs.

use discordfs_core::ObjectId;
use discordfs_objectstore::ObjectLocator;
use serde::{Deserialize, Serialize};

/// A Discord-specific locator that maps an ObjectId to a Discord message
/// and attachment, enabling stable retrieval of uploaded objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordLocator {
    /// The logical object ID.
    pub object_id: ObjectId,
    /// The Discord message ID that contains the attachment.
    pub message_id: String,
    /// The Discord attachment ID within that message.
    pub attachment_id: String,
    /// Which webhook uploaded it.
    ///
    /// A webhook can only fetch and delete its own messages, so with more than
    /// one configured this is the difference between finding an object again
    /// and losing it. Empty for objects written when there was only one.
    pub webhook_id: String,
    /// The CDN URL for direct download.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
}

impl DiscordLocator {
    pub fn new(
        object_id: ObjectId,
        message_id: String,
        attachment_id: String,
        url: String,
        size: u64,
    ) -> Self {
        Self {
            object_id,
            message_id,
            attachment_id,
            webhook_id: String::new(),
            url,
            size,
        }
    }

    pub fn from_webhook(mut self, webhook_id: impl Into<String>) -> Self {
        self.webhook_id = webhook_id.into();
        self
    }

    /// Convert to a generic ObjectLocator.
    pub fn to_object_locator(&self) -> ObjectLocator {
        ObjectLocator::new(self.object_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn locator_roundtrip() {
        let oid = ObjectId::new();
        let loc = DiscordLocator::new(
            oid,
            "msg_123".into(),
            "att_456".into(),
            "https://cdn.discord.com/attachments/foo".into(),
            1024,
        );
        assert_eq!(loc.object_id, oid);
        assert_eq!(loc.message_id, "msg_123");
        assert_eq!(loc.attachment_id, "att_456");
        assert_eq!(loc.size, 1024);
    }

    #[test]
    fn locator_serializes() {
        let oid = ObjectId::from_uuid(Uuid::nil());
        let loc = DiscordLocator::new(oid, "m".into(), "a".into(), "http://x".into(), 10);
        let json = serde_json::to_string(&loc).unwrap();
        let parsed: DiscordLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.object_id, oid);
    }
}
