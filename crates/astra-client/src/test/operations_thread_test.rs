use super::*;

#[test]
fn diagnostics_result_applies_without_entering_a_tokio_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut client = Client::new(Kind::Proxy);
    let action = Action::Diagnostics("proxy".into());
    let input = OperationInput::capture(&client);
    let result = runtime.block_on(perform(
        std::path::Path::new("/astra-test-missing-manager"),
        input,
        action.clone(),
        None,
    ));
    // This is the GPUI-side boundary: application must only install the model,
    // never spawn Tokio work on a thread without an entered Tokio executor.
    assert!(tokio::runtime::Handle::try_current().is_err());
    client.apply_operation(
        result,
        &action,
        ApplyContext {
            same: true,
            log_target_current: true,
            background: true,
        },
    );
    assert_eq!(client.diagnostics.as_ref().unwrap().name, "proxy");
}

#[test]
fn stale_diagnostics_result_does_not_open_a_view() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut client = Client::new(Kind::Proxy);
    let action = Action::Diagnostics("proxy".into());
    let result = runtime.block_on(perform(
        std::path::Path::new("/astra-test-missing-manager"),
        OperationInput::capture(&client),
        action.clone(),
        None,
    ));
    client.apply_operation(
        result,
        &action,
        ApplyContext {
            same: false,
            log_target_current: true,
            background: true,
        },
    );
    assert!(client.diagnostics.is_none());
}
