//! Signed, domain-separated evidence for human-reviewed incoming source files.
//! This proves possession of the coordinator's installation key for these
//! exact bytes. It does NOT prove the active remote bearer/session, receiver
//! authorization, absence of conflicts, or permission to install.
use crate::remote_project_sync::ProposedSourceFile;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ring::signature::{self};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;

const KIND: &str = "RepoTunnelIncomingSourceReviewV1";
const DOMAIN: &[u8] = b"RepoTunnel-human-source-review-v1\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Body {
    pub(crate) schema_version: u32,
    pub(crate) kind: String,
    pub(crate) remote_workspace_id: String,
    pub(crate) receiver_machine_fingerprint: String,
    pub(crate) coordinator_machine_fingerprint: String,
    pub(crate) coordinator_workspace_id: String,
    pub(crate) coordinator_connection_nonce: String,
    pub(crate) coordinator_source_fingerprint: String,
    pub(crate) source_review_id: String,
    pub(crate) files: Vec<ProposedSourceFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignedReview {
    #[serde(flatten)]
    pub(crate) body: Body,
    pub(crate) identity_public_key: String,
    pub(crate) identity_signature: String,
}

impl Body {
    pub(crate) fn from_intent(
        intent: &crate::remote_source_intent::Intent,
        coordinator_machine_fingerprint: String,
    ) -> Self {
        Self {
            schema_version: 1,
            kind: KIND.into(),
            remote_workspace_id: intent.remote_workspace_id.clone(),
            receiver_machine_fingerprint: intent.machine_identity_fingerprint.clone(),
            coordinator_machine_fingerprint,
            coordinator_workspace_id: intent.local_workspace_id.clone(),
            coordinator_connection_nonce: intent.connection_nonce.clone(),
            coordinator_source_fingerprint: intent.source_fingerprint.clone(),
            source_review_id: intent.id.clone(),
            files: intent.proposed.clone(),
        }
    }
}

pub(crate) fn bytes(body: &Body) -> Result<Vec<u8>, String> {
    if body.schema_version != 1
        || body.kind != KIND
        || body.remote_workspace_id.is_empty()
        || body.coordinator_workspace_id.is_empty()
        || body.files.is_empty()
        || body.files.len() > 8
    {
        return Err("Only an exact bounded source-review-v1 manifest may be signed.".into());
    }
    let json = serde_json::to_vec(body)
        .map_err(|_| "Cannot encode a canonical human source review.".to_string())?;
    if json.len() > 8192 - DOMAIN.len() {
        return Err("Source review body exceeds its signed size bound.".into());
    }
    let mut data = Vec::with_capacity(DOMAIN.len() + json.len());
    data.extend_from_slice(DOMAIN);
    data.extend_from_slice(&json);
    Ok(data)
}

pub(crate) fn sign(app: &AppHandle, body: Body) -> Result<SignedReview, String> {
    let signable = bytes(&body)?;
    let (identity_public_key, identity_signature) =
        crate::remote_fleet::sign_human_source_review_bytes(app, &signable)?;
    let signed = SignedReview {
        body,
        identity_public_key,
        identity_signature,
    };
    verify(&signed)?;
    Ok(signed)
}

pub(crate) fn verify(review: &SignedReview) -> Result<(), String> {
    let bytes = bytes(&review.body)?;
    let public = BASE64
        .decode(&review.identity_public_key)
        .map_err(|_| "Coordinator source review public key is invalid base64.".to_string())?;
    let signature = BASE64
        .decode(&review.identity_signature)
        .map_err(|_| "Coordinator source review signature is invalid base64.".to_string())?;
    if public.len() != 32 || signature.len() != 64 {
        return Err(
            "Coordinator source review signing key or signature has invalid length.".into(),
        );
    }
    let fingerprint: String = Sha256::digest(&public)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if fingerprint != review.body.coordinator_machine_fingerprint {
        return Err(
            "Signed source review key does not match the claimed coordinator installation.".into(),
        );
    }
    signature::UnparsedPublicKey::new(&signature::ED25519, &public)
        .verify(&bytes, &signature)
        .map_err(|_| "Source review signature is invalid; exact projects, hashes, pairing and paths must be preserved.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    fn sample_body() -> Body {
        let key = Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
        Body {
            schema_version: 1,
            kind: KIND.into(),
            remote_workspace_id: "remote".into(),
            receiver_machine_fingerprint: "e".repeat(64),
            coordinator_machine_fingerprint: Sha256::digest(key.public_key().as_ref())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            coordinator_workspace_id: "local".into(),
            coordinator_connection_nonce: format!("node-{}", "b".repeat(24)),
            coordinator_source_fingerprint: "a".repeat(64),
            source_review_id: format!("source-review-{}", "c".repeat(32)),
            files: vec![ProposedSourceFile {
                path: "assets/example.bin".into(),
                bytes: 19,
                sha256: "d".repeat(64),
            }],
        }
    }
    fn signed() -> SignedReview {
        let key = Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
        let body = sample_body();
        let digest = bytes(&body).unwrap();
        SignedReview {
            body,
            identity_public_key: BASE64.encode(key.public_key().as_ref()),
            identity_signature: BASE64.encode(key.sign(&digest).as_ref()),
        }
    }
    #[test]
    fn proof_binds_exact_sender_receiver_project_nonce_and_file_hash() {
        let mut proof = signed();
        verify(&proof).unwrap();
        proof.body.files[0].sha256 = "f".repeat(64);
        assert!(verify(&proof).is_err());
        let mut proof = signed();
        proof.body.receiver_machine_fingerprint = "a".repeat(64);
        assert!(verify(&proof).is_err());
        let mut proof = signed();
        proof.body.coordinator_connection_nonce = format!("node-{}", "e".repeat(24));
        assert!(verify(&proof).is_err());
        let mut proof = signed();
        proof.body.remote_workspace_id = "other".into();
        assert!(verify(&proof).is_err());
        let mut proof = signed();
        proof.body.coordinator_machine_fingerprint = "0".repeat(64);
        assert!(verify(&proof).is_err());
    }
    #[test]
    fn proof_roundtrip_preserves_public_key_and_canonical_fields() {
        let review = signed();
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(json["kind"], KIND);
        assert_eq!(json["files"][0]["bytes"], 19);
        assert!(json.get("identitySignature").is_some());
        let decoded: SignedReview = serde_json::from_value(json).unwrap();
        verify(&decoded).unwrap();
    }
}
