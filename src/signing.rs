//! Signing capabilities and utilities.

use crate::types::H256;

/// Error during signing.
#[derive(Debug, derive_more::Display, PartialEq, Clone)]
pub enum SigningError {
    /// A message to sign is invalid. It must be exactly 32 bytes.
    #[display("Message has to be exactly 32 bytes.")]
    InvalidMessage,
    /// The chain ID is too large to encode an EIP-155 recovery value.
    #[display("Chain ID is too large to encode safely.")]
    InvalidChainId,
    /// The transaction type is not supported by the signer.
    #[display("Unsupported transaction type: {_0}.")]
    UnsupportedTransactionType(u64),
    /// The signature implementation returned an invalid recovery identifier.
    #[display("Signature recovery identifier is invalid.")]
    InvalidRecoveryId,
    /// The signature implementation returned invalid or non-canonical scalar values.
    #[display("Signature scalar values are invalid or non-canonical.")]
    InvalidSignature,
}
impl std::error::Error for SigningError {}

/// Error during sender recovery.
#[derive(Debug, derive_more::Display, PartialEq, Clone)]
pub enum RecoveryError {
    /// A message to recover is invalid. It must be exactly 32 bytes.
    #[display("Message has to be exactly 32 bytes.")]
    InvalidMessage,
    /// A signature is invalid and the sender could not be recovered.
    #[display("Signature is invalid (check recovery id).")]
    InvalidSignature,
}
impl std::error::Error for RecoveryError {}

#[cfg(feature = "signing")]
pub use feature_gated::*;

#[cfg(feature = "signing")]
mod feature_gated {
    use super::*;
    use crate::types::Address;
    use once_cell::sync::Lazy;
    pub use secp256k1::SecretKey;
    use secp256k1::{
        ecdsa::{RecoverableSignature, RecoveryId},
        All, Message, PublicKey, Secp256k1,
    };
    use std::ops::Deref;

    static CONTEXT: Lazy<Secp256k1<All>> = Lazy::new(Secp256k1::new);

    /// A trait representing ethereum-compatible key with signing capabilities.
    ///
    /// The purpose of this trait is to prevent leaking `secp256k1::SecretKey` struct
    /// in stack or memory.
    /// To use secret keys securely, they should be wrapped in a struct that prevents
    /// leaving copies in memory (both when it's moved or dropped). Please take a look
    /// at:
    /// - <https://github.com/graphprotocol/solidity-bindgen/blob/master/solidity-bindgen/src/secrets.rs>
    /// - or <https://crates.io/crates/zeroize>
    ///
    /// if you care enough about your secrets to be used securely.
    ///
    /// If it's enough to pass a reference to `SecretKey` (lifetimes) than you can use `SecretKeyRef`
    /// wrapper.
    pub trait Key {
        /// Sign given message and include chain-id replay protection.
        ///
        /// When a chain ID is provided, the `Signature`'s V-value will have chain replay
        /// protection added (as per EIP-155). Otherwise, the V-value will be in
        /// 'Electrum' notation.
        fn sign(&self, message: &[u8], chain_id: Option<u64>) -> Result<Signature, SigningError>;

        /// Sign given message without manipulating V-value; used for typed transactions
        /// (AccessList and EIP-1559)
        fn sign_message(&self, message: &[u8]) -> Result<Signature, SigningError>;

        /// Get public address that this key represents.
        fn address(&self) -> Address;
    }

    /// A `SecretKey` reference wrapper.
    ///
    /// A wrapper around `secp256k1::SecretKey` reference, which enables it to be used in methods expecting
    /// `Key` capabilities.
    pub struct SecretKeyRef<'a> {
        pub(super) key: &'a SecretKey,
    }

    impl<'a> SecretKeyRef<'a> {
        /// A simple wrapper around a reference to `SecretKey` which allows it to be usable for signing.
        pub fn new(key: &'a SecretKey) -> Self {
            Self { key }
        }
    }

    impl<'a> From<&'a SecretKey> for SecretKeyRef<'a> {
        fn from(key: &'a SecretKey) -> Self {
            Self::new(key)
        }
    }

    impl<'a> Deref for SecretKeyRef<'a> {
        type Target = SecretKey;

        fn deref(&self) -> &Self::Target {
            self.key
        }
    }

    impl<T: Deref<Target = SecretKey>> Key for T {
        fn sign(&self, message: &[u8], chain_id: Option<u64>) -> Result<Signature, SigningError> {
            let digest = <[u8; 32]>::try_from(message).map_err(|_| SigningError::InvalidMessage)?;
            let message = Message::from_digest(digest);
            let (recovery_id, signature) = CONTEXT.sign_ecdsa_recoverable(message, self).serialize_compact();

            let standard_v = match recovery_id {
                RecoveryId::Zero => 0_u64,
                RecoveryId::One => 1_u64,
                _ => return Err(SigningError::InvalidRecoveryId),
            };
            let v = if let Some(chain_id) = chain_id {
                // When signing with a chain ID, add chain replay protection.
                chain_id
                    .checked_mul(2)
                    .and_then(|value| value.checked_add(35))
                    .and_then(|value| value.checked_add(standard_v))
                    .ok_or(SigningError::InvalidChainId)?
            } else {
                // Otherwise, convert to 'Electrum' notation.
                // The Ethereum recovery ID was validated as 0 or 1, so this is at most 28.
                #[allow(
                    clippy::arithmetic_side_effects,
                    reason = "validated recovery parity is at most one, so the sum is at most 28"
                )]
                let electrum_v = standard_v + 27;
                electrum_v
            };
            let r = H256::from_slice(&signature[..32]);
            let s = H256::from_slice(&signature[32..]);

            Ok(Signature { v, r, s })
        }

        fn sign_message(&self, message: &[u8]) -> Result<Signature, SigningError> {
            let digest = <[u8; 32]>::try_from(message).map_err(|_| SigningError::InvalidMessage)?;
            let message = Message::from_digest(digest);
            let (recovery_id, signature) = CONTEXT.sign_ecdsa_recoverable(message, self).serialize_compact();

            let v = match recovery_id {
                RecoveryId::Zero => 0_u64,
                RecoveryId::One => 1_u64,
                _ => return Err(SigningError::InvalidRecoveryId),
            };
            let r = H256::from_slice(&signature[..32]);
            let s = H256::from_slice(&signature[32..]);

            Ok(Signature { v, r, s })
        }

        fn address(&self) -> Address {
            secret_key_address(self)
        }
    }

    /// Recover a sender, given message and the signature.
    ///
    /// Signature and `recovery_id` can be obtained from `types::Recovery` type.
    pub fn recover(message: &[u8], signature: &[u8], recovery_id: i32) -> Result<Address, RecoveryError> {
        let digest = <[u8; 32]>::try_from(message).map_err(|_| RecoveryError::InvalidMessage)?;
        let message = Message::from_digest(digest);
        let recovery_id = RecoveryId::try_from(recovery_id).map_err(|_| RecoveryError::InvalidSignature)?;
        let signature =
            RecoverableSignature::from_compact(signature, recovery_id).map_err(|_| RecoveryError::InvalidSignature)?;
        let public_key = CONTEXT
            .recover_ecdsa(message, &signature)
            .map_err(|_| RecoveryError::InvalidSignature)?;

        Ok(public_key_address(&public_key))
    }

    /// Validate scalar range and Ethereum's low-S transaction-signature requirement.
    pub(crate) fn validate_signature(signature: &Signature) -> Result<(), SigningError> {
        const CURVE_ORDER: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ];
        const HALF_CURVE_ORDER: [u8; 32] = [
            0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46,
            0x68, 0x1b, 0x20, 0xa0,
        ];
        let r = signature.r.as_bytes();
        let s = signature.s.as_bytes();
        if r.iter().all(|byte| *byte == 0)
            || r >= CURVE_ORDER.as_ref()
            || s.iter().all(|byte| *byte == 0)
            || s > HALF_CURVE_ORDER.as_ref()
        {
            return Err(SigningError::InvalidSignature);
        }
        Ok(())
    }

    /// Gets the address of a public key.
    ///
    /// The public address is defined as the low 20 bytes of the keccak hash of
    /// the public key. Note that the public key returned from the `secp256k1`
    /// crate is 65 bytes long, that is because it is prefixed by `0x04` to
    /// indicate an uncompressed public key; this first byte is ignored when
    /// computing the hash.
    pub(crate) fn public_key_address(public_key: &PublicKey) -> Address {
        let public_key = public_key.serialize_uncompressed();

        debug_assert_eq!(public_key[0], 0x04);
        let hash = keccak256(&public_key[1..]);

        Address::from_slice(&hash[12..])
    }

    /// Gets the public address of a private key.
    pub(crate) fn secret_key_address(key: &SecretKey) -> Address {
        let secp = &*CONTEXT;
        let public_key = PublicKey::from_secret_key(secp, key);
        public_key_address(&public_key)
    }
}

/// A struct that represents the components of a secp256k1 signature.
pub struct Signature {
    /// V component in electrum format with chain-id replay protection.
    pub v: u64,
    /// R component of the signature.
    pub r: H256,
    /// S component of the signature.
    pub s: H256,
}

/// Compute the Keccak-256 hash of input bytes.
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(bytes);
    hasher.finalize(&mut output);
    output
}

/// Result of the name hash algorithm.
pub type NameHash = [u8; 32];

/// Compute the hash of a domain name using the namehash algorithm.
///
/// [Specification](https://docs.ens.domains/contract-api-reference/name-processing#hashing-names)
pub fn namehash(name: &str) -> NameHash {
    let mut node = [0u8; 32];

    if name.is_empty() {
        return node;
    }

    let mut labels: Vec<&str> = name.split('.').collect();

    labels.reverse();

    for label in labels.iter() {
        let label_hash = keccak256(label.as_bytes());

        node = keccak256(&[node, label_hash].concat());
    }

    node
}

/// Hash a message according to EIP-191.
///
/// The data is a UTF-8 encoded string and will enveloped as follows:
/// `"\x19Ethereum Signed Message:\n" + message.length + message` and hashed
/// using keccak256.
pub fn hash_message<S>(message: S) -> H256
where
    S: AsRef<[u8]>,
{
    let message = message.as_ref();

    let mut eth_message = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
    eth_message.extend_from_slice(message);

    keccak256(&eth_message).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    //See -> https://eips.ethereum.org/EIPS/eip-137 for test cases

    #[cfg(feature = "signing")]
    #[test]
    fn validates_signature_scalar_boundaries() {
        use hex_literal::hex;

        let one = H256::from(hex!(
            "0000000000000000000000000000000000000000000000000000000000000001"
        ));
        let mut signature = Signature { v: 27, r: one, s: one };
        assert_eq!(validate_signature(&signature), Ok(()));

        signature.r = H256::from(hex!(
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
        ));
        assert_eq!(validate_signature(&signature), Err(SigningError::InvalidSignature));

        signature.r = one;
        signature.s = H256::from(hex!(
            "7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a1"
        ));
        assert_eq!(validate_signature(&signature), Err(SigningError::InvalidSignature));
    }

    #[test]
    fn name_hash_empty() {
        let input = "";

        let result = namehash(input);

        let expected = [0u8; 32];

        assert_eq!(expected, result);
    }

    #[test]
    fn name_hash_eth() {
        let input = "eth";

        let result = namehash(input);

        let expected = [
            0x93, 0xcd, 0xeb, 0x70, 0x8b, 0x75, 0x45, 0xdc, 0x66, 0x8e, 0xb9, 0x28, 0x01, 0x76, 0x16, 0x9d, 0x1c, 0x33,
            0xcf, 0xd8, 0xed, 0x6f, 0x04, 0x69, 0x0a, 0x0b, 0xcc, 0x88, 0xa9, 0x3f, 0xc4, 0xae,
        ];

        assert_eq!(expected, result);
    }

    #[test]
    fn name_hash_foo_eth() {
        let input = "foo.eth";

        let result = namehash(input);

        let expected = [
            0xde, 0x9b, 0x09, 0xfd, 0x7c, 0x5f, 0x90, 0x1e, 0x23, 0xa3, 0xf1, 0x9f, 0xec, 0xc5, 0x48, 0x28, 0xe9, 0xc8,
            0x48, 0x53, 0x98, 0x01, 0xe8, 0x65, 0x91, 0xbd, 0x98, 0x01, 0xb0, 0x19, 0xf8, 0x4f,
        ];

        assert_eq!(expected, result);
    }
}
