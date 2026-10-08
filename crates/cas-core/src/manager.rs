use crate::auth::{
    auth_last_refresh_millis, auth_plan_type, auth_subject, identities_match, identity_can_enrich,
    validate_auth,
};
use crate::fsutil::{atomic_write, atomic_write_json};
use crate::process::{ensure_no_codex_processes, terminate_all_codex};
use crate::remote::{
    begin_browser_login, begin_device_login, complete_browser_login, complete_device_login,
    probe_account, test_account_streaming,
};
use crate::{
    AccountChoice, AccountRecord, AccountStatus, AccountStatusSnapshot, AccountTestResult,
    AuthIdentity, BrowserLogin, CasError, CasPaths, CurrentAccount, DeviceLogin, ProcessInfo,
    Registry, RemoveActiveResult, Result, SwitchResult,
};
use fs2::FileExt;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Cas {
    paths: CasPaths,
}

struct StateLock(File);

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl Cas {
    pub fn discover() -> Result<Self> {
        Self::new(CasPaths::discover()?)
    }

    pub fn new(paths: CasPaths) -> Result<Self> {
        paths.ensure()?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &CasPaths {
        &self.paths
    }

    fn lock(&self) -> Result<StateLock> {
        self.paths.ensure()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.paths.lock_path)?;
        std::fs::set_permissions(
            &self.paths.lock_path,
            std::fs::Permissions::from_mode(0o600),
        )?;
        file.try_lock_exclusive().map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                CasError::LockBusy
            } else {
                CasError::Io(e)
            }
        })?;
        Ok(StateLock(file))
    }

    pub fn load_registry(&self) -> Result<Registry> {
        if !self.paths.registry_path.exists() {
            return Ok(Registry::default());
        }
        let bytes = std::fs::read(&self.paths.registry_path)?;
        let mut registry: Registry = serde_json::from_slice(&bytes)?;
        validate_registry(&registry)?;
        registry
            .accounts
            .sort_by_key(|a| a.display_name().to_lowercase());
        Ok(registry)
    }

    fn save_registry(&self, registry: &Registry) -> Result<()> {
        validate_registry(registry)?;
        atomic_write_json(&self.paths.registry_path, registry)?;
        let reread: Registry = serde_json::from_slice(&std::fs::read(&self.paths.registry_path)?)?;
        validate_registry(&reread)?;
        if reread != *registry {
            return Err(CasError::Verification(
                "registry read-back did not match the write".into(),
            ));
        }
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let _lock = self.lock()?;
        Ok(self.load_registry_reconciled()?.accounts)
    }

    pub fn account_choices(&self) -> Result<Vec<AccountChoice>> {
        let _lock = self.lock()?;
        let registry = self.load_registry_reconciled()?;
        self.account_choices_from_registry(&registry)
    }

    pub fn account_choices_prefix(&self, selector: &str) -> Result<Vec<AccountChoice>> {
        let needle = selector.trim().to_lowercase();
        if needle.is_empty() {
            return Err(CasError::AccountNotFound(selector.into()));
        }
        Ok(self
            .account_choices()?
            .into_iter()
            .filter(|choice| {
                choice
                    .account
                    .email
                    .as_deref()
                    .is_some_and(|email| email.to_lowercase().starts_with(&needle))
            })
            .collect())
    }

    pub fn account_choices_exact_email(&self, email: &str) -> Result<Vec<AccountChoice>> {
        let needle = email.trim();
        if needle.is_empty() {
            return Err(CasError::AccountNotFound(email.into()));
        }
        Ok(self
            .account_choices()?
            .into_iter()
            .filter(|choice| {
                choice
                    .account
                    .email
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
            })
            .collect())
    }

    pub fn current_account(&self) -> Result<CurrentAccount> {
        let _lock = self.lock()?;
        let registry = self.load_registry_reconciled()?;
        if !self.paths.codex_auth_path.exists() {
            return Ok(CurrentAccount {
                account: None,
                identity: None,
                managed: false,
                plan_type: None,
            });
        }
        let bytes = self.read_stable_active_auth()?;
        let identity = validate_auth(&bytes)?;
        let matched = registry
            .accounts
            .iter()
            .find(|a| identities_match(&a.identity(), &identity))
            .cloned();
        let account = matched.or_else(|| {
            if identity.has_stable_field() {
                None
            } else {
                registry
                    .active_account_id
                    .as_deref()
                    .and_then(|id| registry.accounts.iter().find(|a| a.id == id).cloned())
            }
        });
        let plan_type = auth_plan_type(&bytes).or_else(|| {
            account
                .as_ref()
                .and_then(|a| a.last_status.as_ref())
                .and_then(|s| s.plan_type.as_ref())
                .map(|s| match s.trim().to_ascii_lowercase().as_str() {
                    "team" => "business".to_owned(),
                    _ => s.trim().to_ascii_lowercase(),
                })
        });
        Ok(CurrentAccount {
            managed: account.is_some(),
            account,
            identity: Some(identity),
            plan_type,
        })
    }

    pub fn import_current(&self, alias: Option<String>) -> Result<AccountRecord> {
        let _lock = self.lock()?;
        let bytes = self.read_stable_active_auth()?;
        let identity = validate_auth(&bytes)?;
        self.upsert_saved_account(identity, alias, &bytes, true)
    }

    pub fn input(&self, path: Option<&Path>) -> Result<AccountRecord> {
        let _lock = self.lock()?;
        let source_buf = match path {
            Some(path) if path.is_dir() => path.join("auth.json"),
            Some(path) => path.to_path_buf(),
            None => self.paths.codex_auth_path.clone(),
        };
        let source = source_buf.as_path();
        if !source.exists() {
            return Err(CasError::ActiveAuthMissing(source.to_path_buf()));
        }

        let is_current = path.is_none() || paths_equivalent(source, &self.paths.codex_auth_path);
        let bytes = if is_current {
            self.read_stable_active_auth()?
        } else {
            std::fs::read(source)?
        };
        let identity = validate_auth(&bytes)?;
        if !identity.has_stable_field() {
            return Err(CasError::InvalidAuth(
                "credential did not contain a stable account identity".into(),
            ));
        }
        self.upsert_saved_account(identity, None, &bytes, is_current)
    }

    pub fn status(&self, selector: Option<&str>) -> Result<Vec<AccountStatus>> {
        let _lock = self.lock()?;
        let registry = self.load_registry_reconciled()?;

        let target_ids = match selector {
            Some(selector) => vec![resolve_email_prefix(&registry, selector)?.id.clone()],
            None => registry
                .accounts
                .iter()
                .map(|account| account.id.clone())
                .collect(),
        };
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }

        self.status_ids_locked(registry, &target_ids)
    }

    pub fn status_ids(&self, target_ids: &[String]) -> Result<Vec<AccountStatus>> {
        let _lock = self.lock()?;
        let registry = self.load_registry_reconciled()?;
        for id in target_ids {
            let _ = resolve_id_exact(&registry, id)?;
        }
        self.status_ids_locked(registry, target_ids)
    }

    fn status_ids_locked(
        &self,
        mut registry: Registry,
        target_ids: &[String],
    ) -> Result<Vec<AccountStatus>> {
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }

        let active_bytes = if self.paths.codex_auth_path.exists() {
            Some(self.read_stable_active_auth()?)
        } else {
            None
        };
        let active_identity = active_bytes.as_deref().map(validate_auth).transpose()?;
        let mut targets = Vec::with_capacity(target_ids.len());
        for id in target_ids {
            let account = registry
                .accounts
                .iter()
                .find(|account| account.id == *id)
                .cloned()
                .ok_or_else(|| CasError::AccountNotFound(id.clone()))?;

            let is_active = active_identity
                .as_ref()
                .is_some_and(|identity| identities_match(&account.identity(), identity))
                || (active_identity.is_none()
                    && registry.active_account_id.as_deref() == Some(account.id.as_str()));
            targets.push((id.clone(), account, is_active));
        }

        let probe_results = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(targets.len());
            for (id, account, is_active) in &targets {
                let active = if *is_active {
                    active_bytes.as_deref()
                } else {
                    None
                };
                handles.push((
                    id.clone(),
                    scope.spawn(move || {
                        self.refresh_saved_account_status(account, active, *is_active)
                    }),
                ));
            }

            handles
                .into_iter()
                .map(|(id, handle)| {
                    let snapshot = handle.join().map_err(|_| {
                        CasError::Verification(format!(
                            "usage refresh worker panicked for account {id}"
                        ))
                    })??;
                    Ok((id, snapshot))
                })
                .collect::<Result<Vec<_>>>()
        })?;

        let now = chrono::Utc::now().timestamp();
        for (id, snapshot) in &probe_results {
            if let Some(record) = registry.accounts.iter_mut().find(|record| record.id == *id) {
                record.last_status = Some(snapshot.clone());
                record.updated_at = now;

                if snapshot.valid == Some(true) {
                    let auth_path = self.paths.account_auth_path(&record.id);
                    if let Ok(bytes) = std::fs::read(&auth_path)
                        && let Ok(identity) = validate_auth(&bytes)
                    {
                        merge_identity(record, &identity);
                    }
                }
            }
        }
        registry.updated_at = now;
        self.save_registry(&registry)?;

        probe_results
            .into_iter()
            .map(|(id, snapshot)| {
                let account = registry
                    .accounts
                    .iter()
                    .find(|record| record.id == id)
                    .cloned()
                    .ok_or_else(|| CasError::AccountNotFound(id.clone()))?;
                Ok(AccountStatus { account, snapshot })
            })
            .collect()
    }

    pub fn status_id(&self, id: &str) -> Result<AccountStatus> {
        self.status_ids(&[id.to_owned()])?
            .into_iter()
            .next()
            .ok_or_else(|| CasError::AccountNotFound(id.into()))
    }

    pub fn test_refresh_all(&self) -> Result<Vec<AccountTestResult>> {
        let _lock = self.lock()?;
        let registry = self.load_registry_reconciled()?;
        if registry.accounts.is_empty() {
            return Ok(Vec::new());
        }

        let active_bytes = if self.paths.codex_auth_path.exists() {
            Some(self.read_stable_active_auth()?)
        } else {
            None
        };
        let active_identity = active_bytes.as_deref().map(validate_auth).transpose()?;

        let worker_results = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(registry.accounts.len());
            for account in &registry.accounts {
                let account = account.clone();
                let active = active_bytes.as_deref();
                let is_active = active_identity
                    .as_ref()
                    .is_some_and(|identity| identities_match(&account.identity(), identity));
                handles.push(
                    scope.spawn(move || self.test_one_saved_account(account, active, is_active)),
                );
            }

            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        CasError::Verification("test/refresh worker panicked".into())
                    })?
                })
                .collect::<Result<Vec<_>>>()
        })?;

        let mut results = Vec::with_capacity(worker_results.len());
        for (result, refreshed, sync_active) in worker_results {
            if let Some(bytes) = refreshed {
                let saved_path = self.paths.account_auth_path(&result.account.id);
                atomic_write(&saved_path, &bytes)?;
                verify_bytes(&saved_path, &bytes)?;
                if sync_active {
                    atomic_write(&self.paths.codex_auth_path, &bytes)?;
                    verify_bytes(&self.paths.codex_auth_path, &bytes)?;
                }
            }
            results.push(result);
        }
        Ok(results)
    }

    pub fn begin_device_login(&self) -> Result<DeviceLogin> {
        begin_device_login()
    }

    pub fn complete_device_login(
        &self,
        login: DeviceLogin,
        alias: Option<String>,
    ) -> Result<AccountRecord> {
        let bytes = complete_device_login(login)?;
        let identity = validate_auth(&bytes)?;
        if !identity.has_stable_field() {
            return Err(CasError::InvalidAuth(
                "login credential did not contain a stable account identity".into(),
            ));
        }
        let _lock = self.lock()?;
        self.upsert_saved_account(identity, alias, &bytes, false)
    }

    pub fn begin_browser_login(&self) -> Result<BrowserLogin> {
        begin_browser_login()
    }

    pub fn complete_browser_login(
        &self,
        login: BrowserLogin,
        alias: Option<String>,
    ) -> Result<AccountRecord> {
        let bytes = complete_browser_login(login)?;
        let identity = validate_auth(&bytes)?;
        if !identity.has_stable_field() {
            return Err(CasError::InvalidAuth(
                "login credential did not contain a stable account identity".into(),
            ));
        }
        let _lock = self.lock()?;
        self.upsert_saved_account(identity, alias, &bytes, false)
    }

    pub fn switch(&self, selector: &str) -> Result<SwitchResult> {
        self.switch_impl(selector, false)
    }

    pub fn switch_id(&self, id: &str) -> Result<SwitchResult> {
        self.switch_impl(id, true)
    }

    fn switch_impl(&self, selector: &str, exact_id: bool) -> Result<SwitchResult> {
        let _lock = self.lock()?;
        self.ensure_file_credential_store()?;

        let mut registry = self.load_registry_reconciled()?;
        let target = if exact_id {
            resolve_id_exact(&registry, selector)?
        } else {
            resolve_email_prefix(&registry, selector)?
        }
        .clone();
        let target_path = self.paths.account_auth_path(&target.id);
        if !target_path.exists() {
            return Err(CasError::SavedAuthMissing(target.id.clone()));
        }

        let terminated_processes = terminate_all_codex()?;
        ensure_no_codex_processes()?;

        let from = if self.paths.codex_auth_path.exists() {
            let current_bytes = std::fs::read(&self.paths.codex_auth_path)?;
            let current_identity = validate_auth(&current_bytes)?;
            let current = self.capture_current_after_shutdown(
                &mut registry,
                current_identity,
                &current_bytes,
            )?;
            self.save_registry(&registry)?;
            Some(current)
        } else {
            None
        };

        // If the requested account is already active, the capture above may have
        // refreshed this exact slot with newer tokens. Read it only after capture.
        let target_bytes = std::fs::read(&target_path)?;
        let target_identity = validate_auth(&target_bytes)?;

        ensure_no_codex_processes()?;
        atomic_write(&self.paths.codex_auth_path, &target_bytes)?;

        let verified = std::fs::read(&self.paths.codex_auth_path)?;
        if verified != target_bytes {
            return Err(CasError::Verification(
                "active auth.json differs from the selected saved credential".into(),
            ));
        }
        let verified_identity = validate_auth(&verified)?;
        if target_identity.has_stable_field()
            && verified_identity.has_stable_field()
            && !identities_match(&target_identity, &verified_identity)
        {
            return Err(CasError::Verification(
                "active auth.json identity differs from the selected account".into(),
            ));
        }

        if !crate::process::list_codex_processes().is_empty() {
            let _ = terminate_all_codex()?;
        }
        ensure_no_codex_processes()?;

        let now = chrono::Utc::now().timestamp();
        if let Some(record) = registry.accounts.iter_mut().find(|a| a.id == target.id) {
            record.last_activated_at = Some(now);
            record.updated_at = now;
        }
        registry.active_account_id = Some(target.id.clone());
        registry.updated_at = now;
        self.save_registry(&registry)?;

        let verified_registry = self.load_registry()?;
        if verified_registry.active_account_id.as_deref() != Some(target.id.as_str()) {
            return Err(CasError::Verification(
                "registry did not retain the selected active account".into(),
            ));
        }

        Ok(SwitchResult {
            from,
            to: target,
            terminated_processes,
        })
    }

    pub fn remove(&self, selector: &str) -> Result<AccountRecord> {
        self.remove_impl(selector, false)
    }

    pub fn remove_id(&self, id: &str) -> Result<AccountRecord> {
        self.remove_impl(id, true)
    }

    /// Delete the account that is *actually* active, but only after the caller
    /// explicitly confirms it. Unlike `remove_id`, this shuts down Codex and
    /// removes both the active auth.json and the saved CAS credential.
    /// A second identity check under the state lock prevents deleting an auth
    /// that was switched between displaying the menu and accepting the prompt.
    pub fn remove_active_id(&self, id: &str) -> Result<RemoveActiveResult> {
        self.remove_active_id_with_shutdown(id, terminate_all_codex, ensure_no_codex_processes)
    }

    // Inject the process shutdown hooks in tests so tests never terminate
    // unrelated user Codex instances. Production always invokes the real
    // shutdown and verification functions above.
    fn remove_active_id_with_shutdown<F, C>(
        &self,
        id: &str,
        shutdown: F,
        check_stopped: C,
    ) -> Result<RemoveActiveResult>
    where
        F: FnOnce() -> Result<Vec<ProcessInfo>>,
        C: Fn() -> Result<()>,
    {
        let _lock = self.lock()?;
        self.ensure_file_credential_store()?;
        let mut registry = self.load_registry_reconciled()?;
        let target = resolve_id_exact(&registry, id)?.clone();

        let before = self.read_stable_active_auth()?;
        let identity = validate_auth(&before)?;
        if !identities_match(&target.identity(), &identity) {
            return Err(CasError::Verification(
                "selected account is no longer the active auth; refusing privileged removal".into(),
            ));
        }
        // Validate removable storage BEFORE closing Codex, so an unsafe
        // account directory does not unnecessarily stop the user's session.
        self.check_removable_account_storage(&target.id)?;

        let terminated_processes = shutdown()?;
        check_stopped()?;

        // Codex may have refreshed the active auth during shutdown. Validate
        // the final credential once more and retain its bytes for rollback.
        let active_bytes = self.read_stable_active_auth()?;
        let active_identity = validate_auth(&active_bytes)?;
        if !identities_match(&target.identity(), &active_identity) {
            return Err(CasError::Verification(
                "active auth changed during Codex shutdown; refusing to delete it".into(),
            ));
        }
        let saved_path = self.paths.account_auth_path(&target.id);
        let saved_bytes = if saved_path.exists() {
            Some(std::fs::read(&saved_path)?)
        } else {
            None
        };
        self.check_removable_account_storage(&target.id)?;
        check_stopped()?;

        // A deletion of an active slot must remove auth.json itself; leaving
        // it behind would silently keep the supposedly deleted login active.
        // Deleting the CAS slot first lets us restore it from the captured
        // bytes if unlinking the active auth.json fails.
        self.remove_account_storage(&target.id)?;
        if let Err(error) = std::fs::remove_file(&self.paths.codex_auth_path) {
            self.restore_removed_account(&target.id, saved_bytes.as_deref())?;
            return Err(error.into());
        }

        registry.accounts.retain(|account| account.id != target.id);
        registry.active_account_id = None;
        registry.updated_at = chrono::Utc::now().timestamp();
        if let Err(error) = self.save_registry(&registry) {
            // Preserve login and registry consistency on I/O failures.
            self.restore_removed_account(&target.id, saved_bytes.as_deref())?;
            atomic_write(&self.paths.codex_auth_path, &active_bytes)?;
            return Err(error);
        }

        Ok(RemoveActiveResult {
            account: target,
            terminated_processes,
        })
    }

    fn restore_removed_account(&self, id: &str, saved: Option<&[u8]>) -> Result<()> {
        if let Some(bytes) = saved {
            self.paths.ensure_account_dir(id)?;
            let path = self.paths.account_auth_path(id);
            atomic_write(&path, bytes)?;
            verify_bytes(&path, bytes)?;
        } else if self.paths.account_dir(id).exists() {
            // A previously missing CAS slot must remain missing after rollback.
            self.remove_account_storage(id)?;
        }
        Ok(())
    }

    fn remove_impl(&self, selector: &str, exact_id: bool) -> Result<AccountRecord> {
        let _lock = self.lock()?;
        let mut registry = self.load_registry_reconciled()?;
        let target = if exact_id {
            resolve_id_exact(&registry, selector)?
        } else {
            resolve_email_exact(&registry, selector)?
        }
        .clone();
        let is_current = if self.paths.codex_auth_path.exists() {
            let bytes = std::fs::read(&self.paths.codex_auth_path)?;
            let identity = validate_auth(&bytes)?;
            identities_match(&target.identity(), &identity)
        } else {
            false
        };
        if is_current || registry.active_account_id.as_deref() == Some(target.id.as_str()) {
            return Err(CasError::ActiveAccountRemoval);
        }

        self.remove_account_storage(&target.id)?;
        registry.accounts.retain(|a| a.id != target.id);
        registry.updated_at = chrono::Utc::now().timestamp();
        self.save_registry(&registry)?;
        Ok(target)
    }

    fn remove_account_storage(&self, id: &str) -> Result<()> {
        if !self.check_removable_account_storage(id)? {
            return Ok(());
        }
        let dir = self.paths.account_dir(id);
        let auth_path = dir.join("auth.json");
        match std::fs::remove_file(&auth_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_dir(&dir)?;
        Ok(())
    }

    fn check_removable_account_storage(&self, id: &str) -> Result<bool> {
        validate_account_id(id)?;
        let dir = self.paths.account_dir(id);
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(CasError::Verification(format!(
                "refusing to remove non-directory or symlink account storage: {}",
                dir.display()
            )));
        }

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name() != "auth.json" {
                return Err(CasError::Verification(format!(
                    "refusing to recursively remove account storage with unexpected entry: {}",
                    entry.path().display()
                )));
            }
        }

        Ok(true)
    }

    fn capture_current_after_shutdown(
        &self,
        registry: &mut Registry,
        identity: AuthIdentity,
        bytes: &[u8],
    ) -> Result<AccountRecord> {
        if let Some(record) = registry
            .accounts
            .iter_mut()
            .find(|a| identities_match(&a.identity(), &identity))
        {
            let now = chrono::Utc::now().timestamp();
            merge_identity(record, &identity);
            record.updated_at = now;
            self.paths.ensure_account_dir(&record.id)?;
            let path = self.paths.account_auth_path(&record.id);
            let bytes_to_store = if path.exists() {
                choose_newer_auth(std::fs::read(&path)?, bytes)
            } else {
                bytes.to_vec()
            };
            atomic_write(&path, &bytes_to_store)?;
            verify_bytes(&path, &bytes_to_store)?;
            registry.active_account_id = Some(record.id.clone());
            registry.updated_at = now;
            return Ok(record.clone());
        }

        if !identity.has_stable_field()
            && let Some(active_id) = registry.active_account_id.clone()
            && let Some(record) = registry.accounts.iter_mut().find(|a| a.id == active_id)
        {
            let now = chrono::Utc::now().timestamp();
            record.updated_at = now;
            self.paths.ensure_account_dir(&record.id)?;
            let path = self.paths.account_auth_path(&record.id);
            let bytes_to_store = if path.exists() {
                choose_newer_auth(std::fs::read(&path)?, bytes)
            } else {
                bytes.to_vec()
            };
            atomic_write(&path, &bytes_to_store)?;
            verify_bytes(&path, &bytes_to_store)?;
            registry.updated_at = now;
            return Ok(record.clone());
        }

        let now = chrono::Utc::now().timestamp();
        let record = AccountRecord {
            id: Uuid::new_v4().to_string(),
            alias: None,
            email: identity.email,
            account_id: identity.account_id,
            user_id: identity.user_id,
            created_at: now,
            updated_at: now,
            last_activated_at: Some(now),
            last_status: None,
        };
        self.paths.ensure_account_dir(&record.id)?;
        let path = self.paths.account_auth_path(&record.id);
        atomic_write(&path, bytes)?;
        verify_bytes(&path, bytes)?;
        registry.active_account_id = Some(record.id.clone());
        registry.accounts.push(record.clone());
        registry.updated_at = now;
        Ok(record)
    }

    fn upsert_saved_account(
        &self,
        identity: AuthIdentity,
        alias: Option<String>,
        bytes: &[u8],
        set_active: bool,
    ) -> Result<AccountRecord> {
        let mut registry = self.load_registry_reconciled()?;
        let now = chrono::Utc::now().timestamp();
        let id = if let Some(existing) = registry
            .accounts
            .iter_mut()
            .find(|a| identities_match(&a.identity(), &identity))
        {
            merge_identity(existing, &identity);
            if let Some(alias) = alias.filter(|s| !s.trim().is_empty()) {
                existing.alias = Some(alias);
            }
            existing.updated_at = now;
            existing.id.clone()
        } else {
            let record = AccountRecord {
                id: Uuid::new_v4().to_string(),
                alias: alias.filter(|s| !s.trim().is_empty()),
                email: identity.email.clone(),
                account_id: identity.account_id.clone(),
                user_id: identity.user_id.clone(),
                created_at: now,
                updated_at: now,
                last_activated_at: None,
                last_status: None,
            };
            let id = record.id.clone();
            registry.accounts.push(record);
            id
        };

        let auth_path = self.paths.account_auth_path(&id);
        self.paths.ensure_account_dir(&id)?;
        let mut bytes_to_store = if auth_path.exists() {
            choose_newer_auth(std::fs::read(&auth_path)?, bytes)
        } else {
            bytes.to_vec()
        };
        if self.paths.codex_auth_path.exists()
            && let Ok(active_bytes) = self.read_stable_active_auth()
            && let Ok(active_identity) = validate_auth(&active_bytes)
            && identities_match(&identity, &active_identity)
        {
            bytes_to_store = choose_newer_auth(bytes_to_store, &active_bytes);
        }
        atomic_write(&auth_path, &bytes_to_store)?;
        verify_bytes(&auth_path, &bytes_to_store)?;

        if set_active {
            registry.active_account_id = Some(id.clone());
            atomic_write(&self.paths.codex_auth_path, &bytes_to_store)?;
            verify_bytes(&self.paths.codex_auth_path, &bytes_to_store)?;
        }
        registry.updated_at = now;
        registry
            .accounts
            .sort_by_key(|a| a.display_name().to_lowercase());
        self.save_registry(&registry)?;
        Ok(registry
            .accounts
            .iter()
            .find(|a| a.id == id)
            .unwrap()
            .clone())
    }

    fn load_registry_reconciled(&self) -> Result<Registry> {
        let mut registry = self.load_registry()?;
        let identities_changed = self.enrich_registry_identities_from_storage(&mut registry)?;
        let duplicates_changed = self.reconcile_registry_duplicates(&mut registry)?;
        if identities_changed || duplicates_changed {
            registry
                .accounts
                .sort_by_key(|account| account.display_name().to_lowercase());
            registry.updated_at = chrono::Utc::now().timestamp();
            self.save_registry(&registry)?;
        }
        Ok(registry)
    }

    fn account_choices_from_registry(&self, registry: &Registry) -> Result<Vec<AccountChoice>> {
        registry
            .accounts
            .iter()
            .cloned()
            .map(|account| {
                let auth_path = self.paths.account_auth_path(&account.id);
                let (token_last_refresh_millis, token_plan) = if auth_path.exists() {
                    let bytes = std::fs::read(&auth_path)?;
                    (auth_last_refresh_millis(&bytes), auth_plan_type(&bytes))
                } else {
                    (None, None)
                };
                let auth_type = token_plan.or_else(|| {
                    account
                        .last_status
                        .as_ref()
                        .and_then(|status| status.plan_type.clone())
                        .map(|plan| match plan.trim().to_ascii_lowercase().as_str() {
                            "team" => "business".into(),
                            _ => plan.trim().to_ascii_lowercase(),
                        })
                });
                Ok(AccountChoice {
                    account,
                    token_last_refresh_millis,
                    auth_type,
                })
            })
            .collect()
    }

    fn enrich_registry_identities_from_storage(&self, registry: &mut Registry) -> Result<bool> {
        let mut changed = false;
        for record in &mut registry.accounts {
            let path = self.paths.account_auth_path(&record.id);
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let saved_identity = validate_auth(&bytes)?;
            let recorded_identity = record.identity();
            let legacy_subject_upgrade = auth_subject(&bytes).is_some_and(|subject| {
                recorded_identity.user_id.as_deref() == Some(subject.as_str())
                    && saved_identity.user_id.as_deref() != Some(subject.as_str())
                    && recorded_identity.account_id == saved_identity.account_id
                    && match (&recorded_identity.email, &saved_identity.email) {
                        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                        _ => true,
                    }
            });
            if recorded_identity.has_stable_field()
                && saved_identity.has_stable_field()
                && !identities_match(&recorded_identity, &saved_identity)
                && !identity_can_enrich(&recorded_identity, &saved_identity)
                && !legacy_subject_upgrade
            {
                return Err(CasError::Verification(format!(
                    "saved credential identity conflicts with registry record {}",
                    record.id
                )));
            }
            let before = record.identity();
            merge_identity(record, &saved_identity);
            changed |= before != record.identity();
        }
        Ok(changed)
    }

    fn reconcile_registry_duplicates(&self, registry: &mut Registry) -> Result<bool> {
        let mut changed = false;
        loop {
            let mut duplicate_pair = None;
            'outer: for left in 0..registry.accounts.len() {
                for right in (left + 1)..registry.accounts.len() {
                    if identities_match(
                        &registry.accounts[left].identity(),
                        &registry.accounts[right].identity(),
                    ) {
                        duplicate_pair = Some((left, right));
                        break 'outer;
                    }
                }
            }
            let Some((left, right)) = duplicate_pair else {
                break;
            };

            let left_id = registry.accounts[left].id.clone();
            let right_id = registry.accounts[right].id.clone();
            let active_id = registry.active_account_id.as_deref();
            let keep_left = if active_id == Some(left_id.as_str()) {
                true
            } else if active_id == Some(right_id.as_str()) {
                false
            } else {
                let left_record = &registry.accounts[left];
                let right_record = &registry.accounts[right];
                (left_record.created_at, left_record.id.as_str())
                    <= (right_record.created_at, right_record.id.as_str())
            };
            let (keep_id, drop_id) = if keep_left {
                (left_id, right_id)
            } else {
                (right_id, left_id)
            };
            self.merge_duplicate_accounts(registry, &keep_id, &drop_id)?;
            changed = true;
        }
        Ok(changed)
    }

    fn merge_duplicate_accounts(
        &self,
        registry: &mut Registry,
        keep_id: &str,
        drop_id: &str,
    ) -> Result<()> {
        let keep = registry
            .accounts
            .iter()
            .find(|account| account.id == keep_id)
            .cloned()
            .ok_or_else(|| CasError::AccountNotFound(keep_id.into()))?;
        let drop = registry
            .accounts
            .iter()
            .find(|account| account.id == drop_id)
            .cloned()
            .ok_or_else(|| CasError::AccountNotFound(drop_id.into()))?;
        if !identities_match(&keep.identity(), &drop.identity()) {
            return Err(CasError::Verification(
                "refusing to merge records that are not the same account".into(),
            ));
        }

        let mut merged = keep.clone();
        merge_identity(&mut merged, &drop.identity());
        merged.created_at = merged.created_at.min(drop.created_at);
        merged.updated_at = merged.updated_at.max(drop.updated_at);
        merged.last_activated_at = match (merged.last_activated_at, drop.last_activated_at) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
        if merged.alias.as_deref().is_none_or(str::is_empty) {
            merged.alias = drop.alias.clone().filter(|value| !value.trim().is_empty());
        }
        merged.last_status = match (&merged.last_status, &drop.last_status) {
            (Some(left), Some(right)) if right.checked_at > left.checked_at => Some(right.clone()),
            (None, Some(right)) => Some(right.clone()),
            _ => merged.last_status.clone(),
        };

        let mut freshest: Option<Vec<u8>> = None;
        for record in [&keep, &drop] {
            let path = self.paths.account_auth_path(&record.id);
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let saved_identity = validate_auth(&bytes)?;
            if record.identity().has_stable_field()
                && saved_identity.has_stable_field()
                && !identities_match(&record.identity(), &saved_identity)
            {
                return Err(CasError::Verification(format!(
                    "refusing to merge duplicate record {} because its saved credential belongs to a different account",
                    record.id
                )));
            }
            freshest = Some(match freshest {
                Some(current) => choose_newer_auth(current, &bytes),
                None => bytes,
            });
        }

        if self.paths.codex_auth_path.exists() {
            let active_bytes = self.read_stable_active_auth()?;
            let active_identity = validate_auth(&active_bytes)?;
            if identities_match(&merged.identity(), &active_identity) {
                freshest = Some(match freshest {
                    Some(current) => choose_newer_auth(current, &active_bytes),
                    None => active_bytes,
                });
            }
        }

        if let Some(bytes) = freshest {
            self.paths.ensure_account_dir(keep_id)?;
            let keep_path = self.paths.account_auth_path(keep_id);
            atomic_write(&keep_path, &bytes)?;
            verify_bytes(&keep_path, &bytes)?;
            let newest_identity = validate_auth(&bytes)?;
            merge_identity(&mut merged, &newest_identity);
        }

        self.remove_account_storage(drop_id)?;
        if registry.active_account_id.as_deref() == Some(drop_id) {
            registry.active_account_id = Some(keep_id.to_owned());
        }
        registry.accounts.retain(|account| account.id != drop_id);
        if let Some(record) = registry
            .accounts
            .iter_mut()
            .find(|account| account.id == keep_id)
        {
            *record = merged;
        }
        Ok(())
    }

    fn refresh_saved_account_status(
        &self,
        account: &AccountRecord,
        active_bytes: Option<&[u8]>,
        sync_active_back: bool,
    ) -> Result<AccountStatusSnapshot> {
        let checked_at = chrono::Utc::now().timestamp();
        let saved_path = self.paths.account_auth_path(&account.id);
        if !saved_path.exists() && active_bytes.is_none() {
            return Ok(AccountStatusSnapshot {
                checked_at,
                valid: Some(false),
                plan_type: None,
                five_hour: None,
                long_window: None,
                error: Some(format!(
                    "saved credential file is missing: {}",
                    saved_path.display()
                )),
            });
        }

        let saved_bytes = match (saved_path.exists(), active_bytes) {
            (true, Some(active)) => choose_newer_auth(std::fs::read(&saved_path)?, active),
            (false, Some(active)) => active.to_vec(),
            (true, None) => std::fs::read(&saved_path)?,
            (false, None) => unreachable!("missing saved auth handled above"),
        };
        let saved_identity = match validate_auth(&saved_bytes) {
            Ok(identity) => identity,
            Err(error) => {
                return Ok(AccountStatusSnapshot {
                    checked_at,
                    valid: Some(false),
                    plan_type: None,
                    five_hour: None,
                    long_window: None,
                    error: Some(error.to_string()),
                });
            }
        };
        if account.identity().has_stable_field()
            && saved_identity.has_stable_field()
            && !identities_match(&account.identity(), &saved_identity)
        {
            return Ok(AccountStatusSnapshot {
                checked_at,
                valid: Some(false),
                plan_type: None,
                five_hour: None,
                long_window: None,
                error: Some("saved credential belongs to a different account".into()),
            });
        }
        atomic_write(&saved_path, &saved_bytes)?;
        verify_bytes(&saved_path, &saved_bytes)?;
        let probe = match probe_account(&saved_bytes) {
            Ok(probe) => probe,
            Err(error) => {
                return Ok(AccountStatusSnapshot {
                    checked_at,
                    valid: None,
                    plan_type: None,
                    five_hour: None,
                    long_window: None,
                    error: Some(error.to_string()),
                });
            }
        };

        if probe.valid == Some(true) {
            let refreshed = probe.auth_bytes.as_deref().unwrap_or(&saved_bytes);
            let refreshed_identity = validate_auth(refreshed)?;
            if saved_identity.has_stable_field()
                && refreshed_identity.has_stable_field()
                && !identities_match(&saved_identity, &refreshed_identity)
            {
                return Ok(AccountStatusSnapshot {
                    checked_at,
                    valid: Some(false),
                    plan_type: probe.plan_type,
                    five_hour: None,
                    long_window: None,
                    error: Some(
                        "Codex refreshed a different account than the saved credential".into(),
                    ),
                });
            }

            let final_bytes = if sync_active_back && self.paths.codex_auth_path.exists() {
                choose_newer_auth(std::fs::read(&self.paths.codex_auth_path)?, refreshed)
            } else {
                refreshed.to_vec()
            };
            atomic_write(&saved_path, &final_bytes)?;
            verify_bytes(&saved_path, &final_bytes)?;
            if sync_active_back {
                atomic_write(&self.paths.codex_auth_path, &final_bytes)?;
                verify_bytes(&self.paths.codex_auth_path, &final_bytes)?;
            }
        }

        Ok(AccountStatusSnapshot {
            checked_at,
            valid: probe.valid,
            plan_type: probe.plan_type,
            five_hour: probe.five_hour,
            long_window: probe.long_window,
            error: probe.error,
        })
    }

    fn test_one_saved_account(
        &self,
        account: AccountRecord,
        active_bytes: Option<&[u8]>,
        is_active: bool,
    ) -> Result<(AccountTestResult, Option<Vec<u8>>, bool)> {
        let saved_path = self.paths.account_auth_path(&account.id);
        let bytes = if saved_path.exists() {
            match (is_active, active_bytes) {
                (true, Some(active)) => choose_newer_auth(std::fs::read(&saved_path)?, active),
                _ => std::fs::read(&saved_path)?,
            }
        } else if is_active {
            active_bytes.unwrap_or_default().to_vec()
        } else {
            return Ok((failed_test(account, "saved auth is missing"), None, false));
        };

        let identity = validate_auth(&bytes)?;
        if account.identity().has_stable_field()
            && identity.has_stable_field()
            && !identities_match(&account.identity(), &identity)
        {
            return Ok((
                failed_test(account, "saved auth belongs to a different account"),
                None,
                false,
            ));
        }

        let probe = match test_account_streaming(&bytes) {
            Ok(probe) => probe,
            Err(error) => return Ok((failed_test(account, &error.to_string()), None, false)),
        };
        let refreshed = probe.auth_bytes.clone();
        Ok((
            AccountTestResult {
                account,
                reasoning_effort: probe.reasoning_effort,
                response: probe.response,
                error: probe.error,
                request_headers: probe.request_headers,
            },
            refreshed,
            is_active,
        ))
    }

    fn read_stable_active_auth(&self) -> Result<Vec<u8>> {
        if !self.paths.codex_auth_path.exists() {
            return Err(CasError::ActiveAuthMissing(
                self.paths.codex_auth_path.clone(),
            ));
        }
        let mut previous = std::fs::read(&self.paths.codex_auth_path)?;
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let current = std::fs::read(&self.paths.codex_auth_path)?;
            if current == previous {
                return Ok(current);
            }
            previous = current;
        }
        Err(CasError::Verification(
            "active auth.json changed repeatedly while it was being read".into(),
        ))
    }

    fn ensure_file_credential_store(&self) -> Result<()> {
        if !self.paths.codex_config_path.exists() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&self.paths.codex_config_path)?;
        let value: toml::Value = toml::from_str(&raw)?;
        if let Some(mode) = value
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            && !mode.eq_ignore_ascii_case("file")
        {
            return Err(CasError::UnsupportedCredentialStore(mode.to_owned()));
        }
        Ok(())
    }
}

fn failed_test(account: AccountRecord, message: &str) -> AccountTestResult {
    AccountTestResult {
        account,
        reasoning_effort: "low".into(),
        response: None,
        error: Some(message.to_owned()),
        request_headers: Vec::new(),
    }
}

fn verify_bytes(path: &Path, expected: &[u8]) -> Result<()> {
    let actual = std::fs::read(path)?;
    if actual != expected {
        return Err(CasError::Verification(format!(
            "read-back mismatch at {}",
            path.display()
        )));
    }
    validate_auth(&actual)?;
    Ok(())
}

fn validate_account_id(id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(id)
        .map_err(|_| CasError::Verification(format!("invalid account id in registry: {id}")))?;
    if parsed.hyphenated().to_string() != id {
        return Err(CasError::Verification(format!(
            "non-canonical account id in registry: {id}"
        )));
    }
    Ok(())
}

fn validate_registry(registry: &Registry) -> Result<()> {
    let mut ids = HashSet::with_capacity(registry.accounts.len());
    for account in &registry.accounts {
        validate_account_id(&account.id)?;
        if !ids.insert(account.id.as_str()) {
            return Err(CasError::Verification(format!(
                "duplicate account id in registry: {}",
                account.id
            )));
        }
    }
    if let Some(active_id) = registry.active_account_id.as_deref() {
        validate_account_id(active_id)?;
        if !ids.contains(active_id) {
            return Err(CasError::Verification(format!(
                "active account id is not present in registry: {active_id}"
            )));
        }
    }
    Ok(())
}

fn merge_identity(record: &mut AccountRecord, identity: &AuthIdentity) {
    if identity.email.is_some() {
        record.email = identity.email.clone();
    }
    if identity.account_id.is_some() {
        record.account_id = identity.account_id.clone();
    }
    if identity.user_id.is_some() {
        record.user_id = identity.user_id.clone();
    }
}

fn resolve_email_prefix<'a>(registry: &'a Registry, selector: &str) -> Result<&'a AccountRecord> {
    let needle = selector.trim();
    if needle.is_empty() {
        return Err(CasError::AccountNotFound(selector.into()));
    }
    let lower = needle.to_lowercase();
    let matches: Vec<_> = registry
        .accounts
        .iter()
        .filter(|account| {
            account
                .email
                .as_deref()
                .is_some_and(|email| email.to_lowercase().starts_with(&lower))
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(CasError::AccountNotFound(selector.into())),
        many => Err(CasError::AmbiguousAccount {
            selector: selector.into(),
            matches: many
                .iter()
                .filter_map(|account| account.email.as_deref())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn resolve_email_exact<'a>(registry: &'a Registry, selector: &str) -> Result<&'a AccountRecord> {
    let needle = selector.trim();
    if needle.is_empty() {
        return Err(CasError::AccountNotFound(selector.into()));
    }
    let matches: Vec<_> = registry
        .accounts
        .iter()
        .filter(|account| {
            account
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(needle))
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(CasError::AccountNotFound(selector.into())),
        many => Err(CasError::AmbiguousAccount {
            selector: selector.into(),
            matches: many
                .iter()
                .filter_map(|account| account.email.as_deref())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn resolve_id_exact<'a>(registry: &'a Registry, id: &str) -> Result<&'a AccountRecord> {
    registry
        .accounts
        .iter()
        .find(|account| account.id == id)
        .ok_or_else(|| CasError::AccountNotFound(id.into()))
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn choose_newer_auth(existing: Vec<u8>, candidate: &[u8]) -> Vec<u8> {
    match (
        auth_last_refresh_millis(&existing),
        auth_last_refresh_millis(candidate),
    ) {
        (Some(existing_refresh), Some(candidate_refresh))
            if existing_refresh > candidate_refresh =>
        {
            existing
        }
        (Some(_), None) => existing,
        _ => candidate.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn auth(email: &str, account_id: &str) -> Vec<u8> {
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"{account_id}","id_token":{{"email":"{email}"}}}}}}"#
        )
        .into_bytes()
    }

    fn auth_at(email: &str, account_id: &str, last_refresh: &str) -> Vec<u8> {
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"{account_id}","id_token":{{"email":"{email}"}}}},"last_refresh":"{last_refresh}"}}"#
        )
        .into_bytes()
    }

    fn auth_user(email: &str, account_id: &str, user_id: &str) -> Vec<u8> {
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"{account_id}","id_token":{{"email":"{email}","chatgpt_user_id":"{user_id}"}}}}}}"#
        )
        .into_bytes()
    }

    fn auth_with_legacy_subject(
        email: &str,
        account_id: &str,
        subject: &str,
        user_id: &str,
    ) -> Vec<u8> {
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"{account_id}","id_token":{{"sub":"{subject}","email":"{email}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}","chatgpt_user_id":"{user_id}"}}}}}}}}"#
        )
        .into_bytes()
    }

    fn cas() -> (TempDir, TempDir, Cas) {
        let codex = tempfile::tempdir().unwrap();
        let app = tempfile::tempdir().unwrap();
        let cas = Cas::new(CasPaths::from_codex_home(codex.path().to_path_buf())).unwrap();
        (app, codex, cas)
    }

    #[test]
    fn import_updates_existing_identity_instead_of_duplicating() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("a@example.com", "acc-a"),
        )
        .unwrap();
        let first = cas.import_current(None).unwrap();
        let second = cas.import_current(Some("main".into())).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(cas.list_accounts().unwrap().len(), 1);
        assert_eq!(
            cas.list_accounts().unwrap()[0].alias.as_deref(),
            Some("main")
        );
    }

    #[test]
    fn same_email_different_workspace_ids_are_separate_accounts() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("same@example.com", "workspace-personal"),
        )
        .unwrap();
        let personal = cas.import_current(None).unwrap();

        std::fs::write(
            codex.path().join("auth.json"),
            auth("same@example.com", "workspace-business"),
        )
        .unwrap();
        let business = cas.import_current(None).unwrap();

        assert_ne!(personal.id, business.id);
        let accounts = cas.list_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        let ids: HashSet<_> = accounts
            .iter()
            .filter_map(|account| account.account_id.as_deref())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("workspace-personal"));
        assert!(ids.contains("workspace-business"));
    }

    #[test]
    fn different_users_in_same_workspace_are_separate_accounts() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth_user("gmail@example.com", "workspace-shared", "user-gmail"),
        )
        .unwrap();
        let gmail = cas.import_current(None).unwrap();

        std::fs::write(
            codex.path().join("auth.json"),
            auth_user("outlook@example.com", "workspace-shared", "user-outlook"),
        )
        .unwrap();
        let outlook = cas.import_current(None).unwrap();

        assert_ne!(gmail.id, outlook.id);
        let accounts = cas.list_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        let users: HashSet<_> = accounts
            .iter()
            .filter_map(|account| account.user_id.as_deref())
            .collect();
        assert!(users.contains("user-gmail"));
        assert!(users.contains("user-outlook"));
    }

    #[test]
    fn registry_legacy_subject_user_id_migrates_to_chatgpt_user_id() {
        let (_app, _codex, cas) = cas();
        let id = Uuid::new_v4().to_string();
        cas.paths.ensure_account_dir(&id).unwrap();
        std::fs::write(
            cas.paths.account_auth_path(&id),
            auth_with_legacy_subject(
                "legacy@example.com",
                "workspace-legacy",
                "auth0|legacy-subject",
                "user-official",
            ),
        )
        .unwrap();
        let registry = Registry {
            accounts: vec![AccountRecord {
                id: id.clone(),
                alias: None,
                email: Some("legacy@example.com".into()),
                account_id: Some("workspace-legacy".into()),
                user_id: Some("auth0|legacy-subject".into()),
                created_at: 1,
                updated_at: 1,
                last_activated_at: None,
                last_status: None,
            }],
            ..Registry::default()
        };
        cas.save_registry(&registry).unwrap();

        let accounts = cas.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].user_id.as_deref(), Some("user-official"));
        assert_eq!(
            cas.load_registry().unwrap().accounts[0].user_id.as_deref(),
            Some("user-official")
        );
    }

    #[test]
    fn importing_older_credential_never_downgrades_saved_token() {
        let (_app, _codex, cas) = cas();
        let fixtures = tempfile::tempdir().unwrap();
        let newer = auth_at("fresh@example.com", "acc-fresh", "2099-01-02T00:00:00Z");
        let older = auth_at("fresh@example.com", "acc-fresh", "2099-01-01T00:00:00Z");
        let newer_path = fixtures.path().join("newer.json");
        let older_path = fixtures.path().join("older.json");
        std::fs::write(&newer_path, &newer).unwrap();
        std::fs::write(&older_path, &older).unwrap();

        let account = cas.input(Some(&newer_path)).unwrap();
        let same = cas.input(Some(&older_path)).unwrap();
        assert_eq!(account.id, same.id);
        assert_eq!(
            std::fs::read(cas.paths.account_auth_path(&account.id)).unwrap(),
            newer
        );
    }

    #[test]
    fn historical_duplicate_slots_collapse_to_newest_credential() {
        let (_app, _codex, cas) = cas();
        let keep_id = Uuid::new_v4().to_string();
        let drop_id = Uuid::new_v4().to_string();
        let old = auth_at(
            "duplicate@example.com",
            "acc-duplicate",
            "2099-01-01T00:00:00Z",
        );
        let new = auth_at(
            "duplicate@example.com",
            "acc-duplicate",
            "2099-01-03T00:00:00Z",
        );
        cas.paths.ensure_account_dir(&keep_id).unwrap();
        cas.paths.ensure_account_dir(&drop_id).unwrap();
        std::fs::write(cas.paths.account_auth_path(&keep_id), &old).unwrap();
        std::fs::write(cas.paths.account_auth_path(&drop_id), &new).unwrap();

        let registry = Registry {
            active_account_id: Some(keep_id.clone()),
            accounts: vec![
                AccountRecord {
                    id: keep_id.clone(),
                    alias: Some("primary".into()),
                    email: Some("duplicate@example.com".into()),
                    account_id: Some("acc-duplicate".into()),
                    user_id: None,
                    created_at: 1,
                    updated_at: 1,
                    last_activated_at: Some(1),
                    last_status: None,
                },
                AccountRecord {
                    id: drop_id.clone(),
                    alias: None,
                    email: Some("duplicate@example.com".into()),
                    account_id: Some("acc-duplicate".into()),
                    user_id: None,
                    created_at: 2,
                    updated_at: 2,
                    last_activated_at: Some(2),
                    last_status: None,
                },
            ],
            ..Registry::default()
        };
        cas.save_registry(&registry).unwrap();

        let accounts = cas.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, keep_id);
        assert_eq!(accounts[0].alias.as_deref(), Some("primary"));
        assert_eq!(
            std::fs::read(cas.paths.account_auth_path(&keep_id)).unwrap(),
            new
        );
        assert!(!cas.paths.account_dir(&drop_id).exists());
        assert_eq!(
            cas.load_registry().unwrap().active_account_id.as_deref(),
            Some(keep_id.as_str())
        );
    }

    #[test]
    fn email_prefix_must_match_from_start_and_be_unique() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("ABuckle7296@outlook.com", "acc-a"),
        )
        .unwrap();
        cas.import_current(None).unwrap();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("AloofBuckle7296@outlook.com", "acc-b"),
        )
        .unwrap();
        cas.import_current(None).unwrap();
        let registry = cas.load_registry().unwrap();

        assert!(matches!(
            resolve_email_prefix(&registry, "A"),
            Err(CasError::AmbiguousAccount { .. })
        ));
        assert_eq!(
            resolve_email_prefix(&registry, "AB")
                .unwrap()
                .email
                .as_deref(),
            Some("ABuckle7296@outlook.com")
        );
        assert_eq!(
            resolve_email_prefix(&registry, "Al")
                .unwrap()
                .email
                .as_deref(),
            Some("AloofBuckle7296@outlook.com")
        );
        assert!(matches!(
            resolve_email_prefix(&registry, "fB"),
            Err(CasError::AccountNotFound(_))
        ));
    }

    #[test]
    fn remove_selector_requires_full_email() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("alpha@example.com", "acc-a"),
        )
        .unwrap();
        cas.import_current(None).unwrap();
        let registry = cas.load_registry().unwrap();
        assert!(resolve_email_exact(&registry, "alp").is_err());
        assert!(resolve_email_exact(&registry, "ALPHA@example.com").is_ok());
    }

    fn auth_with_plan(email: &str, account_id: &str, plan: &str) -> Vec<u8> {
        let mut root: serde_json::Value = serde_json::from_slice(&auth(email, account_id)).unwrap();
        root["tokens"]["id_token"]["https://api.openai.com/auth"] =
            serde_json::json!({"chatgpt_plan_type": plan});
        serde_json::to_vec(&root).unwrap()
    }

    #[test]
    fn current_auth_plan_is_read_from_the_active_token() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth_with_plan("active@example.com", "workspace-active", "team"),
        )
        .unwrap();
        let saved = cas.import_current(None).unwrap();
        let current = cas.current_account().unwrap();
        assert!(current.managed);
        assert_eq!(current.account.as_ref().unwrap().id, saved.id);
        assert_eq!(current.plan_type.as_deref(), Some("business"));
        assert_eq!(
            cas.account_choices().unwrap()[0].auth_type.as_deref(),
            Some("business")
        );
    }

    #[test]
    fn active_removal_requires_explicit_path_then_clears_active_and_saved_auth() {
        use std::cell::Cell;

        let (_app, codex, cas) = cas();
        let active_auth = auth_with_plan("active@example.com", "workspace-active", "team");
        std::fs::write(codex.path().join("auth.json"), &active_auth).unwrap();
        let active = cas.import_current(None).unwrap();

        let other_file = codex.path().join("other-auth.json");
        std::fs::write(&other_file, auth("other@example.com", "workspace-other")).unwrap();
        let inactive = cas.input(Some(&other_file)).unwrap();
        assert!(matches!(
            cas.remove_id(&active.id),
            Err(CasError::ActiveAccountRemoval)
        ));

        // A privileged delete may only target the *actual* active identity,
        // not another account (even if the caller deliberately passes its ID).
        assert!(matches!(
            cas.remove_active_id_with_shutdown(
                &inactive.id,
                || panic!("must not shut down processes for a non-active target"),
                || Ok(())
            ),
            Err(CasError::Verification(_))
        ));
        assert!(cas.paths.codex_auth_path.exists());

        let shutdowns = Cell::new(0);
        let verifications = Cell::new(0);
        let result = cas
            .remove_active_id_with_shutdown(
                &active.id,
                || {
                    shutdowns.set(shutdowns.get() + 1);
                    Ok(vec![ProcessInfo {
                        pid: 1234,
                        name: "codex".into(),
                    }])
                },
                || {
                    verifications.set(verifications.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(result.account.id, active.id);
        assert_eq!(result.terminated_processes.len(), 1);
        assert_eq!(shutdowns.get(), 1);
        assert_eq!(verifications.get(), 2);
        assert!(!cas.paths.codex_auth_path.exists());
        assert!(!cas.paths.account_dir(&active.id).exists());
        assert!(cas.paths.account_auth_path(&inactive.id).exists());
        assert!(cas.load_registry().unwrap().active_account_id.is_none());
        assert_eq!(cas.list_accounts().unwrap().len(), 1);
        assert_eq!(cas.current_account().unwrap().identity, None);
    }

    #[test]
    fn active_removal_does_not_modify_credentials_when_shutdown_fails() {
        let (_app, codex, cas) = cas();
        let auth_bytes = auth("active@example.com", "workspace-active");
        std::fs::write(codex.path().join("auth.json"), &auth_bytes).unwrap();
        let saved = cas.import_current(None).unwrap();
        assert!(matches!(
            cas.remove_active_id_with_shutdown(
                &saved.id,
                || Err(CasError::CodexStillRunning(vec![ProcessInfo {
                    pid: 4242,
                    name: "codex".into(),
                }])),
                || panic!("shutdown failed; no later check is allowed")
            ),
            Err(CasError::CodexStillRunning(_))
        ));
        assert_eq!(
            std::fs::read(&cas.paths.codex_auth_path).unwrap(),
            auth_bytes
        );
        assert!(cas.paths.account_auth_path(&saved.id).exists());
        assert_eq!(
            cas.load_registry().unwrap().active_account_id,
            Some(saved.id)
        );
    }

    #[test]
    fn active_removal_preflights_storage_and_rechecks_auth_after_shutdown() {
        let (_app, codex, cas) = cas();
        let auth_bytes = auth("active@example.com", "workspace-active");
        std::fs::write(codex.path().join("auth.json"), &auth_bytes).unwrap();
        let saved = cas.import_current(None).unwrap();

        let unexpected = cas.paths.account_dir(&saved.id).join("unexpected");
        std::fs::write(&unexpected, b"keep").unwrap();
        assert!(matches!(
            cas.remove_active_id_with_shutdown(
                &saved.id,
                || panic!("invalid account directory must fail before shutdown"),
                || Ok(())
            ),
            Err(CasError::Verification(message)) if message.contains("unexpected")
        ));
        assert!(unexpected.exists());
        std::fs::remove_file(&unexpected).unwrap();

        // Simulate a credential being switched to another account during the
        // shutdown; the post-shutdown identity recheck must prevent deletion.
        let other = auth("other@example.com", "workspace-other");
        assert!(matches!(
            cas.remove_active_id_with_shutdown(
                &saved.id,
                || {
                    std::fs::write(&cas.paths.codex_auth_path, &other)?;
                    Ok(Vec::new())
                },
                || Ok(())
            ),
            Err(CasError::Verification(message)) if message.contains("during Codex shutdown")
        ));
        assert_eq!(std::fs::read(&cas.paths.codex_auth_path).unwrap(), other);
        assert!(cas.paths.account_auth_path(&saved.id).exists());
        assert!(
            cas.load_registry()
                .unwrap()
                .accounts
                .iter()
                .any(|a| a.id == saved.id)
        );
    }

    #[test]
    fn registry_rejects_path_like_account_ids() {
        let (_app, _codex, cas) = cas();
        let mut registry = Registry::default();
        registry.accounts.push(AccountRecord {
            id: "../../..".into(),
            alias: None,
            email: Some("escape@example.com".into()),
            account_id: Some("acc-escape".into()),
            user_id: None,
            created_at: 0,
            updated_at: 0,
            last_activated_at: None,
            last_status: None,
        });
        atomic_write_json(&cas.paths.registry_path, &registry).unwrap();
        assert!(matches!(
            cas.load_registry(),
            Err(CasError::Verification(message)) if message.contains("invalid account id")
        ));
    }

    #[test]
    fn remove_refuses_symlink_account_storage() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("alpha@example.com", "acc-a"),
        )
        .unwrap();
        let account = cas.import_current(None).unwrap();

        std::fs::write(
            codex.path().join("auth.json"),
            auth("beta@example.com", "acc-b"),
        )
        .unwrap();
        cas.import_current(None).unwrap();

        let account_dir = cas.paths.account_dir(&account.id);
        std::fs::remove_file(account_dir.join("auth.json")).unwrap();
        std::fs::remove_dir(&account_dir).unwrap();

        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("sentinel"), b"keep").unwrap();
        std::os::unix::fs::symlink(victim.path(), &account_dir).unwrap();
        assert!(matches!(
            cas.remove("alpha@example.com"),
            Err(CasError::Verification(message)) if message.contains("symlink")
        ));
        assert_eq!(
            std::fs::read(victim.path().join("sentinel")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn remove_refuses_unexpected_account_directory_contents() {
        let (_app, codex, cas) = cas();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("alpha@example.com", "acc-a"),
        )
        .unwrap();
        let account = cas.import_current(None).unwrap();
        std::fs::write(
            codex.path().join("auth.json"),
            auth("beta@example.com", "acc-b"),
        )
        .unwrap();
        cas.import_current(None).unwrap();

        let extra = cas.paths.account_dir(&account.id).join("unexpected");
        std::fs::write(&extra, b"keep").unwrap();
        assert!(matches!(
            cas.remove("alpha@example.com"),
            Err(CasError::Verification(message)) if message.contains("unexpected entry")
        ));
        assert_eq!(std::fs::read(extra).unwrap(), b"keep");
    }
}
