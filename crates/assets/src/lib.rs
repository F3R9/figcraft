//! Image payloads are independent of document operations. References are small, immutable and
//! checked by content, and the native cache keeps their encoded files out of undo snapshots.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub mod cache;
pub mod preview;

/// The same per-image ceiling as native archive members, independent of document size.
pub const MAX_ASSET_BYTES: u64 = 512 << 20;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetRef {
    /// Local cache isolation. The guest derives it from the server and file, never trusts a
    /// scope supplied by a remote peer, and strips it from requests/uploads on the wire.
    pub scope: String,
    pub sha256: String,
    pub len: u64,
}

impl AssetRef {
    pub fn of_arc(bytes: &Arc<Vec<u8>>) -> Self {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock, PoisonError, Weak};
        type Memo = HashMap<usize, (Weak<Vec<u8>>, AssetRef)>;
        static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
        let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
        let key = Arc::as_ptr(bytes) as usize;
        {
            let held = memo.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((old, reference)) = held.get(&key)
                && Weak::ptr_eq(old, &Arc::downgrade(bytes))
            {
                return reference.clone();
            }
        }
        let reference = Self::of(bytes);
        let mut held = memo.lock().unwrap_or_else(PoisonError::into_inner);
        if held.len() >= 20_000 {
            held.retain(|_, (data, _)| data.strong_count() > 0);
        }
        if held.len() < 20_000 {
            held.insert(key, (Arc::downgrade(bytes), reference.clone()));
        }
        reference
    }

    pub fn of(bytes: &[u8]) -> Self {
        Self { scope: String::new(), sha256: format!("{:x}", Sha256::digest(bytes)), len: bytes.len() as u64 }
    }

    pub fn valid(&self) -> bool {
        (self.scope.is_empty() || self.scope.len() == 64 && self.scope.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
            && self.len <= MAX_ASSET_BYTES
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }

    pub fn verify(&self, bytes: &[u8]) -> bool {
        self.valid() && self.len == bytes.len() as u64 && Self::of(bytes).same_content(self)
    }

    pub fn same_content(&self, other: &Self) -> bool {
        self.sha256 == other.sha256 && self.len == other.len
    }

    pub fn cache_key(&self) -> String {
        if self.scope.is_empty() { self.sha256.clone() } else { Self::of(format!("{}:{}", self.scope, self.sha256).as_bytes()).sha256 }
    }
}

/// Serializes as raw binary bytes with postcard, without base64 or per-byte integer encoding.
/// Cloning an operation only clones this pointer, never its image payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob(pub Arc<Vec<u8>>);

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Bytes;
        impl<'de> serde::de::Visitor<'de> for Bytes {
            type Value = Blob;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bounded image bytes")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Blob, E> {
                if v.len() as u64 > MAX_ASSET_BYTES {
                    return Err(E::custom("image exceeds the asset limit"));
                }
                Ok(Blob(Arc::new(v.to_vec())))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Blob, E> {
                if v.len() as u64 > MAX_ASSET_BYTES {
                    return Err(E::custom("image exceeds the asset limit"));
                }
                Ok(Blob(Arc::new(v)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Blob, A::Error> {
                use serde::de::Error;
                let mut bytes = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    if bytes.len() as u64 >= MAX_ASSET_BYTES {
                        return Err(A::Error::custom("image exceeds the asset limit"));
                    }
                    bytes.push(byte);
                }
                Ok(Blob(Arc::new(bytes)))
            }
        }
        d.deserialize_byte_buf(Bytes)
    }
}

pub fn resolve(bytes: &Arc<Vec<u8>>, asset: Option<&AssetRef>) -> Option<Arc<Vec<u8>>> {
    if !bytes.is_empty() {
        return Some(bytes.clone());
    }
    let asset = asset?;
    if let Ok(bytes) = cache::load(asset) {
        return Some(bytes);
    }
    cache::request(asset);
    None
}

pub fn require(bytes: &Arc<Vec<u8>>, asset: Option<&AssetRef>) -> Result<Arc<Vec<u8>>, String> {
    match asset {
        _ if !bytes.is_empty() => Ok(bytes.clone()),
        Some(asset) => cache::require(asset),
        None => Ok(bytes.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_assets_round_trip_without_base64_expansion() {
        let data = Blob(Arc::new(vec![255; 100_000]));
        let wire = postcard::to_allocvec(&data).unwrap();
        assert!(wire.len() < data.0.len() + 16);
        assert_eq!(postcard::from_bytes::<Blob>(&wire).unwrap(), data);
        let reference = AssetRef::of(&data.0);
        assert!(reference.verify(&data.0));
        assert!(!reference.verify(b"corrupt"));
        assert!(!AssetRef { scope: String::new(), sha256: "../escape".into(), len: 9 }.valid());
    }
}
