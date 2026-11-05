use super::*;
#[test]
fn completion_failure_cancellation_and_gauge_are_distinct() {
    fn start() -> Operation {
        operation!("test.operation", "测试")
    }
    start().finish(true);
    start().finish(false);
    drop(start());
    let snapshot = snapshot("epoch");
    let value = |name: &str| {
        snapshot
            .metrics
            .iter()
            .find(|m| m.name == format!("test.operation.{name}"))
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(value("started"), "3");
    assert_eq!(value("completed"), "2");
    assert_eq!(value("failed"), "1");
    assert_eq!(value("cancelled"), "1");
    assert_eq!(value("in_flight"), "0");
}
#[test]
fn counters_keep_integer_precision_and_registration_is_shared() {
    let a = Handle::register("test.precision", "测试", "count", Kind::Counter);
    let b = Handle::register("test.precision", "测试", "count", Kind::Counter);
    a.add(9_007_199_254_740_993);
    assert_eq!(b.get(), 9_007_199_254_740_993);
    assert_eq!(
        snapshot("epoch")
            .metrics
            .iter()
            .find(|m| m.name == "test.precision")
            .unwrap()
            .value,
        "9007199254740993"
    );
}
