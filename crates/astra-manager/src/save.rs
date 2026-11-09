use astra_management::v2::{ErrorCode, SaveOutcome, SaveState};
#[derive(Debug)]
pub(crate) struct SaveFailure {
    pub outcome: SaveOutcome,
    pub source: Option<anyhow::Error>,
}
impl SaveFailure {
    pub(crate) fn code(&self) -> ErrorCode {
        match self.outcome.state {
            SaveState::OutcomeUnknown => ErrorCode::OutcomeUnknown,
            _ => ErrorCode::Internal,
        }
    }
}
impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.outcome.message)
    }
}
impl std::error::Error for SaveFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| error.as_ref())
    }
}

pub(crate) fn for_operation(error: anyhow::Error, operation_id: &str) -> anyhow::Error {
    let code = crate::error::code(&error);
    let unknown = matches!(code, astra_management::v2::ErrorCode::OutcomeUnknown);
    let mut outcome = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SaveFailure>())
        .map(|failure| failure.outcome.clone())
        .unwrap_or_else(|| {
            outcome(
                if unknown {
                    SaveState::OutcomeUnknown
                } else {
                    SaveState::Rejected
                },
                if unknown { None } else { Some(false) },
                if unknown { None } else { Some(false) },
                format!("{error:#}"),
            )
        });
    outcome.operation_id = operation_id.into();
    crate::error::classified(
        code,
        SaveFailure {
            outcome,
            source: Some(error),
        }
        .into(),
    )
}
pub(crate) fn outcome(
    state: SaveState,
    disk_saved: Option<bool>,
    runtime_applied: Option<bool>,
    message: impl Into<String>,
) -> SaveOutcome {
    SaveOutcome {
        state,
        disk_saved,
        runtime_applied,
        recovery: None,
        operation_id: String::new(),
        message: message.into(),
    }
}
pub(crate) fn failed(
    state: SaveState,
    disk_saved: Option<bool>,
    runtime_applied: Option<bool>,
    message: impl Into<String>,
) -> anyhow::Error {
    SaveFailure {
        outcome: outcome(state, disk_saved, runtime_applied, message),
        source: None,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_management::v2::ErrorCode;

    #[test]
    fn lost_save_receipt_does_not_claim_the_write_was_rejected() {
        let source = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "receipt closed",
        ));
        let error = for_operation(
            crate::error::classified(ErrorCode::OutcomeUnknown, source),
            "save-once",
        );
        let api = crate::error::api_error(&error);
        assert!(matches!(api.code, ErrorCode::OutcomeUnknown));
        let outcome = api.details.unwrap();
        assert!(matches!(outcome.state, SaveState::OutcomeUnknown));
        assert_eq!(outcome.disk_saved, None);
        assert_eq!(outcome.runtime_applied, None);
        assert_eq!(outcome.operation_id, "save-once");
        assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));
    }

    #[test]
    fn operation_annotation_preserves_nested_save_evidence() {
        let error = failed(
            SaveState::OutcomeUnknown,
            Some(true),
            None,
            "runtime confirmation missing",
        );
        let error = crate::error::classified(ErrorCode::Internal, error).context("native save");
        let api = crate::error::api_error(&for_operation(error, "save-once"));
        assert!(matches!(api.code, ErrorCode::OutcomeUnknown));
        let outcome = api.details.unwrap();
        assert!(matches!(outcome.state, SaveState::OutcomeUnknown));
        assert_eq!(outcome.disk_saved, Some(true));
        assert_eq!(outcome.runtime_applied, None);
        assert_eq!(outcome.message, "runtime confirmation missing");
        assert_eq!(outcome.operation_id, "save-once");
    }
}
