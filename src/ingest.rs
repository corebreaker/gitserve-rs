//! Ingestion of the packs that a push sends.

use std::io::BufRead;

use git_storage_trait::GitOps;
use gix_hash::{Kind, ObjectId};
use gunnar_wire::receive_pack::{PackIngest, PackReceipt};
use gunnar_wire::{Error, IndexedPack, PackVersion, Result};

/// The length of a pack header: the `PACK` signature, the version and the number of objects.
const HEADER_LEN: usize = 12;

/// Takes the pack of a push into a store, which completes a thin pack with the bases it already holds.
pub struct StoreIngest<'a, S: GitOps> {
    store: &'a S,
    hash_kind: Kind,
}

impl<'a, S: GitOps> StoreIngest<'a, S> {
    /// Creates the ingestion into `store`, whose objects are named with `hash_kind`.
    pub fn new(store: &'a S, hash_kind: Kind) -> Self {
        Self { store, hash_kind }
    }
}

impl<S: GitOps> PackIngest for StoreIngest<'_, S> {
    fn ingest(&self, pack: &mut dyn BufRead) -> Result<PackReceipt> {
        let mut bytes = Vec::new();
        pack.read_to_end(&mut bytes)?;

        let header = parse_header(&bytes, self.hash_kind)?;
        self.store
            .put_pack(&bytes)
            .map_err(|err| Error::Pack(format!("cannot store the pack: {err:#}")))?;

        // The store writes the index itself, and tells neither its checksum nor the paths: they stay null and
        // unset. receive-pack only logs the object count and the pack id, and hands the rest back in its outcome,
        // which gitserve does not read; without an object set, the receipt weighs on no gate either.
        Ok(PackReceipt::without_object_set(IndexedPack {
            num_objects: header.num_objects,
            pack_id: header.pack_id,
            index_id: ObjectId::null(self.hash_kind),
            pack_version: header.version,
            hash_kind: self.hash_kind,
            index_path: None,
            data_path: None,
            keep_path: None,
            stored_bytes: Some(bytes.len() as u64),
        }))
    }
}

/// What the header and the trailer of a pack tell.
#[derive(Debug, PartialEq, Eq)]
struct PackHeader {
    version: PackVersion,
    num_objects: u32,
    /// The trailing checksum of the pack, which identifies it.
    pack_id: ObjectId,
}

/// Reads the header and the trailing checksum of the pack in `bytes`. The store checks the rest.
fn parse_header(bytes: &[u8], hash_kind: Kind) -> Result<PackHeader> {
    let trailer_len = hash_kind.len_in_bytes();
    if bytes.len() < HEADER_LEN + trailer_len || !bytes.starts_with(b"PACK") {
        return Err(Error::Pack("the push sent no valid pack".into()));
    }

    let word = |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let pack_id = ObjectId::try_from(&bytes[bytes.len() - trailer_len..])
        .map_err(|err| Error::Pack(format!("invalid pack checksum: {err}")))?;

    Ok(PackHeader {
        version: PackVersion::from_u32(word(4)),
        num_objects: word(8),
        pack_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(objects: u32, trailer: &[u8]) -> Vec<u8> {
        let mut bytes = b"PACK".to_vec();
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&objects.to_be_bytes());
        bytes.extend_from_slice(trailer);
        bytes
    }

    #[test]
    fn parse_header_reads_the_header_and_the_checksum() {
        let checksum = [7u8; 20];
        let header = parse_header(&pack(3, &checksum), Kind::Sha1).unwrap();

        assert_eq!(header.version, PackVersion::V2);
        assert_eq!(header.num_objects, 3);
        assert_eq!(header.pack_id, ObjectId::try_from(&checksum[..]).unwrap());
    }

    #[test]
    fn parse_header_takes_the_checksum_length_from_the_hash_kind() {
        let checksum = [9u8; 32];
        let header = parse_header(&pack(0, &checksum), Kind::Sha256).unwrap();

        assert_eq!(header.pack_id, ObjectId::try_from(&checksum[..]).unwrap());
    }

    #[test]
    fn parse_header_rejects_what_is_not_a_pack() {
        assert!(parse_header(b"", Kind::Sha1).is_err());
        assert!(parse_header(&pack(1, &[0; 10]), Kind::Sha1).is_err());

        let mut not_a_pack = pack(1, &[0; 20]);
        not_a_pack[..4].copy_from_slice(b"KCAP");
        assert!(parse_header(&not_a_pack, Kind::Sha1).is_err());
    }
}
