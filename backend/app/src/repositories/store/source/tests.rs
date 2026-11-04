use crate::repositories::store::source::advertisement;
use crate::repositories::store::source::append_packet;
use crate::repositories::store::source::bytes;
use crate::repositories::store::source::packet;
use crate::repositories::store::source::unpack;

#[test]
fn git_packets_refuse_truncation_and_control_lengths() {
    for bytes in [
        b"".as_slice(),
        b"zzzz",
        b"0001",
        b"0003",
        b"0009abc",
        b"ffff",
    ] {
        let mut input = bytes;
        assert!(packet(&mut input).is_err(), "{bytes:?}");
    }
    assert!(packet(&mut &b"0000"[..]).unwrap().is_none());
}

#[test]
fn advertisements_preserve_refs_and_refuse_unsafe_names() {
    let prefix = b"001e# service=git-upload-pack\n0000";
    assert_eq!(
        advertisement(&[prefix.as_slice(), b"0000"].concat())
            .unwrap()
            .0
            .len(),
        0
    );
    let sha = "a".repeat(40);
    for (name, valid) in [
        ("refs/heads/main", true),
        ("refs/tags/v1", true),
        ("refs/heads/../../HEAD", false),
        ("-invalid", false),
    ] {
        let mut bytes = prefix.to_vec();
        append_packet(
            &mut bytes,
            &format!("{sha} {name}\0side-band-64k symref=HEAD:refs/heads/main\n"),
        );
        bytes.extend_from_slice(b"0000");
        assert_eq!(advertisement(&bytes).is_ok(), valid, "{name}");
    }
    let mut with_private_refs = prefix.to_vec();
    append_packet(
        &mut with_private_refs,
        &format!("{sha} refs/heads/main\0side-band-64k\n"),
    );
    for name in ["refs/codeyard", "refs/codeyard/publications/foreign"] {
        append_packet(&mut with_private_refs, &format!("{sha} {name}\n"));
    }
    with_private_refs.extend_from_slice(b"0000");
    let (visible, _) = advertisement(&with_private_refs).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible["refs/heads/main"], sha);

    let mut duplicate = prefix.to_vec();
    append_packet(
        &mut duplicate,
        &format!("{sha} refs/heads/main\0side-band-64k\n"),
    );
    append_packet(&mut duplicate, &format!("{sha} refs/heads/main\n"));
    duplicate.extend_from_slice(b"0000");
    assert!(advertisement(&duplicate).is_err());
}

#[test]
fn transfers_ignore_progress_but_not_remote_errors() {
    let mut bytes = b"0008NAK\n".to_vec();
    let mut data = vec![1];
    data.extend_from_slice(b"PACKcontents");
    bytes.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    bytes.extend_from_slice(&data);
    bytes.extend_from_slice(b"0009\x02info0000");
    assert_eq!(unpack(bytes.clone()).unwrap(), b"PACKcontents");
    bytes.extend_from_slice(b"0009\x03fail");
    assert!(unpack(bytes).is_err());
    assert!(unpack(b"0008NAK\n0000".to_vec()).is_err());
    assert!(unpack(b"0008ACK\n0000".to_vec()).is_err());
}
