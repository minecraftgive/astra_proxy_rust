//! Test peers speak the same framed v2 protocol as production frontends.
use astra_management::{
    client::{receive, send},
    v2::*,
};
use astra_platform::platform::{LocalListener, LocalStream};

pub(super) async fn next(listener: &LocalListener) -> (LocalStream, Request) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = receive(&mut stream).await.unwrap();
    (stream, request)
}
pub(super) async fn reply(mut stream: LocalStream, request: Request, body: ResponseBody) {
    send(&mut stream, &Response::success(request.id, body))
        .await
        .unwrap();
}
pub(super) async fn error(mut stream: LocalStream, request: Request, message: &str) {
    send(
        &mut stream,
        &Response::error(request.id, ErrorCode::Conflict, message),
    )
    .await
    .unwrap();
}
pub(super) fn draft(original: &str, text: &str, revision: &str) -> Draft {
    Draft {
        last_save: None,
        id: "test-draft".into(),
        instance: "proxy".into(),
        path: "proxy.toml".into(),
        original: original.into(),
        text: text.into(),
        revision: revision.into(),
        source_revision: "test-source-revision".into(),
        kind: InstanceKind::Proxy,
    }
}
pub(super) async fn save_request(
    listener: &LocalListener,
    original: &str,
    text: &str,
) -> (LocalStream, Request) {
    let (stream, request) = next(listener).await;
    assert!(matches!(
        request.body,
        RequestBody::Drafts(DraftCommand::Open { .. })
    ));
    reply(
        stream,
        request,
        ResponseBody::Draft(draft(original, original, "0")),
    )
    .await;
    let (stream, request) = next(listener).await;
    assert!(
        matches!(&request.body,RequestBody::Drafts(DraftCommand::Edit{text:actual,..}) if actual==text)
    );
    reply(
        stream,
        request,
        ResponseBody::Draft(draft(original, text, "1")),
    )
    .await;
    let (stream, request) = next(listener).await;
    assert!(matches!(
        request.body,
        RequestBody::Drafts(DraftCommand::Save { .. })
    ));
    (stream, request)
}
