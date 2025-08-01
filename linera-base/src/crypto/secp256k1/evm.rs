// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Defines EIP-191 compatible signature primitives used by the Linera protocol.

use std::{
    borrow::Cow,
    fmt,
    hash::{Hash, Hasher},
    str::FromStr,
};

use alloy_primitives::{eip191_hash_message, Signature};
use k256::{
    ecdsa::{SigningKey, VerifyingKey},
    elliptic_curve::sec1::FromEncodedPoint,
    EncodedPoint,
};
use linera_witty::{
    GuestPointer, HList, InstanceWithMemory, Layout, Memory, Runtime, RuntimeError, RuntimeMemory,
    WitLoad, WitStore, WitType,
};
use serde::{Deserialize, Serialize};

use super::{BcsHashable, BcsSignable, CryptoError, CryptoHash, HasTypeName};
use crate::doc_scalar;

/// Name of the secp256k1 scheme.
const EVM_SECP256K1_SCHEME_LABEL: &str = "evm_secp256k1";

/// Length of secp256k1 compressed public key.
const EVM_SECP256K1_PUBLIC_KEY_SIZE: usize = 33;

/// Length of secp256k1 signature.
const EVM_SECP256K1_SIGNATURE_SIZE: usize = 65;

/// A secp256k1 secret key.
pub struct EvmSecretKey(pub SigningKey);

impl Eq for EvmSecretKey {}
impl PartialEq for EvmSecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bytes() == other.0.to_bytes()
    }
}

/// A secp256k1 public key.
#[derive(Eq, PartialEq, Ord, PartialOrd, Copy, Clone)]
pub struct EvmPublicKey(pub VerifyingKey);

impl Hash for EvmPublicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_encoded_point(true).as_bytes().hash(state);
    }
}

/// Secp256k1 public/secret key pair.
#[derive(Debug, PartialEq, Eq)]
pub struct EvmKeyPair {
    /// Secret key.
    pub secret_key: EvmSecretKey,
    /// Public key.
    pub public_key: EvmPublicKey,
}

/// A secp256k1 signature.
#[derive(Eq, PartialEq, Copy, Clone)]
pub struct EvmSignature(pub Signature);

impl FromStr for EvmSignature {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // If the string starts with "0x", we remove it before decoding.
        let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
        Self::from_slice(&bytes)
    }
}

impl EvmPublicKey {
    /// A fake public key used for testing.
    #[cfg(with_testing)]
    pub fn test_key(seed: u8) -> Self {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed as u64);
        let sk = k256::SecretKey::random(&mut rng);
        Self(sk.public_key().into())
    }

    /// Returns the bytes of the public key in compressed representation.
    pub fn as_bytes(&self) -> [u8; EVM_SECP256K1_PUBLIC_KEY_SIZE] {
        // UNWRAP: We already have valid key so conversion should not fail.
        self.0.to_encoded_point(true).as_bytes().try_into().unwrap()
    }

    /// Decodes the bytes into the public key.
    /// Expects the bytes to be of compressed representation.
    ///
    /// Panics if the encoding can't be done in a constant time.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let encoded_point =
            EncodedPoint::from_bytes(bytes).map_err(|_| CryptoError::IncorrectPublicKeySize {
                scheme: EVM_SECP256K1_SCHEME_LABEL,
                len: bytes.len(),
                expected: EVM_SECP256K1_PUBLIC_KEY_SIZE,
            })?;

        match k256::PublicKey::from_encoded_point(&encoded_point).into_option() {
            Some(public_key) => Ok(Self(public_key.into())),
            None => {
                let error = CryptoError::Secp256k1PointAtInfinity(hex::encode(bytes));
                Err(error)
            }
        }
    }

    /// Returns an EVM address for the public key.
    pub fn address(&self) -> alloy_primitives::Address {
        alloy_primitives::Address::from_public_key(&self.0)
    }

    /// Recovers the public key from the signature and the value.
    ///
    /// This function turns the `value` into a `CryptoHash`, then hashes it using EIP-191
    /// and finally recovers the public key from the signature.
    pub fn recover_from_msg<'de, T>(
        signature: &EvmSignature,
        value: &T,
    ) -> Result<Self, CryptoError>
    where
        T: BcsSignable<'de>,
    {
        let message = CryptoHash::new(value).as_bytes().0;
        let public_key =
            signature
                .0
                .recover_from_msg(message)
                .map_err(|_| CryptoError::InvalidSignature {
                    error: "Failed to recover public key from signature".to_string(),
                    type_name: Self::type_name().to_string(),
                })?;
        Ok(EvmPublicKey(public_key))
    }
}

impl fmt::Debug for EvmSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted for secp256k1 secret key>")
    }
}

impl Serialize for EvmSecretKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        // This is only used for JSON configuration.
        assert!(serializer.is_human_readable());
        serializer.serialize_str(&hex::encode(self.0.to_bytes()))
    }
}

impl<'de> Deserialize<'de> for EvmSecretKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        // This is only used for JSON configuration.
        assert!(deserializer.is_human_readable());
        let str = String::deserialize(deserializer)?;
        let bytes = hex::decode(&str).map_err(serde::de::Error::custom)?;
        let sk = SigningKey::from_slice(&bytes).map_err(serde::de::Error::custom)?;
        Ok(EvmSecretKey(sk))
    }
}

#[cfg(with_testing)]
impl FromStr for EvmSecretKey {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s)?;
        let sk = SigningKey::from_slice(&bytes).expect("Failed to create secret key");
        Ok(EvmSecretKey(sk))
    }
}

impl Serialize for EvmPublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&hex::encode(self.as_bytes()))
        } else {
            let compact_pk = serde_utils::CompressedPublicKey(self.as_bytes());
            serializer.serialize_newtype_struct("EvmPublicKey", &compact_pk)
        }
    }
}

impl<'de> Deserialize<'de> for EvmPublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let value = hex::decode(s).map_err(serde::de::Error::custom)?;
            Ok(EvmPublicKey::from_bytes(&value).map_err(serde::de::Error::custom)?)
        } else {
            #[derive(Deserialize)]
            #[serde(rename = "EvmPublicKey")]
            struct PublicKey(serde_utils::CompressedPublicKey);
            let compact = PublicKey::deserialize(deserializer)?;
            Ok(EvmPublicKey::from_bytes(&compact.0 .0).map_err(serde::de::Error::custom)?)
        }
    }
}

impl FromStr for EvmPublicKey {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex::decode(s.strip_prefix("0x").unwrap_or(s))?
            .as_slice()
            .try_into()
    }
}

impl TryFrom<&[u8]> for EvmPublicKey {
    type Error = CryptoError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Self::from_bytes(value)
    }
}

impl fmt::Display for EvmPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let str = hex::encode(self.as_bytes());
        write!(f, "{}", str)
    }
}

impl fmt::Debug for EvmPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..", hex::encode(&self.as_bytes()[0..9]))
    }
}

impl BcsHashable<'_> for EvmPublicKey {}

impl WitType for EvmPublicKey {
    const SIZE: u32 = <(u64, u64, u64, u64, u8) as WitType>::SIZE;
    type Layout = <(u64, u64, u64, u64, u8) as WitType>::Layout;
    type Dependencies = HList![];

    fn wit_type_name() -> Cow<'static, str> {
        "evm-secp256k1-public-key".into()
    }

    fn wit_type_declaration() -> Cow<'static, str> {
        concat!(
            "    record evm-secp256k1-public-key {\n",
            "        part1: u64,\n",
            "        part2: u64,\n",
            "        part3: u64,\n",
            "        part4: u64,\n",
            "        part5: u8\n",
            "    }\n",
        )
        .into()
    }
}

impl WitLoad for EvmPublicKey {
    fn load<Instance>(
        memory: &Memory<'_, Instance>,
        location: GuestPointer,
    ) -> Result<Self, RuntimeError>
    where
        Instance: InstanceWithMemory,
        <Instance::Runtime as Runtime>::Memory: RuntimeMemory<Instance>,
    {
        let (part1, part2, part3, part4, part5) = WitLoad::load(memory, location)?;
        Ok(Self::from((part1, part2, part3, part4, part5)))
    }

    fn lift_from<Instance>(
        flat_layout: <Self::Layout as Layout>::Flat,
        memory: &Memory<'_, Instance>,
    ) -> Result<Self, RuntimeError>
    where
        Instance: InstanceWithMemory,
        <Instance::Runtime as Runtime>::Memory: RuntimeMemory<Instance>,
    {
        let (part1, part2, part3, part4, part5) = WitLoad::lift_from(flat_layout, memory)?;
        Ok(Self::from((part1, part2, part3, part4, part5)))
    }
}

impl WitStore for EvmPublicKey {
    fn store<Instance>(
        &self,
        memory: &mut Memory<'_, Instance>,
        location: GuestPointer,
    ) -> Result<(), RuntimeError>
    where
        Instance: InstanceWithMemory,
        <Instance::Runtime as Runtime>::Memory: RuntimeMemory<Instance>,
    {
        let (part1, part2, part3, part4, part5) = (*self).into();
        (part1, part2, part3, part4, part5).store(memory, location)
    }

    fn lower<Instance>(
        &self,
        memory: &mut Memory<'_, Instance>,
    ) -> Result<<Self::Layout as Layout>::Flat, RuntimeError>
    where
        Instance: InstanceWithMemory,
        <Instance::Runtime as Runtime>::Memory: RuntimeMemory<Instance>,
    {
        let (part1, part2, part3, part4, part5) = (*self).into();
        (part1, part2, part3, part4, part5).lower(memory)
    }
}

impl From<(u64, u64, u64, u64, u8)> for EvmPublicKey {
    fn from((part1, part2, part3, part4, part5): (u64, u64, u64, u64, u8)) -> Self {
        let mut bytes = [0u8; EVM_SECP256K1_PUBLIC_KEY_SIZE];
        bytes[0..8].copy_from_slice(&part1.to_be_bytes());
        bytes[8..16].copy_from_slice(&part2.to_be_bytes());
        bytes[16..24].copy_from_slice(&part3.to_be_bytes());
        bytes[24..32].copy_from_slice(&part4.to_be_bytes());
        bytes[32] = part5;
        Self::from_bytes(&bytes).unwrap()
    }
}

impl From<EvmPublicKey> for (u64, u64, u64, u64, u8) {
    fn from(key: EvmPublicKey) -> Self {
        let bytes = key.as_bytes();
        let part1 = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
        let part2 = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let part3 = u64::from_be_bytes(bytes[16..24].try_into().unwrap());
        let part4 = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
        let part5 = bytes[32];
        (part1, part2, part3, part4, part5)
    }
}

impl EvmKeyPair {
    /// Generates a new key pair.
    #[cfg(all(with_getrandom, with_testing))]
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        Self::generate_from(&mut rng)
    }

    /// Generates a new key pair from the given RNG. Use with care.
    #[cfg(with_getrandom)]
    pub fn generate_from<R: crate::crypto::CryptoRng>(rng: &mut R) -> Self {
        let secret_key = EvmSecretKey(SigningKey::random(rng));
        let public_key = secret_key.public();
        EvmKeyPair {
            secret_key,
            public_key,
        }
    }
}

impl EvmSecretKey {
    /// Returns a public key for the given secret key.
    pub fn public(&self) -> EvmPublicKey {
        EvmPublicKey(*self.0.verifying_key())
    }

    /// Copies the key pair, **including the secret key**.
    ///
    /// The `Clone` and `Copy` traits are deliberately not implemented for `EvmSecretKey` to prevent
    /// accidental copies of secret keys.
    pub fn copy(&self) -> Self {
        Self(self.0.clone())
    }

    /// Generates a new key pair.
    #[cfg(all(with_getrandom, with_testing))]
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        Self::generate_from(&mut rng)
    }

    /// Generates a new key pair from the given RNG. Use with care.
    #[cfg(with_getrandom)]
    pub fn generate_from<R: crate::crypto::CryptoRng>(rng: &mut R) -> Self {
        EvmSecretKey(SigningKey::random(rng))
    }

    /// Returns an EVM address for the public key.
    pub fn address(&self) -> alloy_primitives::Address {
        alloy_primitives::Address::from_private_key(&self.0)
    }
}

impl EvmSignature {
    /// Computes a secp256k1 signature for `prehash` using the given `secret`.
    pub fn new(prehash: CryptoHash, secret: &EvmSecretKey) -> Self {
        Self::sign_prehash(secret, prehash)
    }

    /// Computes a signature from a prehash.
    pub fn sign_prehash(secret: &EvmSecretKey, prehash: CryptoHash) -> Self {
        let message = eip191_hash_message(prehash.as_bytes().0).0;
        let (signature, rid) = secret
            .0
            .sign_prehash_recoverable(&message)
            .expect("Failed to sign prehashed data"); // NOTE: This is a critical error we don't control.
        EvmSignature((signature, rid).into())
    }

    /// Checks a signature.
    pub fn check<'de, T>(&self, value: &T, author: EvmPublicKey) -> Result<(), CryptoError>
    where
        T: BcsSignable<'de> + fmt::Debug,
    {
        let prehash = CryptoHash::new(value).as_bytes().0;
        self.verify_inner::<T>(prehash, author)
    }

    /// Checks a signature against a recovered public key.
    pub fn check_with_recover<'de, T>(
        &self,
        value: &T,
        sender_address: [u8; 20],
    ) -> Result<EvmPublicKey, CryptoError>
    where
        T: BcsSignable<'de> + fmt::Debug,
    {
        let msg = CryptoHash::new(value).as_bytes().0;
        let recovered_public_key = match self.0.recover_from_msg(msg) {
            Ok(public_key) => EvmPublicKey(public_key),
            Err(_) => {
                return Err(CryptoError::InvalidSignature {
                    error: "Failed to recover public key from signature".to_string(),
                    type_name: T::type_name().to_string(),
                });
            }
        };
        if recovered_public_key.address() != alloy_primitives::Address::new(sender_address) {
            return Err(CryptoError::InvalidSignature {
                error: "Recovered public key does not match sender address".to_string(),
                type_name: T::type_name().to_string(),
            });
        }
        Ok(recovered_public_key)
    }

    /// Verifies a batch of signatures.
    ///
    /// Returns an error on first failed signature.
    pub fn verify_batch<'a, 'de, T, I>(value: &'a T, votes: I) -> Result<(), CryptoError>
    where
        T: BcsSignable<'de> + fmt::Debug,
        I: IntoIterator<Item = &'a (EvmPublicKey, EvmSignature)>,
    {
        let prehash = CryptoHash::new(value).as_bytes().0;
        for (author, signature) in votes {
            signature.verify_inner::<T>(prehash, *author)?;
        }
        Ok(())
    }

    /// Returns the byte representation of the signature.
    pub fn as_bytes(&self) -> [u8; EVM_SECP256K1_SIGNATURE_SIZE] {
        self.0.as_bytes()
    }

    fn verify_inner<'de, T>(
        &self,
        prehash: [u8; 32],
        author: EvmPublicKey,
    ) -> Result<(), CryptoError>
    where
        T: BcsSignable<'de> + fmt::Debug,
    {
        use k256::ecdsa::signature::hazmat::PrehashVerifier;

        let message_hash = eip191_hash_message(prehash).0;

        author
            .0
            .verify_prehash(
                &message_hash,
                &self.0.to_k256().map_err(CryptoError::Secp256k1Error)?,
            )
            .map_err(|error| CryptoError::InvalidSignature {
                error: error.to_string(),
                type_name: T::type_name().to_string(),
            })
    }

    /// Creates a signature from the bytes.
    /// Expects the signature to be serialized in raw-bytes form.
    pub fn from_slice<A: AsRef<[u8]>>(bytes: A) -> Result<Self, CryptoError> {
        let bytes = bytes.as_ref();
        let sig = alloy_primitives::Signature::from_raw(bytes).map_err(|_| {
            CryptoError::IncorrectSignatureBytes {
                scheme: EVM_SECP256K1_SCHEME_LABEL,
                len: bytes.len(),
                expected: EVM_SECP256K1_SIGNATURE_SIZE,
            }
        })?;
        Ok(EvmSignature(sig))
    }
}

impl Serialize for EvmSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&hex::encode(self.as_bytes()))
        } else {
            let compact = serde_utils::CompactSignature(self.as_bytes());
            serializer.serialize_newtype_struct("EvmSignature", &compact)
        }
    }
}

impl<'de> Deserialize<'de> for EvmSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let value = hex::decode(s).map_err(serde::de::Error::custom)?;
            Self::from_slice(&value).map_err(serde::de::Error::custom)
        } else {
            #[derive(Deserialize)]
            #[serde(rename = "EvmSignature")]
            struct Signature(serde_utils::CompactSignature);

            let value = Signature::deserialize(deserializer)?;
            Self::from_slice(value.0 .0.as_ref()).map_err(serde::de::Error::custom)
        }
    }
}

impl fmt::Display for EvmSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = hex::encode(self.as_bytes());
        write!(f, "{}", s)
    }
}

impl fmt::Debug for EvmSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..", hex::encode(&self.as_bytes()[0..9]))
    }
}

doc_scalar!(EvmSignature, "A secp256k1 signature value");
doc_scalar!(EvmPublicKey, "A secp256k1 public key value");

mod serde_utils {
    use serde::{Deserialize, Serialize};
    use serde_with::serde_as;

    use super::{EVM_SECP256K1_PUBLIC_KEY_SIZE, EVM_SECP256K1_SIGNATURE_SIZE};

    /// Wrapper around compact signature serialization
    /// so that we can implement custom serializer for it that uses fixed length.
    // Serde treats arrays larger than 32 as variable length arrays, and adds the length as a prefix.
    // Since we want a fixed size representation, we wrap it in this helper struct and use serde_as.
    #[serde_as]
    #[derive(Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CompactSignature(#[serde_as(as = "[_; 65]")] pub [u8; EVM_SECP256K1_SIGNATURE_SIZE]);

    #[serde_as]
    #[derive(Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CompressedPublicKey(
        #[serde_as(as = "[_; 33]")] pub [u8; EVM_SECP256K1_PUBLIC_KEY_SIZE],
    );
}

#[cfg(with_testing)]
mod tests {
    #[test]
    fn eip191_compatibility() {
        use std::str::FromStr;

        use crate::crypto::{CryptoHash, EvmSecretKey, EvmSignature};

        // Generated in MetaMask.
        let secret_key = "f77a21701522a03b01c111ad2d2cdaf2b8403b47507ee0aec3c2e52b765d7a66";
        let signer = EvmSecretKey::from_str(secret_key).unwrap();

        let crypto_hash = CryptoHash::from_str(
            "c520e2b24b05e70c39c36d4aa98e9129ac0079ea002d4c382e6996ea11946d1e",
        )
        .unwrap();

        let signature = EvmSignature::new(crypto_hash, &signer);
        let js_signature = EvmSignature::from_str("0xe257048813b851f812ba6e508e972d8bb09504824692b027ca95d31301dbe8c7103a2f35ce9950d031d260f412dcba09c24027288872a67abe261c0a3e55c9121b").unwrap();
        assert_eq!(signature, js_signature);
    }

    #[test]
    fn test_signatures() {
        use serde::{Deserialize, Serialize};

        use crate::crypto::{
            secp256k1::evm::{EvmKeyPair, EvmSignature},
            BcsSignable, CryptoHash, TestString,
        };

        #[derive(Debug, Serialize, Deserialize)]
        struct Foo(String);

        impl BcsSignable<'_> for Foo {}

        let keypair1 = EvmKeyPair::generate();
        let keypair2 = EvmKeyPair::generate();

        let ts = TestString("hello".into());
        let ts_cryptohash = CryptoHash::new(&ts);
        let tsx = TestString("hellox".into());
        let foo = Foo("hello".into());

        let s = EvmSignature::new(ts_cryptohash, &keypair1.secret_key);
        assert!(s.check(&ts, keypair1.public_key).is_ok());
        assert!(s.check(&ts, keypair2.public_key).is_err());
        assert!(s.check(&tsx, keypair1.public_key).is_err());
        assert!(s.check(&foo, keypair1.public_key).is_err());
    }

    #[test]
    fn test_public_key_serialization() {
        use crate::crypto::secp256k1::evm::EvmPublicKey;
        let key_in = EvmPublicKey::test_key(0);
        let s = serde_json::to_string(&key_in).unwrap();
        let key_out: EvmPublicKey = serde_json::from_str(&s).unwrap();
        assert_eq!(key_out, key_in);

        let s = bcs::to_bytes(&key_in).unwrap();
        let key_out: EvmPublicKey = bcs::from_bytes(&s).unwrap();
        assert_eq!(key_out, key_in);
    }

    #[test]
    fn test_secret_key_serialization() {
        use crate::crypto::secp256k1::evm::{EvmKeyPair, EvmSecretKey};
        let key_in = EvmKeyPair::generate().secret_key;
        let s = serde_json::to_string(&key_in).unwrap();
        let key_out: EvmSecretKey = serde_json::from_str(&s).unwrap();
        assert_eq!(key_out, key_in);
    }

    #[test]
    fn test_signature_serialization() {
        use crate::crypto::{
            secp256k1::evm::{EvmKeyPair, EvmSignature},
            CryptoHash, TestString,
        };
        let keypair = EvmKeyPair::generate();
        let prehash = CryptoHash::new(&TestString("hello".into()));
        let sig = EvmSignature::new(prehash, &keypair.secret_key);
        let s = serde_json::to_string(&sig).unwrap();
        let sig2: EvmSignature = serde_json::from_str(&s).unwrap();
        assert_eq!(sig, sig2);

        let s = bcs::to_bytes(&sig).unwrap();
        let sig2: EvmSignature = bcs::from_bytes(&s).unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn public_key_from_str() {
        use std::str::FromStr;

        use crate::crypto::secp256k1::evm::EvmPublicKey;
        let key = EvmPublicKey::test_key(0);
        let s = key.to_string();
        let key2 = EvmPublicKey::from_str(s.as_str()).unwrap();
        assert_eq!(key, key2);
    }

    #[test]
    fn bytes_repr_compact_public_key() {
        use crate::crypto::secp256k1::evm::{EvmPublicKey, EVM_SECP256K1_PUBLIC_KEY_SIZE};
        let key_in: EvmPublicKey = EvmPublicKey::test_key(0);
        let bytes = key_in.as_bytes();
        assert!(
            bytes.len() == EVM_SECP256K1_PUBLIC_KEY_SIZE,
            "::to_bytes() should return compressed representation"
        );
        let key_out = EvmPublicKey::from_bytes(&bytes).unwrap();
        assert_eq!(key_in, key_out);
    }

    #[test]
    fn human_readable_ser() {
        use crate::crypto::{
            secp256k1::evm::{EvmKeyPair, EvmSignature},
            CryptoHash, TestString,
        };
        let key_pair = EvmKeyPair::generate();
        let prehash = CryptoHash::new(&TestString("hello".into()));
        let sig = EvmSignature::new(prehash, &key_pair.secret_key);
        let s = serde_json::to_string(&sig).unwrap();
        let sig2: EvmSignature = serde_json::from_str(&s).unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn public_key_recovery() {
        use crate::crypto::{
            secp256k1::evm::{EvmKeyPair, EvmPublicKey, EvmSignature},
            CryptoHash, TestString,
        };
        let key_pair = EvmKeyPair::generate();
        let address = key_pair.public_key.address();
        let msg = TestString("hello".into());
        let prehash = CryptoHash::new(&msg);
        let sig = EvmSignature::new(prehash, &key_pair.secret_key);

        sig.check_with_recover(&msg, address.0 .0).unwrap();

        let public_key = EvmPublicKey::recover_from_msg(&sig, &msg).unwrap();
        assert_eq!(public_key, key_pair.public_key);
    }
}
