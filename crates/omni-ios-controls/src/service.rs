//! Control and widget state and their APNs delivery, plus the app's device
//! registry and widget timelines.
//!
//! Control registrations whose slot state hash differs from
//! `lastDeliveredHash` are pushed, and so are widget push tokens whose On Air
//! hash differs; a transient failure is retried once (250 ms) and otherwise
//! carried to the next live-check tick; a permanent rejection is not retried
//! for the same state until the app re-registers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::join_all;
use omni_api::ios::{
    ApnsEnvironment, IOS_CONTROL_SLOT_COUNT, IosDiagnostics, LiveSlotState, OnAirResponse,
};
use omni_core::clock::SharedClock;
use omni_live::Roster;
use omni_live::error::LiveError;
use omni_store::Store;

use crate::apns::{ApnsPush, ApnsPushResult, ApnsSender};
use crate::devices::{
    DeviceInput, DeviceWriteError, IosDevice, IosWidgetPush, delete_widget_push, list_devices,
    list_widget_pushes, mark_widget_delivered, upsert_device, upsert_widget_push,
};
use crate::persistence::{
    ControlInput, RegistrationStoreError, delete_registration, list_registrations, mark_delivered,
    replace_device_registrations,
};
use crate::slots::{build_live_control_slots, live_control_slot_hash};
use crate::widgets::{build_on_air, on_air_hash};

const LOG: &str = "IOSControls";
const RETRY_DELAY: Duration = Duration::from_millis(250);
/// Widget pushes are stored by APNs for at most an hour.
const WIDGET_PUSH_TTL_SECS: i64 = 60 * 60;

/// A control-state operation failed.
#[derive(Debug, thiserror::Error)]
pub enum IosControlError {
    #[error(transparent)]
    Status(#[from] LiveError),
    #[error(transparent)]
    Registrations(#[from] RegistrationStoreError),
}

/// The permanent-failure key prefix of a device's widget push token.
fn widget_failure_id(device_id: &str) -> String {
    format!("widget:{device_id}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushOutcome {
    Delivered,
    Permanent,
    Transient,
}

enum Attempt {
    Done(PushOutcome),
    Transient(String),
}

pub struct IosControlService {
    store: Store,
    roster: Roster,
    home_url: String,
    clock: SharedClock,
    apns: Option<Arc<dyn ApnsSender>>,
    permanent_failures: Mutex<HashSet<String>>,
    last_reconciled_at: Mutex<Option<i64>>,
}

impl IosControlService {
    pub fn new(
        store: Store,
        roster: Roster,
        home_url: impl Into<String>,
        clock: SharedClock,
        apns: Option<Arc<dyn ApnsSender>>,
    ) -> Self {
        Self {
            store,
            roster,
            home_url: home_url.into(),
            clock,
            apns,
            permanent_failures: Mutex::default(),
            last_reconciled_at: Mutex::default(),
        }
    }

    pub fn apns_enabled(&self) -> bool {
        self.apns.is_some()
    }

    /// The APNs sender, when configured.
    pub fn apns(&self) -> Option<Arc<dyn ApnsSender>> {
        self.apns.clone()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    async fn slots(&self) -> Result<Vec<LiveSlotState>, LiveError> {
        build_live_control_slots(
            &self.store,
            &self.roster.snapshot(),
            &self.home_url,
            self.clock.now_ms(),
        )
        .await
    }

    /// One slot (1-based), or `None` out of range.
    pub async fn get_slot(&self, slot: f64) -> Result<Option<LiveSlotState>, IosControlError> {
        if slot.fract() != 0.0 || !(1.0..=f64::from(IOS_CONTROL_SLOT_COUNT)).contains(&slot) {
            return Ok(None);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = slot as usize - 1;
        Ok(self.slots().await?.into_iter().nth(index))
    }

    pub async fn diagnostics(&self) -> Result<IosDiagnostics, IosControlError> {
        let registrations = list_registrations(&self.store).await?;
        let hashes = self.desired_hashes().await?;
        let undelivered = registrations
            .iter()
            .filter(|row| {
                hashes
                    .get(&row.slot)
                    .is_some_and(|hash| row.last_delivered_hash.as_deref() != Some(hash))
            })
            .count();
        let devices = list_devices(&self.store).await?;
        let widget_pushes = list_widget_pushes(&self.store).await?;
        Ok(IosDiagnostics {
            apns_enabled: self.apns.is_some(),
            registration_count: registrations.len() as u64,
            undelivered_count: undelivered as u64,
            last_reconciled_at: *self.lock_reconciled(),
            device_count: devices.len() as u64,
            alert_device_count: devices.iter().filter(|d| d.receives_alerts()).count() as u64,
            widget_push_count: widget_pushes.len() as u64,
        })
    }

    /// Registers or updates an app device for alert pushes.
    pub async fn register_alert_device(
        &self,
        input: DeviceInput,
    ) -> Result<IosDevice, DeviceWriteError> {
        upsert_device(&self.store, input).await
    }

    /// Registers a device's widget push token; a re-registration is also an
    /// operator retry after a permanent APNs error.
    pub async fn register_widget_push(
        &self,
        device_id: &str,
        push_token: &str,
        environment: ApnsEnvironment,
    ) -> Result<IosWidgetPush, DeviceWriteError> {
        let row = upsert_widget_push(&self.store, device_id, push_token, environment).await?;
        self.clear_permanent_failures(&widget_failure_id(device_id));
        Ok(row)
    }

    /// The On Air widget timeline. Missing avatars only leave `avatarUrl` null.
    pub async fn on_air(&self) -> Result<OnAirResponse, IosControlError> {
        let streamers = self.roster.snapshot();
        let avatars = match omni_live::avatars::avatar_urls(&self.store, &streamers).await {
            Ok(avatars) => avatars,
            Err(error) => {
                tracing::warn!(target: LOG, "On Air avatars unavailable: {error}");
                HashMap::new()
            }
        };
        Ok(build_on_air(&self.store, &streamers, &avatars, self.clock.now_ms()).await?)
    }

    /// Pushes `widgets` to every widget push token whose On Air hash changed.
    pub async fn reconcile_widgets(&self) -> Result<(), IosControlError> {
        let Some(apns) = self.apns.clone() else {
            return Ok(());
        };
        let rows = list_widget_pushes(&self.store).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let on_air = build_on_air(
            &self.store,
            &self.roster.snapshot(),
            &HashMap::new(),
            self.clock.now_ms(),
        )
        .await?;
        let hash = on_air_hash(&on_air);
        let pushes = rows
            .into_iter()
            .filter(|row| row.last_delivered_hash.as_deref() != Some(hash.as_str()))
            .map(|row| {
                let (apns, hash) = (apns.clone(), hash.clone());
                async move {
                    let failure_key = format!("{}:{hash}", widget_failure_id(&row.device_id));
                    if self.failures().contains(&failure_key) {
                        return Ok(());
                    }
                    match self.push_widget(apns.as_ref(), &row, &hash).await? {
                        PushOutcome::Permanent => {
                            self.failures().insert(failure_key);
                        }
                        PushOutcome::Delivered => {
                            self.clear_permanent_failures(&widget_failure_id(&row.device_id));
                        }
                        PushOutcome::Transient => {}
                    }
                    Ok::<(), IosControlError>(())
                }
            });
        for outcome in join_all(pushes).await {
            outcome?;
        }
        Ok(())
    }

    async fn push_widget(
        &self,
        apns: &dyn ApnsSender,
        row: &IosWidgetPush,
        hash: &str,
    ) -> Result<PushOutcome, IosControlError> {
        let expiration = self.clock.now_ms().div_euclid(1_000) + WIDGET_PUSH_TTL_SECS;
        let push = ApnsPush::widgets(&row.push_token, row.environment, expiration);
        let mut last_error = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match apns.send_push(&push).await {
                Ok(ApnsPushResult::Sent) => {
                    mark_widget_delivered(&self.store, &row.device_id, &row.push_token, hash)
                        .await?;
                    return Ok(PushOutcome::Delivered);
                }
                Ok(ApnsPushResult::InvalidToken { reason }) => {
                    delete_widget_push(&self.store, &row.device_id, &row.push_token).await?;
                    tracing::info!(target: LOG, "Removed stale widget push token: {reason}");
                    return Ok(PushOutcome::Delivered);
                }
                Ok(ApnsPushResult::Failed { status, reason }) => {
                    let message = format!("Widget push failed ({status}): {reason}");
                    if !(status == 0 || status == 429 || status >= 500) {
                        tracing::warn!(target: LOG, "{message}");
                        return Ok(PushOutcome::Permanent);
                    }
                    last_error = message;
                }
                Err(error) => last_error = error.to_string(),
            }
        }
        tracing::warn!(target: LOG, "{last_error}");
        Ok(PushOutcome::Transient)
    }

    fn lock_reconciled(&self) -> std::sync::MutexGuard<'_, Option<i64>> {
        self.last_reconciled_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn failures(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.permanent_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces the device's set; an explicit sync is also an operator retry
    /// after a permanent APNs error.
    pub async fn register_device(
        &self,
        device_id: &str,
        controls: Vec<ControlInput>,
    ) -> Result<(), IosControlError> {
        let previous: Vec<String> = list_registrations(&self.store)
            .await?
            .into_iter()
            .filter(|row| row.device_id == device_id)
            .map(|row| row.registration_id)
            .collect();
        let rows = replace_device_registrations(&self.store, device_id, controls).await?;
        for id in &previous {
            if !rows.iter().any(|row| &row.registration_id == id) {
                self.clear_permanent_failures(id);
            }
        }
        if self.apns.is_none() {
            return Ok(());
        }
        for row in &rows {
            self.clear_permanent_failures(&row.registration_id);
        }
        let pending = self.undelivered(None).await?;
        self.deliver(pending).await
    }

    /// Pushes every registration whose slot state changed.
    pub async fn reconcile(&self) -> Result<(), IosControlError> {
        let slots = self.slots().await?;
        *self.lock_reconciled() = Some(self.clock.now_ms());
        if self.apns.is_none() {
            return Ok(());
        }
        let hashes = slots
            .iter()
            .map(|slot| (slot.slot, live_control_slot_hash(slot)))
            .collect();
        let pending = self.undelivered(Some(hashes)).await?;
        self.deliver(pending).await
    }

    async fn desired_hashes(&self) -> Result<HashMap<u8, String>, LiveError> {
        Ok(self
            .slots()
            .await?
            .iter()
            .map(|slot| (slot.slot, live_control_slot_hash(slot)))
            .collect())
    }

    async fn undelivered(
        &self,
        hashes: Option<HashMap<u8, String>>,
    ) -> Result<Vec<(String, String)>, IosControlError> {
        let desired = match hashes {
            Some(hashes) => hashes,
            None => self.desired_hashes().await?,
        };
        Ok(list_registrations(&self.store)
            .await?
            .into_iter()
            .filter_map(|row| {
                let hash = desired.get(&row.slot)?;
                (row.last_delivered_hash.as_deref() != Some(hash.as_str()))
                    .then(|| (row.registration_id, hash.clone()))
            })
            .collect())
    }

    async fn deliver(&self, pending: Vec<(String, String)>) -> Result<(), IosControlError> {
        let pushes = pending.into_iter().map(|(id, hash)| async move {
            let failure_key = format!("{id}:{hash}");
            if self.failures().contains(&failure_key) {
                return Ok(());
            }
            match self.push(&id, &hash).await? {
                PushOutcome::Permanent => {
                    self.failures().insert(failure_key);
                }
                PushOutcome::Delivered => self.clear_permanent_failures(&id),
                PushOutcome::Transient => {}
            }
            Ok::<(), IosControlError>(())
        });
        for outcome in join_all(pushes).await {
            outcome?;
        }
        Ok(())
    }

    fn clear_permanent_failures(&self, registration_id: &str) {
        let prefix = format!("{registration_id}:");
        self.failures().retain(|key| !key.starts_with(&prefix));
    }

    async fn push(
        &self,
        registration_id: &str,
        hash: &str,
    ) -> Result<PushOutcome, IosControlError> {
        let mut last_error = None;
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match self.attempt(registration_id, hash).await {
                Ok(Attempt::Done(outcome)) => return Ok(outcome),
                Ok(Attempt::Transient(message)) => last_error = Some(Ok(message)),
                Err(error) => last_error = Some(Err(error)),
            }
        }
        match last_error {
            Some(Err(error)) => Err(error),
            Some(Ok(message)) => {
                tracing::warn!(target: LOG, "{message}");
                Ok(PushOutcome::Transient)
            }
            None => Ok(PushOutcome::Transient),
        }
    }

    async fn attempt(&self, registration_id: &str, hash: &str) -> Result<Attempt, IosControlError> {
        let registration = list_registrations(&self.store)
            .await?
            .into_iter()
            .find(|row| row.registration_id == registration_id);
        let (Some(registration), Some(apns)) = (registration, self.apns.as_ref()) else {
            return Ok(Attempt::Done(PushOutcome::Delivered));
        };
        let result = match apns.send_control_changed(&registration).await {
            Ok(result) => result,
            Err(error) => return Ok(Attempt::Transient(error.to_string())),
        };
        match result {
            ApnsPushResult::Sent => {
                mark_delivered(
                    &self.store,
                    &registration.registration_id,
                    &registration.push_token,
                    hash,
                )
                .await?;
                Ok(Attempt::Done(PushOutcome::Delivered))
            }
            ApnsPushResult::InvalidToken { reason } => {
                delete_registration(
                    &self.store,
                    &registration.registration_id,
                    &registration.push_token,
                )
                .await?;
                tracing::info!(target: LOG, "Removed stale control token: {reason}");
                Ok(Attempt::Done(PushOutcome::Delivered))
            }
            ApnsPushResult::Failed { status, reason } => {
                let message = format!("Control push failed ({status}): {reason}");
                if status == 0 || status == 429 || status >= 500 {
                    return Ok(Attempt::Transient(message));
                }
                tracing::warn!(target: LOG, "{message}");
                Ok(Attempt::Done(PushOutcome::Permanent))
            }
        }
    }
}
