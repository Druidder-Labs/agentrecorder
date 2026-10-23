use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey};
use serde::{Deserialize, Serialize};

pub const INTOTO_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<EnvelopeSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeSignature {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub keyid: String,
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DsseError {
    Malformed(String),
    Unsigned,
    BadSignature { keyid: String },
    UnknownSigner,
}

impl std::fmt::Display for DsseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "not a DSSE envelope: {why}"),
            Self::Unsigned => f.write_str("the envelope carries no signature"),
            Self::BadSignature { keyid } => {
                write!(
                    f,
                    "the signature does not verify with trusted key {keyid:?}; the payload or the signature was changed"
                )
            }
            Self::UnknownSigner => f.write_str("no trusted key verifies any signature on the envelope"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    Public(String),
    Private(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Public(why) => write!(f, "not a P-256 public key in PEM (SubjectPublicKeyInfo): {why}"),
            Self::Private(why) => write!(f, "not an unencrypted P-256 private key in PEM (PKCS#8): {why}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub name: String,
    key: VerifyingKey,
}

impl TrustedKey {
    pub fn from_pem(name: &str, pem: &str) -> Result<Self, KeyError> {
        let key = VerifyingKey::from_public_key_pem(pem).map_err(|e| KeyError::Public(e.to_string()))?;
        Ok(Self { name: name.to_string(), key })
    }
}

pub struct SigningIdentity {
    pub keyid: String,
    key: SigningKey,
}

impl SigningIdentity {
    pub fn from_pem(keyid: &str, pem: &str) -> Result<Self, KeyError> {
        let key = SigningKey::from_pkcs8_pem(pem).map_err(|e| KeyError::Private(e.to_string()))?;
        Ok(Self { keyid: keyid.to_string(), key })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub signer: String,
}

pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {} {} ", payload_type.len(), payload_type, payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

pub fn parse(bytes: &[u8]) -> Result<Envelope, DsseError> {
    serde_json::from_slice(bytes).map_err(|e| DsseError::Malformed(e.to_string()))
}

pub fn sign(payload_type: &str, payload: &[u8], who: &SigningIdentity) -> Envelope {
    let signature: Signature = who.key.sign(&pae(payload_type, payload));
    Envelope {
        payload_type: payload_type.to_string(),
        payload: STANDARD.encode(payload),
        signatures: vec![EnvelopeSignature {
            keyid: who.keyid.clone(),
            sig: STANDARD.encode(signature.to_der().as_bytes()),
        }],
    }
}

fn decode_signature(sig: &str) -> Option<Signature> {
    let bytes = STANDARD.decode(sig).ok()?;
    Signature::from_der(&bytes).or_else(|_| Signature::from_slice(&bytes)).ok()
}

pub fn verify(envelope: &Envelope, keys: &[TrustedKey]) -> Result<Verified, DsseError> {
    let payload =
        STANDARD.decode(&envelope.payload).map_err(|e| DsseError::Malformed(format!("payload is not base64: {e}")))?;
    if envelope.signatures.is_empty() {
        return Err(DsseError::Unsigned);
    }
    let message = pae(&envelope.payload_type, &payload);
    let mut named_failure = None;
    for s in &envelope.signatures {
        if let Some(signature) = decode_signature(&s.sig) {
            for k in keys {
                if k.key.verify(&message, &signature).is_ok() {
                    return Ok(Verified {
                        payload_type: envelope.payload_type.clone(),
                        payload,
                        signer: k.name.clone(),
                    });
                }
            }
        }
        if !s.keyid.is_empty() && keys.iter().any(|k| k.name == s.keyid) {
            named_failure = Some(s.keyid.clone());
        }
    }
    Err(named_failure.map_or(DsseError::UnknownSigner, |keyid| DsseError::BadSignature { keyid }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI_PUB: &str = include_str!("../tests/fixtures/ci.pub.pem");
    const CI_KEY: &str = include_str!("../tests/fixtures/ci.key.pem");
    const OTHER_PUB: &str = include_str!("../tests/fixtures/other.pub.pem");
    const OTHER_KEY: &str = include_str!("../tests/fixtures/other.key.pem");
    const OPENSSL_SIGNED: &str = include_str!("../tests/fixtures/openssl-signed.dsse.json");

    fn ci() -> Vec<TrustedKey> {
        vec![TrustedKey::from_pem("ci", CI_PUB).unwrap()]
    }

    #[test]
    fn pae_matches_the_dsse_specification_example() {
        assert_eq!(
            pae("http://example.com/HelloWorld", b"hello world"),
            b"DSSEv1 29 http://example.com/HelloWorld 11 hello world"
        );
    }

    #[test]
    fn an_envelope_signed_by_openssl_verifies() {
        let env = parse(OPENSSL_SIGNED.as_bytes()).unwrap();
        let v = verify(&env, &ci()).unwrap();
        assert_eq!(v.signer, "ci");
        assert_eq!(v.payload_type, INTOTO_PAYLOAD_TYPE);
        assert!(String::from_utf8(v.payload).unwrap().contains("terraform-validate"));
    }

    #[test]
    fn a_changed_payload_fails_and_names_the_key() {
        let mut env = parse(OPENSSL_SIGNED.as_bytes()).unwrap();
        let body = String::from_utf8(STANDARD.decode(&env.payload).unwrap()).unwrap().replace("PASSED", "FAILED");
        env.payload = STANDARD.encode(body);
        assert_eq!(verify(&env, &ci()), Err(DsseError::BadSignature { keyid: "ci".into() }));
    }

    #[test]
    fn a_changed_payload_type_fails() {
        let mut env = parse(OPENSSL_SIGNED.as_bytes()).unwrap();
        env.payload_type = "application/json".into();
        assert!(matches!(verify(&env, &ci()), Err(DsseError::BadSignature { .. })));
    }

    #[test]
    fn a_signature_by_another_key_does_not_verify() {
        let other = SigningIdentity::from_pem("ci", OTHER_KEY).unwrap();
        let env = sign(INTOTO_PAYLOAD_TYPE, b"{}", &other);
        assert_eq!(verify(&env, &ci()), Err(DsseError::BadSignature { keyid: "ci".into() }));
        let anonymous = SigningIdentity::from_pem("", OTHER_KEY).unwrap();
        assert_eq!(verify(&sign(INTOTO_PAYLOAD_TYPE, b"{}", &anonymous), &ci()), Err(DsseError::UnknownSigner));
        let both = vec![TrustedKey::from_pem("ci", CI_PUB).unwrap(), TrustedKey::from_pem("other", OTHER_PUB).unwrap()];
        assert_eq!(verify(&env, &both).unwrap().signer, "other");
    }

    #[test]
    fn signing_round_trips_and_unsigned_or_malformed_envelopes_fail() {
        let me = SigningIdentity::from_pem("ci", CI_KEY).unwrap();
        let env = sign(INTOTO_PAYLOAD_TYPE, b"{\"a\":1}", &me);
        let text = serde_json::to_string(&env).unwrap();
        let v = verify(&parse(text.as_bytes()).unwrap(), &ci()).unwrap();
        assert_eq!(v.payload, b"{\"a\":1}");
        let mut unsigned = env.clone();
        unsigned.signatures.clear();
        assert_eq!(verify(&unsigned, &ci()), Err(DsseError::Unsigned));
        let mut garbage = env.clone();
        garbage.signatures[0].sig = "not base64!".into();
        assert_eq!(verify(&garbage, &ci()), Err(DsseError::BadSignature { keyid: "ci".into() }));
        let mut bad_payload = env;
        bad_payload.payload = "%%%".into();
        assert!(matches!(verify(&bad_payload, &ci()), Err(DsseError::Malformed(_))));
        assert!(matches!(parse(b"{\"payload\":1}"), Err(DsseError::Malformed(_))));
    }

    #[test]
    fn keys_in_the_wrong_format_are_refused() {
        assert!(matches!(TrustedKey::from_pem("ci", CI_KEY), Err(KeyError::Public(_))));
        assert!(matches!(SigningIdentity::from_pem("ci", CI_PUB), Err(KeyError::Private(_))));
    }
}
