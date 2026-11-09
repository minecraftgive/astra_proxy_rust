//! Manual in-core group tests keep the diagnostic gate until the same core is idle.
use super::*;
use astra_model::{control::Identity, runtime};

fn same_core(before: &Identity, after: &Identity) -> bool {
    before.boot_id == after.boot_id
        && before.process == after.process
        && before.applied == after.applied
}
impl Hub {
    pub(super) async fn manual_group_test(
        self: &Arc<Self>,
        name: String,
        group: String,
        expected_pid: Option<u32>,
    ) -> Result<Reply> {
        let hub = self.clone();
        self.spawn_legacy(async move {
            let route = hub.route(&name)?;
            anyhow::ensure!(
                route.instance.kind == Kind::Proxy && route.pid.is_some(),
                "Select a running proxy"
            );
            anyhow::ensure!(
                expected_pid.is_none() || expected_pid == route.pid,
                "Core PID changed before manual group test"
            );
            let identity = timeout(Duration::from_secs(3), control::identity(&route.socket))
                .await
                .context("Read group-test core identity timed out")??;
            anyhow::ensure!(
                Some(identity.process.pid) == route.pid,
                "Core identity differs before manual group test"
            );
            let baseline = timeout(
                Duration::from_secs(3),
                control::runtime(&route.socket, runtime::Command::Snapshot),
            )
            .await
            .context("Read group-test baseline timed out")??;
            anyhow::ensure!(
                baseline.pid == identity.process.pid,
                "Core changed before manual group test"
            );
            let target = baseline
                .groups
                .iter()
                .find(|value| value.name == group)
                .context("Unknown proxy group")?;
            anyhow::ensure!(!target.testing, "A group test is already running");
            let confirmed = timeout(Duration::from_secs(3), control::identity(&route.socket))
                .await
                .context("Confirm group-test identity timed out")??;
            anyhow::ensure!(
                same_core(&identity, &confirmed),
                "Core changed before manual group test"
            );
            let owned = async {
                let initial = timeout(
                    Duration::from_secs(3),
                    control::runtime(&route.socket, runtime::Command::TestGroup(group.clone())),
                )
                .await
                .context("Group-test acceptance timed out; outcome unconfirmed")??;
                anyhow::ensure!(
                    Some(initial.pid) == route.pid,
                    "Core changed while accepting group test"
                );
                timeout(Duration::from_secs(70), async {
                    loop {
                        anyhow::ensure!(
                            hub.route(&name)?.pid == route.pid,
                            "Core route changed during group test"
                        );
                        let before = control::identity(&route.socket).await?;
                        let snapshot =
                            control::runtime(&route.socket, runtime::Command::Snapshot).await?;
                        let after = control::identity(&route.socket).await?;
                        anyhow::ensure!(
                            same_core(&identity, &before)
                                && same_core(&before, &after)
                                && snapshot.pid == identity.process.pid,
                            "Core identity/configuration changed during group test"
                        );
                        let current = snapshot
                            .groups
                            .iter()
                            .find(|value| value.name == group)
                            .context("Group disappeared before idle state could be confirmed")?;
                        if !current.testing {
                            return Ok::<_, anyhow::Error>(Reply::text(serde_json::to_string(
                                &snapshot,
                            )?));
                        }
                        sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .context("Group test did not reach confirmed idle state before deadline")?
            }
            .await;
            // This is a user-traffic core. Never kill it to cancel a diagnostic;
            // retain an explicit unresolved-cleanup gate instead of allowing NAT.
            owned.map_err(|error| {
                crate::network::CleanupUnconfirmed(format!(
                    "Manual group test cleanup unconfirmed: {error:#}"
                ))
                .into()
            })
        })
        .await
        .context("Manual group-test owner failed")?
    }
}

#[cfg(all(test, unix))]
#[path = "../../test/manager_group_diagnostic_test.rs"]
mod tests;
