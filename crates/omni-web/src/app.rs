//! Router and application shell.

use leptos::prelude::*;
use omni_web_kit::components::{ButtonLink, ButtonVariant, Glyph, Icon, IconSize};
use omni_web_kit::live::provide_live_data;
use omni_web_kit::router::{Link, provide_router};
use omni_web_pages::{
    CalendarPage, ClaudePage, DeliveriesPage, FeedbackPage, GatewayPage, McpPage, MediaDetailPage,
    MediaPage, PetsPage, PodcastDetailPage, PodcastsPage, PodsDetailPage, PodsPage, RemindersPage,
    StreamerConfigPage, StylistPage,
};
use wasm_bindgen::JsCast as _;

use crate::pages::{
    CostsPage, DataPage, EmailActivityPage, HomePage, LivePage, LivestreamIntelligencePage,
    OperationsPage, StreamerPage,
};
use crate::routes::{Route, match_route, normalize_path, page_title};
use crate::shell::{Shell, ShellContext};

const MAIN_DESTINATIONS: [(&str, &str, Icon); 5] = [
    ("Live", "/live", Icon::Live),
    ("Movies & TV", "/media", Icon::Film),
    ("Podcasts", "/podcasts", Icon::Headphones),
    ("Email", "/emails", Icon::Mail),
    ("Operations", "/operations", Icon::Pulse),
];

#[component]
fn NotFound(path: Memo<String>) -> impl IntoView {
    let shell = use_context::<ShellContext>();
    view! {
        <div class="not-found-page">
            <h1 class="page-title">"Nothing at " <span class="mono">{move || path.get()}</span></h1>
            <p class="lede">"This link does not lead to a page in Omni."</p>
            <div class="cluster">
                <ButtonLink to="/" variant=ButtonVariant::Primary icon=Icon::Home>"Home"</ButtonLink>
                {shell.map(|shell| view! {
                    <button type="button" class="btn" on:click=move |_| shell.palette_open.set(true)>
                        <Glyph icon=Icon::Search size=IconSize::Medium/>
                        "Search"
                        <kbd class="kbd">"⌘K"</kbd>
                    </button>
                })}
            </div>
            <div class="panel">
                <div class="rows">
                    {MAIN_DESTINATIONS
                        .iter()
                        .map(|(label, href, icon)| view! {
                            <Link to=*href class="row">
                                <Glyph icon=*icon/>
                                <span class="row-main"><span class="row-title">{*label}</span></span>
                                <span class="row-end"><Glyph icon=Icon::ChevronRight size=IconSize::Small/></span>
                            </Link>
                        })
                        .collect_view()}
                </div>
            </div>
        </div>
    }
}

fn render(route: Route, path: Memo<String>) -> AnyView {
    match route {
        Route::Home => view! { <HomePage/> }.into_any(),
        Route::Live => view! { <LivePage/> }.into_any(),
        Route::StreamerConfig => view! { <StreamerConfigPage/> }.into_any(),
        Route::Media => view! { <MediaPage/> }.into_any(),
        Route::MediaDetail(id) => view! { <MediaDetailPage id/> }.into_any(),
        Route::Podcasts => view! { <PodcastsPage/> }.into_any(),
        Route::PodcastDetail(id) => view! { <PodcastDetailPage id/> }.into_any(),
        Route::Feedback(kind, id) => view! { <FeedbackPage kind id/> }.into_any(),
        Route::Pods => view! { <PodsPage/> }.into_any(),
        Route::PodsDetail(id) => view! { <PodsDetailPage id/> }.into_any(),
        Route::Streamer(streamer_id) => view! { <StreamerPage streamer_id/> }.into_any(),
        Route::StreamerIntelligence(streamer_id) => {
            view! { <LivestreamIntelligencePage streamer_id/> }.into_any()
        }
        Route::Emails => view! { <EmailActivityPage/> }.into_any(),
        Route::Data => view! { <DataPage/> }.into_any(),
        Route::Costs => view! { <CostsPage/> }.into_any(),
        Route::Operations => view! { <OperationsPage/> }.into_any(),
        Route::Reminders => view! { <RemindersPage/> }.into_any(),
        Route::Pets => view! { <PetsPage/> }.into_any(),
        Route::Deliveries => view! { <DeliveriesPage/> }.into_any(),
        Route::Calendar => view! { <CalendarPage/> }.into_any(),
        Route::Stylist => view! { <StylistPage/> }.into_any(),
        Route::Mcp => view! { <McpPage/> }.into_any(),
        Route::Gateway => view! { <GatewayPage/> }.into_any(),
        Route::Claude => view! { <ClaudePage/> }.into_any(),
        Route::NotFound => view! { <NotFound path/> }.into_any(),
    }
}

#[component]
pub fn App() -> impl IntoView {
    let raw_path = provide_router();
    provide_live_data();
    let path = Memo::new(move |_| normalize_path(&raw_path.get()));
    let route = Memo::new(move |_| path.with(|p| match_route(p)));

    // Move focus to the page on navigation (not on first load).
    Effect::new(move |previous: Option<String>| {
        let current = path.get();
        if previous.as_ref().is_some_and(|p| *p != current)
            && let Some(main) = document()
                .get_element_by_id("main-content")
                .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let options = web_sys::FocusOptions::new();
            options.set_prevent_scroll(true);
            let _ = main.focus_with_options(&options);
        }
        current
    });
    Effect::new(move |_| document().set_title(&path.with(|p| page_title(p))));

    view! {
        <Shell route path>
            {move || render(route.get(), path)}
        </Shell>
    }
}
