use super::{selection, DetailedRequest, DiagnosticContext, DiagnosticPlan, Egress};
use anyhow::{Context, Result};
use astra_model::control::Identity;

fn unchanged(before: &Identity, after: &Identity) -> bool {
    before.boot_id == after.boot_id
        && before.process == after.process
        && before.applied == after.applied
}

impl DiagnosticPlan {
    pub(super) async fn path_context(
        &self,
        context: &DiagnosticContext,
    ) -> astra_model::network_path::PathContext {
        let mut observed = astra_model::network_path::PathContext {
            sampled_at_ms: context.sampled_at_ms.clone(),
            core_tun: None,
            physical: None,
        };
        if let (Some((socket, _)), Some(identity)) = (&self.control, &context.running_core) {
            let current = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let runtime =
                    astra_ipc::control::runtime(socket, astra_model::runtime::Command::Snapshot)
                        .await?;
                let after = astra_ipc::control::identity(socket).await?;
                anyhow::ensure!(
                    unchanged(identity, &after) && runtime.pid == after.process.pid,
                    "Core changed while observing TUN state"
                );
                Ok::<_, anyhow::Error>(runtime.tun)
            })
            .await;
            if let Ok(Ok(tun)) = current {
                observed.core_tun = Some(tun);
            }
        }
        observed
    }
    pub(super) async fn prepare_pinned(
        &mut self,
        request: &mut DetailedRequest,
        context: &mut DiagnosticContext,
    ) -> Result<()> {
        let Egress::Outbound(tag) = &request.egress else {
            return Ok(());
        };
        let pinned = if let Some((socket, pid)) = self.control.clone() {
            let before = astra_ipc::control::identity(&socket)
                .await
                .context("Read pinning core identity")?;
            anyhow::ensure!(
                Some(before.process.pid) == pid,
                "Core process changed; choose a confirmed concrete node"
            );
            self.refresh().await?;
            let runtime =
                astra_ipc::control::runtime(&socket, astra_model::runtime::Command::Snapshot)
                    .await
                    .context("Read pinning runtime snapshot")?;
            let after = astra_ipc::control::identity(&socket)
                .await
                .context("Verify pinning core identity")?;
            let digest = astra_ipc::control::config_digest(&self.text);
            anyhow::ensure!(unchanged(&before, &after) && runtime.pid == after.process.pid
                && after.applied.as_ref().is_some_and(|applied| applied.digest == digest && applied.path == self.config_path),
                "Core/configuration changed during pinning; choose a confirmed concrete node and retry");
            let pinned = selection::rebuild(&self.text, tag, Some(&runtime))?;
            context.running_core = Some(after);
            context.config_digest = Some(digest.to_string());
            context.config_observation = "matched_running".into();
            pinned
        } else {
            selection::rebuild(&self.text, tag, None)?
        };
        context.actual_egress = Egress::Outbound(pinned.leaf_tag.clone());
        context.egress_source = "pinned_request".into();
        context.config_used = true;
        context.selection_chain = pinned.selection_chain.clone();
        context.effective_interface = pinned.effective_interface.clone();
        context.interface_source = pinned.interface_source.clone();
        request.pinned = Some(pinned);
        Ok(())
    }

    pub(super) async fn verify_pinned(
        &self,
        request: &DetailedRequest,
        context: &DiagnosticContext,
    ) -> Result<()> {
        let Some(pinned) = &request.pinned else {
            return Ok(());
        };
        if let Some((socket, pid)) = &self.control {
            let captured = context
                .running_core
                .as_ref()
                .context("Pinned running-core identity is missing")?;
            let before = astra_ipc::control::identity(socket)
                .await
                .context("Recheck pinning core identity")?;
            anyhow::ensure!(
                Some(before.process.pid) == *pid && unchanged(captured, &before),
                "Core reloaded or restarted after pinning; choose a concrete node and retry"
            );
            let runtime =
                astra_ipc::control::runtime(socket, astra_model::runtime::Command::Snapshot)
                    .await
                    .context("Recheck selected path")?;
            let after = astra_ipc::control::identity(socket)
                .await
                .context("Recheck pinning configuration")?;
            anyhow::ensure!(
                runtime.pid == after.process.pid && unchanged(&before, &after),
                "Core changed during pinning revalidation; retry with a concrete node"
            );
            let current = selection::rebuild(&self.text, &pinned.requested_tag, Some(&runtime))?;
            anyhow::ensure!(
                selection::same_path(pinned, &current),
                "Selected path switched after sampling; choose a concrete node and retry"
            );
        }
        Ok(())
    }

    pub(super) async fn sample_resolvers(
        &self,
        request: &mut DetailedRequest,
        context: &mut DiagnosticContext,
    ) {
        request.interface_resolvers.clear();
        if request.resolver.is_some() {
            return;
        }
        let Some(interface) = context.effective_interface.as_ref() else {
            return;
        };
        let sampler = astra_platform::network_snapshot::Sampler::default();
        let snapshot = sampler.sample("diagnostic-context").await;
        request.interface_resolvers = resolver_pool(snapshot, interface);
    }
}

fn resolver_pool(
    snapshot: astra_model::network_state::Snapshot,
    interface: &str,
) -> Vec<astra_model::network_state::Resolver> {
    // Truncation may hide a more-specific scoped resolver. Never guess.
    if snapshot.truncated
        || snapshot.issues.iter().any(|issue| {
            issue.component.starts_with("dns")
                || issue.component.to_ascii_lowercase().contains("nrpt")
        })
        || snapshot.dns.iter().any(|resolver| {
            resolver.interface_id.is_none()
                && (matches!(resolver.scope.as_str(), "policy" | "scoped" | "service")
                    || !resolver.domains.is_empty())
        })
    {
        return vec![];
    }
    let interfaces: Vec<_> = snapshot
        .interfaces
        .iter()
        .filter(|candidate| {
            candidate.id == interface
                || candidate.name == interface
                || candidate.index.to_string() == interface
        })
        .collect();
    let [selected] = interfaces.as_slice() else {
        return vec![];
    };
    if !selected.connected {
        return vec![];
    }
    snapshot
        .dns
        .into_iter()
        .filter(|resolver| {
            resolver.interface_id.as_ref().is_some_and(|id| {
                id == &selected.id || id == &selected.name || id == &selected.index.to_string()
            })
        })
        .map(|mut resolver| {
            // Normalize only after matching the target Agent's concrete adapter.
            resolver.interface_id = Some(interface.to_owned());
            resolver
        })
        .collect()
}

#[cfg(test)]
#[path = "../../test/network_pin_state_test.rs"]
mod tests;
