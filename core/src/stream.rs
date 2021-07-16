use anyhow::Result;
use ed25519_dalek::{Keypair, PublicKey, Signature, Signer};
use fnv::FnvHashSet;
use parking_lot::Mutex;
use rkyv::ser::serializers::AllocSerializer;
use rkyv::ser::Serializer;
use rkyv::{AlignedVec, Archive, Deserialize, Serialize};
use std::sync::Arc;
use zerocopy::{AsBytes, FromBytes};

#[derive(Archive, Deserialize, Serialize, AsBytes, FromBytes, Clone, Copy, Eq, Hash, PartialEq)]
#[archive(as = "StreamId")]
#[repr(C)]
#[cfg_attr(feature = "serde-derive", derive(serde::Deserialize, serde::Serialize))]
pub struct StreamId {
    peer: [u8; 32],
    stream: u64,
}

impl std::fmt::Debug for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let peer = base64::encode_config(&self.peer, base64::URL_SAFE_NO_PAD);
        write!(f, "{}.{}", peer, self.stream)
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::str::FromStr for StreamId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (peer, stream) = s
            .split_once('.')
            .ok_or(anyhow::anyhow!("invalid stream id"))?;
        let mut bytes = base64::decode_config(peer, base64::URL_SAFE_NO_PAD)?;
        bytes.resize(32, 0);
        let mut peer = [0; 32];
        peer.copy_from_slice(&bytes);
        let stream = stream.parse()?;
        Ok(Self { peer, stream })
    }
}

impl StreamId {
    pub fn new(peer: [u8; 32], stream: u64) -> Self {
        Self { peer, stream }
    }

    pub fn peer(&self) -> PublicKey {
        PublicKey::from_bytes(&self.peer).unwrap()
    }

    pub fn stream(&self) -> u64 {
        self.stream
    }
}

#[derive(
    Archive, Deserialize, Serialize, AsBytes, FromBytes, Clone, Copy, Debug, Eq, PartialEq,
)]
#[archive(as = "Head")]
#[repr(C)]
#[cfg_attr(feature = "serde-derive", derive(serde::Deserialize, serde::Serialize))]
pub struct Head {
    pub id: StreamId,
    pub hash: [u8; 32],
    pub len: u64,
}

impl Head {
    pub fn id(&self) -> &StreamId {
        &self.id
    }

    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }

    pub fn len(&self) -> u64 {
        self.len
    }
}

impl Head {
    pub(crate) fn new(id: StreamId) -> Self {
        Self {
            id,
            hash: [
                175, 19, 73, 185, 245, 249, 161, 166, 160, 64, 77, 234, 54, 220, 201, 73, 155, 203,
                37, 201, 173, 193, 18, 183, 204, 154, 147, 202, 228, 31, 50, 98,
            ],
            len: 0,
        }
    }
}

#[derive(
    Archive, Deserialize, Serialize, AsBytes, FromBytes, Clone, Copy, Debug, Eq, PartialEq,
)]
#[archive(as = "SignedHead")]
#[repr(C)]
#[cfg_attr(feature = "serde-derive", derive(serde::Deserialize, serde::Serialize))]
pub struct SignedHead {
    pub head: Head,
    #[cfg_attr(feature = "serde-derive", serde(with = "serde_big_array::BigArray"))]
    pub sig: [u8; 64],
}

impl Default for SignedHead {
    fn default() -> Self {
        Self::new(StreamId {
            peer: [0; 32],
            stream: 0,
        })
    }
}

impl SignedHead {
    pub fn head(&self) -> &Head {
        &self.head
    }

    pub fn sig(&self) -> &[u8; 64] {
        &self.sig
    }

    pub fn verify(&self, id: &StreamId) -> Result<()> {
        if id != self.head().id() {
            return Err(anyhow::anyhow!("missmatched stream id"));
        }
        let sig = Signature::from(self.sig);
        id.peer().verify_strict(self.head.as_bytes(), &sig)?;
        Ok(())
    }
}

impl SignedHead {
    pub(crate) fn new(id: StreamId) -> Self {
        Self {
            head: Head::new(id),
            sig: [0; 64],
        }
    }

    pub(crate) fn sign(&mut self, key: &Keypair) {
        debug_assert_eq!(key.public, self.head.id().peer());
        self.sig = key.sign(self.head.as_bytes()).to_bytes();
    }

    pub(crate) fn set_signature(&mut self, sig: [u8; 64]) -> Result<()> {
        let sig2 = Signature::from(sig);
        self.head
            .id()
            .peer()
            .verify_strict(self.head.as_bytes(), &sig2)?;
        self.sig = sig;
        Ok(())
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct Stream {
    pub(crate) head: SignedHead,
    pub(crate) outboard: Vec<u8>,
}

impl Stream {
    pub fn head(&self) -> &Head {
        self.head.head()
    }
}

impl Stream {
    pub(crate) fn new(id: StreamId) -> Self {
        Self {
            head: SignedHead::new(id),
            outboard: vec![0, 0, 0, 0, 0, 0, 0, 0],
        }
    }

    pub(crate) fn to_bytes(&self) -> Result<AlignedVec> {
        let mut ser = AllocSerializer::<4096>::default();
        ser.serialize_value(self).unwrap();
        Ok(ser.into_serializer().into_inner())
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug, Default, Eq, PartialEq)]
#[repr(C)]
#[cfg_attr(feature = "serde-derive", derive(serde::Deserialize, serde::Serialize))]
pub struct Slice {
    pub head: SignedHead,
    pub data: Vec<u8>,
}

impl Slice {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            head: Default::default(),
            data: Vec::with_capacity(capacity),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut ser = AllocSerializer::<4096>::default();
        ser.serialize_value(self).unwrap();
        ser.into_serializer().into_inner().into_vec()
    }
}

pub(crate) struct StreamLock {
    id: StreamId,
    locks: Arc<Mutex<FnvHashSet<StreamId>>>,
}

impl StreamLock {
    pub fn new(id: StreamId, locks: Arc<Mutex<FnvHashSet<StreamId>>>) -> Self {
        Self { id, locks }
    }
}

impl Drop for StreamLock {
    fn drop(&mut self) {
        let mut locks = self.locks.lock();
        debug_assert!(locks.remove(&self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_stream() {
        let (outboard, hash) = bao::encode::outboard(&[]);
        let id = StreamId::new([0; 32], 42);
        let expect = Stream {
            head: SignedHead {
                head: Head {
                    id,
                    hash: *hash.as_bytes(),
                    len: 0,
                },
                sig: [0; 64],
            },
            outboard,
        };
        let actual = Stream::new(id);
        assert_eq!(actual, expect);
    }
}
