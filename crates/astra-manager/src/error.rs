//! Internal error classification. Display text is never part of the protocol contract.
use astra_management::v2::{ApiError, ErrorCode, Response};

#[derive(Debug)]
pub(crate) struct AgentError {
    pub code: ErrorCode,
    pub source: anyhow::Error,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, f)
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(crate) fn classified(code: ErrorCode, source: anyhow::Error) -> anyhow::Error {
    AgentError { code, source }.into()
}

pub(crate) fn message(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    classified(code, anyhow::Error::msg(message.into()))
}

pub(crate) fn code(error: &anyhow::Error) -> ErrorCode {
    // Once an effect is uncertain, an outer policy or storage context cannot
    // turn it into a definitive rejection that invites another execution.
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<AgentError>()
            .is_some_and(|error| matches!(error.code, ErrorCode::OutcomeUnknown))
            || cause
                .downcast_ref::<crate::save::SaveFailure>()
                .is_some_and(|failure| matches!(failure.code(), ErrorCode::OutcomeUnknown))
    }) {
        return ErrorCode::OutcomeUnknown;
    }
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AgentError>())
        .map(|error| error.code.clone())
        .unwrap_or(ErrorCode::Internal)
}

pub(crate) fn api_error(error: &anyhow::Error) -> ApiError {
    ApiError {
        code: code(error),
        message: format!("{error:#}"),
        details: error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::save::SaveFailure>())
            .map(|failure| failure.outcome.clone()),
    }
}

pub(crate) fn response(id: String, error: anyhow::Error) -> Response {
    Response {
        version: astra_management::v2::VERSION,
        id,
        result: Err(api_error(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_survives_context_and_does_not_read_messages() {
        for text in [
            "different wording",
            "冲突",
            "Unsupported: not found; outcome unknown",
        ] {
            let error = message(ErrorCode::Conflict, text).context("operation context");
            assert!(matches!(code(&error), ErrorCode::Conflict));
        }
        for text in [
            "Conflict: busy",
            "Unsupported: feature",
            "not found",
            "outcome unknown; not replayed",
        ] {
            assert!(matches!(code(&anyhow::anyhow!(text)), ErrorCode::Internal));
        }
        let error =
            classified(ErrorCode::NotFound, anyhow::anyhow!("absent")).context("read resource");
        assert!(matches!(api_error(&error).code, ErrorCode::NotFound));
        let error = classified(
            ErrorCode::RequiresLocalAction,
            message(ErrorCode::OutcomeUnknown, "native receipt was lost"),
        );
        assert!(matches!(code(&error), ErrorCode::OutcomeUnknown));
    }

    #[test]
    fn save_uncertainty_and_details_survive_wrapping() {
        let failure = crate::save::failed(
            astra_management::v2::SaveState::OutcomeUnknown,
            Some(true),
            None,
            "保存结果待确认",
        );
        let error = classified(ErrorCode::Internal, failure).context("save operation");
        let response = api_error(&error);
        assert!(matches!(response.code, ErrorCode::OutcomeUnknown));
        let details = response.details.unwrap();
        assert!(matches!(
            details.state,
            astra_management::v2::SaveState::OutcomeUnknown
        ));
        assert_eq!(details.disk_saved, Some(true));
        assert_eq!(details.runtime_applied, None);
    }
}
