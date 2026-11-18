use super::*;
#[test]
fn failed_system_proxy_rollback_keeps_both_causes_and_requires_shutdown() {
    let mut system = system_proxy::Guard::default();
    let consistency = std::sync::OnceLock::new();
    let error = failed_system_proxy_switch(
        anyhow::anyhow!("candidate rejected"),
        Ok(system_proxy::Guard::default()),
        &mut system,
        &consistency,
    );
    assert_eq!(error.to_string(), "candidate rejected");
    assert!(consistency.get().is_none());
    let error = failed_system_proxy_switch(
        anyhow::anyhow!("candidate rejected"),
        Err(anyhow::anyhow!("previous settings unavailable")),
        &mut system,
        &consistency,
    );
    let message = error.to_string();
    assert!(message.contains("candidate rejected"));
    assert!(message.contains("previous settings unavailable"));
    assert!(message.contains("outcome unknown"));
    assert_eq!(consistency.get(), Some(&message));
}
