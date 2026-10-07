//! Task invocation identities shared by native and proxied MCP tools.

use std::fmt::Write as _;
use std::io;

use anyhow::{anyhow, Context, Result};

const INVOCATION_PREFIX: &str = "tool-invocation";
const SESSION_NONCE_BYTES: usize = 16;

/// Allocates invocation identities for one MCP session.
///
/// An invocation id names persistent, never-replaced task evidence, so it must
/// stay unique across process restarts and concurrent sessions of one task.
/// Each session draws a 128-bit nonce from operating-system entropy once and
/// appends its own ordinal: `tool-invocation-{nonce}-{sequence}`. Two sessions
/// share a nonce only with negligible probability (below 2^-64 even after 2^32
/// sessions of one task), and artifact publication refuses an existing name if
/// that ever happens. Legacy `tool-invocation-{sequence}` handles have no
/// nonce component, so they cannot equal a new identity.
///
/// `sequence` keeps its existing meaning: a per-session ordinal that starts at
/// 1, orders invocations within the session, and pairs start and terminal
/// events. Entropy failure or sequence exhaustion fails closed without
/// consuming an identity.
#[derive(Default)]
pub(crate) struct InvocationAllocator {
    session_nonce: Option<String>,
    last_sequence: u64,
}

impl InvocationAllocator {
    /// Returns the next `(sequence, invocation_id)` for this session.
    pub(crate) fn allocate(&mut self) -> Result<(u64, String)> {
        self.allocate_with(fill_from_os_entropy)
    }

    fn allocate_with(
        &mut self,
        fill: impl FnOnce(&mut [u8; SESSION_NONCE_BYTES]) -> io::Result<()>,
    ) -> Result<(u64, String)> {
        let sequence = self.last_sequence.checked_add(1).ok_or_else(|| {
            anyhow!("MCP session exhausted its invocation sequence; restart the MCP server")
        })?;
        let nonce = match &self.session_nonce {
            Some(nonce) => nonce.clone(),
            None => {
                let mut bytes = [0_u8; SESSION_NONCE_BYTES];
                fill(&mut bytes)
                    .context("failed to draw an MCP session nonce from operating-system entropy")?;
                let nonce = bytes.iter().fold(
                    String::with_capacity(SESSION_NONCE_BYTES * 2),
                    |mut hex, byte| {
                        let _ = write!(hex, "{byte:02x}");
                        hex
                    },
                );
                self.session_nonce = Some(nonce.clone());
                nonce
            }
        };
        self.last_sequence = sequence;
        Ok((sequence, format!("{INVOCATION_PREFIX}-{nonce}-{sequence}")))
    }
}

/// Fills `bytes` from the operating-system CSPRNG (`getrandom(2)` with a
/// `/dev/urandom` fallback on Linux, `getentropy` on macOS, `BCryptGenRandom`
/// on Windows). Platforms without a source report an error, never a guess.
fn fill_from_os_entropy(bytes: &mut [u8; SESSION_NONCE_BYTES]) -> io::Result<()> {
    getrandom::getrandom(bytes).map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::cmd_mcp::artifact_io::ArtifactHandle;

    fn fixed(byte: u8) -> impl FnOnce(&mut [u8; SESSION_NONCE_BYTES]) -> io::Result<()> {
        move |bytes| {
            bytes.fill(byte);
            Ok(())
        }
    }

    #[test]
    fn sessions_keep_ordinals_and_draw_distinct_identities() {
        let mut first = InvocationAllocator::default();
        let mut second = InvocationAllocator::default();
        let mut identities = BTreeSet::new();
        for expected in 1..=3 {
            for allocator in [&mut first, &mut second] {
                let (sequence, invocation_id) = allocator.allocate().unwrap();
                assert_eq!(sequence, expected);
                assert!(identities.insert(invocation_id));
            }
        }
        // Fresh 128-bit nonces differ here with overwhelming probability.
        // The legacy shape is excluded structurally, not probabilistically.
        assert!(identities
            .iter()
            .all(|id| id.starts_with("tool-invocation-") && id.matches('-').count() == 3));
        assert!(identities
            .iter()
            .all(|id| ArtifactHandle::from_invocation(id, "result").is_ok()));
    }

    #[test]
    fn nonce_is_drawn_once_and_reused_for_the_session() {
        let mut allocator = InvocationAllocator::default();
        assert_eq!(
            allocator.allocate_with(fixed(0xab)).unwrap(),
            (1, format!("tool-invocation-{}-1", "ab".repeat(16)))
        );
        let (sequence, invocation_id) = allocator
            .allocate_with(|_| panic!("the session nonce must not be redrawn"))
            .unwrap();
        assert_eq!(sequence, 2);
        assert_eq!(
            invocation_id,
            format!("tool-invocation-{}-2", "ab".repeat(16))
        );
    }

    #[test]
    fn entropy_failure_fails_closed_without_consuming_an_identity() {
        let mut allocator = InvocationAllocator::default();
        let error = allocator
            .allocate_with(|_| Err(io::Error::other("entropy unavailable")))
            .unwrap_err();
        assert!(format!("{error:#}").contains("entropy unavailable"));
        assert_eq!(allocator.last_sequence, 0);
        assert!(allocator.session_nonce.is_none());

        assert_eq!(allocator.allocate_with(fixed(1)).unwrap().0, 1);
    }

    #[test]
    fn sequence_exhaustion_fails_closed_without_wrapping_or_repeating() {
        let mut allocator = InvocationAllocator::default();
        allocator.allocate_with(fixed(7)).unwrap();
        allocator.last_sequence = u64::MAX - 1;

        let (sequence, last) = allocator.allocate().unwrap();
        assert_eq!(sequence, u64::MAX);
        assert!(ArtifactHandle::from_invocation(&last, "result").is_ok());
        for _ in 0..2 {
            let error = allocator.allocate().unwrap_err();
            assert!(error.to_string().contains("exhausted"), "{error:#}");
            assert_eq!(allocator.last_sequence, u64::MAX);
        }
    }
}
