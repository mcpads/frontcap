//! Typed one-call stdio messages for Android bridge processes.
//! Process launch/deadlines live in `process`; recorder wiring remains separate from this schema.
use crate::request::{Completion, Identity, InvalidRequest, NativeRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeCall {
    Initialize {},
    Execute {
        identity: Identity,
        request: NativeRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeReply {
    Ready {
        identity: Identity,
        controller_id: String,
        source_sha256: String,
    },
    Completed {
        completion: Completion,
    },
    Unresolved {
        error: String,
    },
}

impl BridgeReply {
    /// Bind initialization to the frozen implementation before creating a request store.
    pub fn validate_ready(&self, source: &str) -> Result<(&Identity, &str), InvalidRequest> {
        let Self::Ready {
            identity,
            controller_id,
            source_sha256,
        } = self
        else {
            return Err(InvalidRequest("bridge did not initialize a target"));
        };
        identity.validate()?;
        if source_sha256 != source
            || source.len() != 64
            || !source
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || !canonical_uuid(controller_id)
        {
            return Err(InvalidRequest("bridge initialization identity differs"));
        }
        Ok((identity, controller_id))
    }
}
