#[tokio::test]
async fn version_is_checked_before_new_fields_are_decoded() {
    let (mut sender, mut receiver) = tokio::io::duplex(128);
    // An older reply lacks a field required by the new contract.
    super::send_management(&mut sender, &(1u16, 42u32))
        .await
        .unwrap();
    let error = super::receive_management_versioned::<(u16, u32, String)>(&mut receiver, 2)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("peer version 1, expected 2"));
    super::send_management(&mut sender, &(2u16, 42u32, "ready"))
        .await
        .unwrap();
    let reply: (u16, u32, String) = super::receive_management_versioned(&mut receiver, 2)
        .await
        .unwrap();
    assert_eq!(reply.2, "ready");
}
