//! The pack's own contract (ADR-0067 parts 4, 8–11), against the local
//! backend — the same `object_store` code path production S3 takes, staged
//! and renamed on complete, so the atomicity assertions here are about the
//! real publish mechanism and not a test double.

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_storage::StorageError;
use scarab_storage_s3::pack::{PackMember, PackMemberKind, PACK_TRAILER_BYTES};
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-pack-{}-{}", std::process::id(), n))
}

fn tagged(data: &[u8]) -> String {
    format!("sha256:{}", scarab_storage::sha256_hex(data))
}

/// The whole round trip: append members, finish, then read every member back
/// by ranged read at the offsets the footer recorded — and read the footer
/// itself back off the bucket alone (part 11: the bucket self-describes; no
/// database was consulted anywhere in this test).
///
/// Mutations killed: an offset computed after the buffer extend (every member
/// after the first ranges into the wrong bytes); a footer serialised before
/// the last member (the index under-reports); `bytes` excluding the
/// footer/trailer (a size-based reader ranges past the end).
#[tokio::test]
async fn members_range_read_back_exactly_and_the_footer_is_the_index() {
    let dir = temp_dir();
    let store = S3Storage::local(&dir).expect("local store");

    let a = b"first member".to_vec();
    let b = vec![7u8; 4096];
    let c = br#"[{"name":"x"}]"#.to_vec();

    let mut w = store.open_pack("packs/f1/000001.pack").await.expect("open");
    w.append(PackMemberKind::Blob, tagged(&a), &a).await.expect("append a");
    w.append(PackMemberKind::Blob, tagged(&b), &b).await.expect("append b");
    w.append(PackMemberKind::Tree, tagged(&c), &c).await.expect("append c");
    let finished = w.finish().await.expect("finish");

    assert_eq!(finished.key, "packs/f1/000001.pack");
    assert_eq!(finished.members.len(), 3);
    let body: u64 = (a.len() + b.len() + c.len()) as u64;
    assert!(
        finished.bytes > body + PACK_TRAILER_BYTES,
        "total must include the footer and trailer, not just member bytes"
    );

    // Every member ranges back to exactly its bytes.
    for (member, data) in finished.members.iter().zip([&a, &b, &c]) {
        let got = store
            .get_range(&finished.key, member.offset, member.len)
            .await
            .expect("ranged read");
        assert_eq!(got, **data, "member {} bytes", member.address);
        assert_eq!(member.len, data.len() as u64);
    }
    assert_eq!(finished.members[1].offset, a.len() as u64);
    assert_eq!(finished.members[1].kind, PackMemberKind::Blob);
    assert_eq!(finished.members[2].kind, PackMemberKind::Tree);

    // The bucket alone rebuilds the index: footer == the finished members.
    let index: Vec<PackMember> = store.pack_index(&finished.key).await.expect("pack index");
    assert_eq!(index, finished.members);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A pack that was never finished does not exist — the property the whole
/// commit-point ordering rests on (ADR-0067 part 10: bytes before pointers is
/// only safe because unfinished bytes are invisible, not partial).
#[tokio::test]
async fn an_unfinished_pack_publishes_nothing_at_its_key() {
    use scarab_storage::ObjectStore;

    let dir = temp_dir();
    let store = S3Storage::local(&dir).expect("local store");

    let data = vec![1u8; 1024];
    let mut w = store.open_pack("packs/f2/000001.pack").await.expect("open");
    w.append(PackMemberKind::Blob, tagged(&data), &data)
        .await
        .expect("append");
    // Dropped without finish(): the multipart upload is abandoned.
    drop(w);

    assert!(
        matches!(
            store.get("packs/f2/000001.pack").await,
            Err(StorageError::NotFound)
        ),
        "an abandoned multipart upload must leave NO object at the key"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A member larger than one part streams through the part buffer rather than
/// growing it — asserted at the observable grain: the bytes still range back
/// exactly, across the several parts they were shipped in.
#[tokio::test]
async fn a_member_larger_than_one_part_survives_the_part_boundary() {
    let dir = temp_dir();
    let store = S3Storage::local(&dir).expect("local store");

    // 9 MiB of non-repeating bytes: crosses the 8 MiB part boundary, and any
    // reordering/duplication of part payloads changes some byte.
    let mut big = vec![0u8; 9 * 1024 * 1024];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in big.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    let small = b"after the big one".to_vec();

    let mut w = store.open_pack("packs/f3/000001.pack").await.expect("open");
    w.append(PackMemberKind::Blob, tagged(&big), &big).await.expect("big");
    w.append(PackMemberKind::Blob, tagged(&small), &small)
        .await
        .expect("small");
    let finished = w.finish().await.expect("finish");

    let got_big = store
        .get_range(&finished.key, finished.members[0].offset, finished.members[0].len)
        .await
        .expect("range big");
    assert_eq!(got_big, big, "the multi-part member must reassemble byte-exactly");
    let got_small = store
        .get_range(&finished.key, finished.members[1].offset, finished.members[1].len)
        .await
        .expect("range small");
    assert_eq!(got_small, small);

    let _ = std::fs::remove_dir_all(&dir);
}
