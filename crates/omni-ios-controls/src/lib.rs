//! The Omni iOS app backend: signed routes for the app and its extensions,
//! the device registry and APNs alert route that `omni_alerts::Pushover`
//! tries before Pushover, the four live control slots, the On Air widget
//! timeline, and APNs `controls` and `widgets` pushes
//! reconciled after every live-check tick. See `docs/ios-app.md`.

pub mod alerts;
pub mod apns;
pub mod devices;
pub mod persistence;
pub mod routes;
pub mod service;
pub mod slots;
pub mod widgets;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_live::{Roster, TickHook};
use omni_runtime::{AppContext, Subsystem};
use omni_store::entity::EntityDescriptor;

use crate::alerts::AppAlertRoute;
use crate::apns::{ApnsConfig, ApnsControlClient, ApnsSender};
use crate::devices::{IosDevice, IosWidgetPush};
use crate::persistence::IosControlRegistration;
use crate::routes::IosRoutesState;
use crate::service::IosControlService;

const LOG: &str = "Main";

/// The iOS control service and its routes.
pub struct IosControls {
    service: Arc<IosControlService>,
    state: IosRoutesState,
}

impl IosControls {
    /// Builds the service; APNs pushes need team ID, key ID, key path and the
    /// auth token, and an unreadable key only disables pushes.
    pub async fn new(ctx: &AppContext, roster: Roster) -> Self {
        let config = &ctx.config;
        let values = [
            &config.ios_control_apns_team_id,
            &config.ios_control_apns_key_id,
            &config.ios_control_apns_key_path,
        ];
        let any = values
            .iter()
            .any(|v| v.as_deref().is_some_and(|s| !s.is_empty()));
        let complete = values
            .iter()
            .all(|v| v.as_deref().is_some_and(|s| !s.is_empty()));
        if any && !complete {
            tracing::warn!(
                target: LOG,
                "iOS control APNs pushes disabled: team ID, key ID, and key path must all be set"
            );
        }
        let auth_token = config
            .ios_control_auth_token
            .clone()
            .filter(|t| !t.is_empty());
        let mut apns: Option<Arc<dyn ApnsSender>> = None;
        if complete && auth_token.is_some() {
            let apns_config = ApnsConfig {
                team_id: config.ios_control_apns_team_id.clone().unwrap_or_default(),
                key_id: config.ios_control_apns_key_id.clone().unwrap_or_default(),
                bundle_id: config.ios_control_bundle_id.clone(),
                private_key_path: config.ios_control_apns_key_path.clone().unwrap_or_default(),
            };
            match ApnsControlClient::create(
                apns_config,
                ctx.http.clone(),
                ctx.clock.clone(),
                ctx.side_effects,
            )
            .await
            {
                Ok(client) => apns = Some(Arc::new(client)),
                Err(error) => tracing::warn!(
                    target: LOG,
                    "iOS control APNs pushes disabled: failed to load signing key ({error})"
                ),
            }
        }
        if complete && auth_token.is_none() {
            tracing::warn!(target: LOG, "iOS control APNs pushes disabled: IOS_CONTROL_AUTH_TOKEN is not set");
        }
        let service = Arc::new(IosControlService::new(
            ctx.store.clone(),
            roster,
            config.ios_control_home_url.clone(),
            ctx.clock.clone(),
            apns,
        ));
        Self::from_service(service, auth_token, ctx.clock.clone())
    }

    /// Wraps an existing service (tests inject a fake APNs sender).
    pub fn from_service(
        service: Arc<IosControlService>,
        auth_token: Option<String>,
        clock: omni_core::clock::SharedClock,
    ) -> Self {
        let state = IosRoutesState {
            service: service.clone(),
            auth_token,
            clock,
            nonces: Arc::new(Mutex::new(HashMap::new())),
        };
        Self { service, state }
    }

    pub fn service(&self) -> Arc<IosControlService> {
        self.service.clone()
    }

    /// The app notification route for `omni_alerts::Pushover::set_app_route`.
    pub fn app_route(&self) -> Arc<dyn omni_alerts::AppPushRoute> {
        Arc::new(AppAlertRoute::new(
            self.service.store().clone(),
            self.service.apns(),
            self.service.clock(),
        ))
    }

    /// The live-check hook that reconciles controls and widgets after every tick.
    pub fn reconciler(&self) -> Arc<dyn TickHook> {
        Arc::new(Reconciler {
            service: self.service.clone(),
        })
    }

    pub fn into_subsystem(self) -> Subsystem {
        let mut subsystem = Subsystem::named("ios-controls");
        subsystem.router = routes::router(self.state);
        subsystem.entities = vec![
            EntityDescriptor::of::<IosControlRegistration>(),
            EntityDescriptor::of::<IosDevice>(),
            EntityDescriptor::of::<IosWidgetPush>(),
        ];
        subsystem
    }
}

struct Reconciler {
    service: Arc<IosControlService>,
}

impl TickHook for Reconciler {
    fn after_tick(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let controls = self.service.reconcile().await;
            let widgets = self.service.reconcile_widgets().await;
            let errors: Vec<String> = [controls, widgets]
                .into_iter()
                .filter_map(Result::err)
                .map(|e| e.to_string())
                .collect();
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        })
    }
}
