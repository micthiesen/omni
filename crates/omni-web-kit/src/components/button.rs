//! The one button family: [`Button`], [`ButtonLink`], [`ConfirmButton`] and
//! [`RunButton`].

use std::time::Duration;

use leptos::prelude::*;

use super::icon::{Glyph, Icon, IconSize};
use super::toast::{ToastKind, use_toast};
use crate::live::use_live_data;
use crate::router::Link;
use crate::task::{TaskHandle, sleep, spawn_detached};

/// Visual weight. At most one `Primary` per view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    #[default]
    Secondary,
    Ghost,
    Danger,
}

impl ButtonVariant {
    fn class(self) -> &'static str {
        match self {
            ButtonVariant::Primary => "primary",
            ButtonVariant::Secondary => "",
            ButtonVariant::Ghost => "ghost",
            ButtonVariant::Danger => "danger",
        }
    }
}

/// `Md` is 36 px (44 on phone), `Sm` 30 px (36 on phone).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonSize {
    #[default]
    Md,
    Sm,
}

fn button_class(
    variant: ButtonVariant,
    size: ButtonSize,
    icon_only: bool,
    block: bool,
    extra: &str,
) -> String {
    let mut class = String::from("btn");
    for part in [
        variant.class(),
        if size == ButtonSize::Sm { "sm" } else { "" },
        if icon_only { "icon-only" } else { "" },
        if block { "block" } else { "" },
        extra,
    ] {
        if !part.is_empty() {
            class.push(' ');
            class.push_str(part);
        }
    }
    class
}

/// A button. `disabled_reason` is required in spirit: every disabled control
/// says why (shown as the title and to screen readers).
#[component]
pub fn Button(
    #[prop(optional)] variant: ButtonVariant,
    #[prop(optional)] size: ButtonSize,
    #[prop(optional)] icon: Option<Icon>,
    /// Square icon button; `aria_label` is then required.
    #[prop(optional)]
    icon_only: bool,
    #[prop(optional)] block: bool,
    #[prop(optional)] submit: bool,
    #[prop(into, optional)] busy: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] disabled_reason: MaybeProp<String>,
    #[prop(into, optional)] title: MaybeProp<String>,
    #[prop(into, optional)] aria_label: MaybeProp<String>,
    #[prop(into, optional)] aria_pressed: MaybeProp<String>,
    #[prop(into, optional)] aria_expanded: MaybeProp<String>,
    #[prop(into, optional)] aria_controls: MaybeProp<String>,
    #[prop(into, optional)] class: MaybeProp<String>,
    #[prop(optional)] on_click: Option<Callback<web_sys::MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let class = move || {
        let mut c = button_class(
            variant,
            size,
            icon_only,
            block,
            &class.get().unwrap_or_default(),
        );
        if busy.get() {
            c.push_str(" busy");
        }
        c
    };
    let title = move || {
        if disabled.get() && !busy.get() {
            disabled_reason.get().or_else(|| title.get())
        } else {
            title.get()
        }
    };
    let icon_size = if size == ButtonSize::Sm {
        IconSize::Small
    } else {
        IconSize::Medium
    };
    let leading = move || {
        if busy.get() {
            Some(view! { <span class="spinner" aria-hidden="true"></span> }.into_any())
        } else {
            icon.map(|icon| view! { <Glyph icon size=icon_size/> }.into_any())
        }
    };
    view! {
        <button
            type=if submit { "submit" } else { "button" }
            class=class
            disabled=move || disabled.get() || busy.get()
            aria-busy=move || busy.get().then_some("true")
            title=title
            aria-label=move || aria_label.get()
            aria-pressed=move || aria_pressed.get()
            aria-expanded=move || aria_expanded.get()
            aria-controls=move || aria_controls.get()
            on:click=move |event| {
                if let Some(on_click) = on_click {
                    on_click.run(event);
                }
            }
        >
            {leading}
            {children.map(|children| children())}
        </button>
    }
}

/// A link styled as a button. `external` opens a new tab; otherwise it is a
/// client-side [`Link`].
#[component]
pub fn ButtonLink(
    #[prop(into)] to: String,
    #[prop(optional)] variant: ButtonVariant,
    #[prop(optional)] size: ButtonSize,
    #[prop(optional)] icon: Option<Icon>,
    #[prop(optional)] external: bool,
    #[prop(optional)] block: bool,
    #[prop(optional)] download: bool,
    #[prop(into, optional)] title: MaybeProp<String>,
    #[prop(into, optional)] aria_label: MaybeProp<String>,
    children: Children,
) -> impl IntoView {
    let class = button_class(variant, size, false, block, "");
    let icon_size = if size == ButtonSize::Sm {
        IconSize::Small
    } else {
        IconSize::Medium
    };
    let leading = icon.map(|icon| view! { <Glyph icon size=icon_size/> });
    if external || download {
        view! {
            <a
                class=class
                href=to
                target=external.then_some("_blank")
                rel=external.then_some("noopener")
                download=download.then_some("")
                title=move || title.get()
                aria-label=move || aria_label.get()
            >
                {leading}
                {children()}
                {external.then(|| view! { <Glyph icon=Icon::External size=IconSize::Small/> })}
            </a>
        }
        .into_any()
    } else {
        view! {
            <Link to class=class title=title aria_label=aria_label>
                {leading}
                {children()}
            </Link>
        }
        .into_any()
    }
}

const ARM_WINDOW: Duration = Duration::from_secs(3);

/// Two-step button: the first press arms it for 3 s ("Confirm …" with a
/// draining bar); the second acts. Escape, blur or the timeout disarm it.
/// Replaces every `window.confirm()`.
#[component]
pub fn ConfirmButton(
    #[prop(into)] label: Signal<String>,
    /// Armed label; defaults to "Confirm" (or "Delete permanently" when
    /// `destructive`).
    #[prop(into, optional)]
    confirm_label: MaybeProp<String>,
    #[prop(optional)] variant: ButtonVariant,
    #[prop(optional)] size: ButtonSize,
    #[prop(optional)] icon: Option<Icon>,
    #[prop(optional)] destructive: bool,
    #[prop(optional)] block: bool,
    #[prop(into, optional)] busy: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] disabled_reason: MaybeProp<String>,
    #[prop(into, optional)] title: MaybeProp<String>,
    /// Screen-reader-only words after the label naming what the button acts
    /// on, for buttons repeated on every row ("Revoke" + "token laptop").
    #[prop(into, optional)]
    context: MaybeProp<String>,
    on_confirm: Callback<()>,
) -> impl IntoView {
    let armed = RwSignal::new(false);
    let timer = StoredValue::new(None::<TaskHandle>);
    let disarm = move || {
        if let Some(Some(handle)) = timer.try_get_value() {
            handle.abort();
        }
        armed.set(false);
    };
    on_cleanup(move || {
        if let Some(Some(handle)) = timer.try_get_value() {
            handle.abort();
        }
    });
    let on_click = move |_| {
        if armed.get_untracked() {
            disarm();
            on_confirm.run(());
            return;
        }
        armed.set(true);
        let handle = spawn_detached(async move {
            sleep(ARM_WINDOW).await;
            armed.try_set(false);
        });
        timer.try_set_value(Some(handle));
    };
    let class = move || {
        let mut c = button_class(
            variant,
            size,
            false,
            block,
            if destructive {
                "confirm-destructive"
            } else {
                ""
            },
        );
        if armed.get() {
            c.push_str(if destructive {
                " armed danger"
            } else {
                " armed"
            });
        }
        if busy.get() {
            c.push_str(" busy");
        }
        c
    };
    let text = move || {
        if armed.get() {
            confirm_label.get().unwrap_or_else(|| {
                if destructive {
                    "Delete permanently".to_owned()
                } else {
                    "Confirm".to_owned()
                }
            })
        } else {
            label.get()
        }
    };
    let icon_size = if size == ButtonSize::Sm {
        IconSize::Small
    } else {
        IconSize::Medium
    };
    let leading = move || {
        if busy.get() {
            Some(view! { <span class="spinner" aria-hidden="true"></span> }.into_any())
        } else {
            icon.map(|icon| view! { <Glyph icon size=icon_size/> }.into_any())
        }
    };
    let title = move || {
        if disabled.get() && !busy.get() {
            disabled_reason.get().or_else(|| title.get())
        } else if armed.get() {
            Some("Press again to confirm".to_owned())
        } else {
            title.get()
        }
    };
    view! {
        <button
            type="button"
            class=class
            disabled=move || disabled.get() || busy.get()
            aria-busy=move || busy.get().then_some("true")
            title=title
            on:click=on_click
            on:blur=move |_| disarm()
            on:keydown=move |event: web_sys::KeyboardEvent| {
                if event.key() == "Escape" && armed.get_untracked() {
                    event.stop_propagation();
                    disarm();
                }
            }
        >
            {leading}
            <span>{text}</span>
            {move || context.get().map(|context| view! { <span class="sr-only">{format!(" {context}")}</span> })}
            <span class="sr-only" aria-live="polite">
                {move || armed.get().then_some("Press again to confirm")}
            </span>
        </button>
    }
}

/// Runs a task through [`LiveData::run_task`](crate::live::LiveData::run_task)
/// after a two-step confirm. Busy while the snapshot says it is running; a
/// 409 shows "<name> is already running" and does not flip `running`.
#[component]
pub fn RunButton(
    #[prop(into)] task: String,
    #[prop(into)] running: Signal<bool>,
    #[prop(into, optional)] label: MaybeProp<String>,
    #[prop(optional)] primary: bool,
    #[prop(optional)] size: ButtonSize,
    /// Picks count for the recommendation run routes.
    #[prop(into, optional)]
    max_recommendations: Signal<Option<u32>>,
) -> impl IntoView {
    let live = use_live_data();
    let toast = use_toast();
    let pending = RwSignal::new(false);
    let busy = Signal::derive(move || running.get() || pending.get());
    let run_label = Signal::derive(move || {
        if running.get() {
            "Running".to_owned()
        } else {
            label.get().unwrap_or_else(|| "Run".to_owned())
        }
    });
    let name = StoredValue::new(task);
    let on_confirm = Callback::new(move |()| {
        let name = name.get_value();
        let max = max_recommendations.get_untracked();
        pending.set(true);
        spawn_detached(async move {
            let result = live.run_task(name, max).await;
            pending.try_set(false);
            toast.show(
                result.message,
                if result.ok {
                    ToastKind::Info
                } else {
                    ToastKind::Error
                },
            );
        });
    });
    view! {
        <ConfirmButton
            label=run_label
            confirm_label="Confirm run"
            variant=if primary { ButtonVariant::Primary } else { ButtonVariant::Secondary }
            size
            icon=Icon::Play
            busy
            on_confirm
        />
    }
}
