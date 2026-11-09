//! Isolate application state while preserving the host's measured network and trust context.
use anyhow::{Context, Result};
use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};
use tokio::process::{Child, Command};

pub(super) struct Environment {
    root: Option<tempfile::TempDir>,
    original_cwd: PathBuf,
    cwd: PathBuf,
    variables: Vec<(OsString, OsString)>,
    spawned: bool,
}

impl Environment {
    pub(super) fn new(runtime: &Path, isolated: bool) -> Result<Self> {
        Self::with_environment(
            runtime,
            isolated,
            std::env::current_dir()?,
            std::env::vars_os().collect(),
        )
    }

    fn with_environment(
        runtime: &Path,
        isolated: bool,
        original_cwd: PathBuf,
        mut variables: Vec<(OsString, OsString)>,
    ) -> Result<Self> {
        variables.retain(|(key, _)| !variable_name(key).starts_with("ASTRA_"));
        let runtime = absolute_in(runtime, &original_cwd)?;
        let mut environment = Self {
            root: None,
            cwd: original_cwd.clone(),
            original_cwd,
            variables,
            spawned: false,
        };
        if isolated {
            let root = tempfile::Builder::new()
                .prefix("nettool-")
                .tempdir_in(runtime)
                .context("Create private diagnostic directory")?;
            astra_platform::storage::private_dir(root.path())?;
            for name in ["runtime", "tmp", "work"] {
                astra_platform::storage::private_dir(&root.path().join(name))?;
            }
            // Dynamic loader variables have their own platform grammar
            // ($ORIGIN, @loader_path, relative names). Preserve their original
            // working directory instead of rewriting or removing them.
            let loader_override = environment.variables.iter().any(|(key, value)| {
                let key = variable_name(key);
                !value.is_empty() && (key.starts_with("LD_") || key.starts_with("DYLD_"))
            });
            // A valid cwd can contain a path-list separator. In that case a
            // relative PATH/certificate entry cannot be represented after
            // rebasing. Keep cwd and every original path value together;
            // private runtime/temporary storage still applies below.
            if !loader_override && environment.rebase_paths().is_ok() {
                environment.cwd = root.path().join("work");
            }
            environment.set(
                "ASTRA_RUNTIME_DIR",
                root.path().join("runtime").into_os_string(),
            );
            for name in ["TMPDIR", "TMP", "TEMP"] {
                environment.set(name, root.path().join("tmp").into_os_string());
            }
            environment.root = Some(root);
        } else {
            // Legacy outbound loading resolves subscription caches using the
            // original profile identity and relative assets from its base.
            environment.set("ASTRA_RUNTIME_DIR", runtime.into_os_string());
        }
        Ok(environment)
    }

    fn set(&mut self, key: &str, value: OsString) {
        self.variables
            .retain(|(existing, _)| variable_name(existing) != variable_name(OsStr::new(key)));
        self.variables.push((key.into(), value));
    }

    fn rebase_paths(&mut self) -> Result<()> {
        // Commit all rewrites together, or none: a partial rewrite can change
        // DNS/trust behavior when the caller must retain its original cwd.
        let mut variables = self.variables.clone();
        for (key, value) in &mut variables {
            let key = variable_name(key);
            if matches!(
                key.as_ref(),
                "SSL_CERT_FILE" | "HOSTALIASES" | "RESOLV_HOST_CONF"
            ) {
                if !value.is_empty() {
                    *value = absolute_in(Path::new(value), &self.original_cwd)?.into_os_string();
                }
            } else if matches!(key.as_ref(), "SSL_CERT_DIR" | "PATH") {
                let paths = std::env::split_paths(value)
                    .map(|path| {
                        // Certificate loaders ignore empty directory entries;
                        // unlike PATH, they must not acquire trust from cwd.
                        if key == "SSL_CERT_DIR" && path.as_os_str().is_empty() {
                            Ok(path)
                        } else {
                            absolute_in(&path, &self.original_cwd)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                *value = std::env::join_paths(paths)
                    .with_context(|| format!("Preserve diagnostic {key} paths"))?;
            }
        }
        self.variables = variables;
        Ok(())
    }

    pub(super) fn absolute(&self, path: &Path) -> Result<PathBuf> {
        absolute_in(path, &self.original_cwd)
    }

    pub(super) fn command(&self, executable: &Path) -> Result<Command> {
        let mut command = astra_platform::platform::background_core(self.absolute(executable)?);
        command
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.variables.iter().map(|(k, v)| (k, v)));
        Ok(command)
    }

    pub(super) fn spawn(&mut self, command: &mut Command) -> Result<Child> {
        let child = command.spawn()?;
        // An unexpectedly dropped future must not remove files while its
        // kill-on-drop child may still be using them. Normal paths explicitly
        // reap first, then call finish to remove their private directory.
        self.spawned = true;
        Ok(child)
    }

    pub(super) fn finish<T>(mut self, result: Result<T>) -> Result<T> {
        if result.as_ref().err().is_some_and(|error| {
            error
                .chain()
                .any(|cause| cause.is::<crate::network::CleanupUnconfirmed>())
        }) {
            if let Some(root) = self.root.take() {
                let path = root.keep();
                return result.with_context(|| format!("Private diagnostic directory retained until process cleanup is confirmed: {}", path.display()));
            }
            return result;
        }
        self.spawned = false;
        let cleanup = self
            .root
            .take()
            .map(|root| root.close())
            .transpose()
            .context("Remove private diagnostic directory");
        match (result, cleanup) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(error), Ok(_)) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(error.context(format!(
                "Diagnostic directory cleanup also failed: {cleanup:#}"
            ))),
        }
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        if self.spawned {
            if let Some(root) = self.root.take() {
                let _retained = root.keep();
            }
        }
    }
}

fn absolute_in(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    // Windows drive-relative paths (C:certs) can remain relative after join.
    // Resolve them now, before the child receives a different cwd.
    std::path::absolute(path).context("Resolve diagnostic environment path")
}

fn variable_name(key: &OsStr) -> Cow<'_, str> {
    let name = key.to_string_lossy();
    #[cfg(windows)]
    return Cow::Owned(name.to_ascii_uppercase());
    #[cfg(not(windows))]
    name
}

#[cfg(test)]
#[path = "../test/nettool_environment_test.rs"]
mod tests;
