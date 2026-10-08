use mm_fleet::sealed::{
    CredentialPlaintext, Keypair, SealError, aad, display_fingerprint, fingerprint_of, open, seal,
};

#[test]
fn round_trip_with_matching_aad() {
    let kp = Keypair::generate();
    let a = aad("p-1", "scaleway", &kp.fingerprint());
    let sealed = seal(&kp.public_bytes(), b"secret-token", &a).expect("seal");
    assert_eq!(sealed.enc.len(), 32, "X25519 encapsulated key is 32 bytes");
    assert_eq!(
        sealed.ct.len(),
        b"secret-token".len() + 16,
        "16-byte Poly1305 tag"
    );
    let pt = open(&kp, &sealed.enc, &sealed.ct, &a).expect("open");
    assert_eq!(pt, b"secret-token");
}

#[test]
fn a_blob_cannot_move_to_another_provider_row() {
    let kp = Keypair::generate();
    let sealed = seal(
        &kp.public_bytes(),
        b"t",
        &aad("p-1", "scaleway", &kp.fingerprint()),
    )
    .unwrap();
    let err = open(
        &kp,
        &sealed.enc,
        &sealed.ct,
        &aad("p-2", "scaleway", &kp.fingerprint()),
    )
    .unwrap_err();
    assert!(matches!(err, SealError::Open));
}

#[test]
fn another_key_cannot_open() {
    let kp = Keypair::generate();
    let other = Keypair::generate();
    let a = aad("p-1", "scaleway", &kp.fingerprint());
    let sealed = seal(&kp.public_bytes(), b"t", &a).unwrap();
    assert!(matches!(
        open(&other, &sealed.enc, &sealed.ct, &a).unwrap_err(),
        SealError::Open
    ));
}

#[test]
fn fingerprint_is_sixteen_hex_chars_and_displays_in_groups() {
    let kp = Keypair::generate();
    let fp = kp.fingerprint();
    assert_eq!(fp.len(), 16);
    assert!(
        fp.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(fingerprint_of(&kp.public_bytes()), fp);
    assert_eq!(
        display_fingerprint("ab12cd34ef567890"),
        "ab12 cd34 ef56 7890"
    );
}

#[test]
fn secret_bytes_round_trip_through_from_secret_bytes() {
    let kp = Keypair::generate();
    let again = Keypair::from_secret_bytes(&kp.secret_bytes()).expect("load");
    assert_eq!(again.public_bytes(), kp.public_bytes());
}

#[test]
fn error_messages_never_carry_bytes() {
    for e in [
        SealError::BadKey,
        SealError::BadEnc,
        SealError::Open,
        SealError::Seal,
    ] {
        let s = e.to_string();
        assert!(!s.contains('['), "{s}");
        assert!(s.len() < 80, "{s}");
    }
}

#[test]
fn credential_plaintext_json_shape_is_stable() {
    let p = CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "scaleway".into(),
        endpoint: "https://api.scaleway.com".into(),
        account: Some("proj".into()),
        fields: [("secret_key".to_string(), "SCW-x".to_string())]
            .into_iter()
            .collect(),
    };
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(
        json,
        r#"{"v":1,"provider_id":"p-1","kind":"scaleway","endpoint":"https://api.scaleway.com","account":"proj","fields":{"secret_key":"SCW-x"}}"#
    );
}

#[test]
fn credential_plaintext_debug_never_prints_the_token() {
    let p = CredentialPlaintext {
        v: 1,
        provider_id: "p-1".into(),
        kind: "scaleway".into(),
        endpoint: "https://api.scaleway.com".into(),
        account: Some("proj".into()),
        fields: [("secret_key".to_string(), "SCW-x".to_string())]
            .into_iter()
            .collect(),
    };
    for rendered in [format!("{p:?}"), format!("{p:#?}")] {
        assert!(!rendered.contains("SCW-x"), "token leaked: {rendered}");
        assert!(rendered.contains("secret_key"), "{rendered}");
        assert!(rendered.contains("p-1"), "{rendered}");
    }
}

#[test]
fn a_blob_sealed_by_the_browser_library_opens_in_rust() {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/browser_sealed.json")).unwrap();
    let ikm = hex::decode(v["ikm"].as_str().unwrap()).unwrap();
    let kp = Keypair::derive_for_tests(&ikm);
    assert_eq!(
        kp.fingerprint(),
        v["key_id"].as_str().unwrap(),
        "both sides derive the same public key from the IKM"
    );
    let enc = hex::decode(v["enc"].as_str().unwrap()).unwrap();
    let ct = hex::decode(v["ciphertext"].as_str().unwrap()).unwrap();
    let provider_id = v["provider_id"].as_str().unwrap();
    let kind = v["kind"].as_str().unwrap();
    let a = aad(provider_id, kind, &kp.fingerprint());
    let pt = open(&kp, &enc, &ct, &a).expect("browser-sealed blob opens in Rust");
    let parsed: CredentialPlaintext = serde_json::from_slice(&pt).unwrap();
    let expected: CredentialPlaintext = serde_json::from_value(v["plaintext"].clone()).unwrap();
    assert_eq!(
        parsed, expected,
        "the opened plaintext is the fixture's plaintext"
    );
    assert_eq!(parsed.provider_id, provider_id);
    assert_eq!(parsed.kind, kind);
    assert_eq!(parsed.fields["secret_key"], "SCW-FIXTURE-NOT-A-REAL-KEY");
    assert_eq!(
        pt,
        serde_json::to_vec(&parsed).unwrap(),
        "the browser serialises in Rust's struct field order, byte for byte"
    );
}
