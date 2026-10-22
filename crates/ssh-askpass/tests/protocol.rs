use ssh_askpass::{MAX_PROMPT_BYTES, Secret, decode_request, encode_request, read_frame};
#[test]
fn request_roundtrip_preserves_host_fingerprint_and_confirmation() {
    let prompt = "The authenticity of host '[example.test]:2200' can't be established.\nED25519 key fingerprint is SHA256:fixture.\nAre you sure you want to continue connecting (yes/no/[fingerprint])?";
    let bytes = encode_request("0123456789abcdef", prompt, true).unwrap();
    assert_eq!(
        decode_request(&bytes).unwrap(),
        ("0123456789abcdef".into(), prompt.into(), true)
    );
}
#[test]
fn untrusted_ipc_lengths_and_secrets_are_bounded() {
    assert!(encode_request("token", &"x".repeat(MAX_PROMPT_BYTES + 1), false).is_err());
    assert!(read_frame(&mut &u32::MAX.to_be_bytes()[..], MAX_PROMPT_BYTES).is_err());
    assert!(Secret::new(b"one\ntwo".to_vec()).is_err());
    assert_eq!(
        format!("{:?}", Secret::new(b"sensitive".to_vec()).unwrap()),
        "Secret([redacted])"
    );
}
