//! Static pages served by teapawt itself.

use axum::{
   Router,
   extract::State,
   http::{HeaderMap, HeaderValue, header},
   response::{Html, IntoResponse, Redirect, Response},
   routing::get,
};
use axum_extra::extract::CookieJar;
use maud::{PreEscaped, html};

use crate::{
   AppState,
   types::Prefs,
   views::{
      donate as donate_view, explore as explore_view, layout::PageLayout, search as search_view,
   },
};

fn request_host(config: &crate::config::Config) -> (String, String) {
   let origin = crate::utils::host::current_origin(config.url_prefix());
   let host = crate::utils::host::request_origin().map_or_else(
      || crate::utils::host::host_label(config.url_prefix()).to_owned(),
      |value| crate::utils::host::host_label(&value).to_owned(),
   );
   (origin, host)
}

fn home_description(host: &str) -> String {
   format!(
      "{host} is a public Nitter replacement — a privacy-focused Twitter/X frontend. nitter.net shut down; this instance is a drop-in successor."
   )
}

fn about_description(host: &str) -> String {
   format!(
      "About {host}, a public Nitter replacement. After nitter.net shut down, this teapawt instance lets you browse Twitter/X without JavaScript or tracking."
   )
}

fn home_json_ld(origin: &str, host: &str) -> String {
   let same_as: Vec<String> = crate::utils::host::APEX_HOSTS
      .iter()
      .filter(|item| **item != host)
      .map(|item| format!("https://{item}"))
      .collect();
   let value = serde_json::json!({
      "@context": "https://schema.org",
      "@type": "WebSite",
      "name": host,
      "alternateName": ["teapawt", "Nitter", "xitter"],
      "url": format!("{origin}/"),
      "description": home_description(host),
      "sameAs": same_as,
      "potentialAction": {
         "@type": "SearchAction",
         "target": {
            "@type": "EntryPoint",
            "urlTemplate": format!("{origin}/search?f=tweets&q={{search_term_string}}")
         },
         "query-input": "required name=search_term_string"
      }
   });
   value.to_string().replace('<', "\\u003c")
}

/// High-traffic public accounts linked from the homepage so crawlers discover
/// profile URLs immediately instead of waiting for the sitemap.
const HOME_PROFILES: &[&str] = &[
   "elonmusk",
   "nasa",
   "spacex",
   "openai",
   "google",
   "microsoft",
   "apple",
   "youtube",
   "bbc",
   "cnn",
   "nytimes",
   "reuters",
   "POTUS",
   "WhiteHouse",
   "UN",
   "WHO",
   "MrBeast",
   "MKBHD",
   "lexfridman",
   "natgeo",
   "espn",
   "nba",
   "nfl",
   "github",
   "rustlang",
   "wikipedia",
];

/// Wallets that keep the instance running, shown on the About page.
const DONATION_ADDRESSES: &[(&str, &str)] = &[
   ("BTC", "bc1qtm4rep7kyp2p28hlkfktpnd0j7uew47vnt2mev"),
   ("LTC", "ltc1q9xlaysse0mpfwf025esd69kt9zu3md22gh23du"),
   ("ETH", "0x6abA18D203520c8F90ca63eE7426a7bC345D98e6"),
   ("SOL", "E5ekoiauCR8yiNp451dmuPzhUxo84P5x3m9WJBK5kxZe"),
   ("XRP", "r3jYXNdMk2Vz5hXhKCKE9re8uz93JQFG1r"),
   ("DOGE", "DSBq5jRWkiwmEh1G7zBwmQDgRkvRr61fmb"),
   (
      "XMR",
      "85CTwq5wGjWZw5GgzgwZPrHyZxpLXMK5oZSSSk5j4x9w1bpqzisaWAUZYaAPfh6rP2MZJcaFqF1XmEzkw6YCqJSS2Txu6Z8",
   ),
];

/// The pages teapawt serves itself rather than fetching from X.
pub fn router() -> Router<AppState> {
   Router::new()
      .route("/", get(home))
      .route("/about", get(about))
      .route("/help", get(|| async { Redirect::to("/about") }))
}

async fn home(State(state): State<AppState>, jar: CookieJar, headers: HeaderMap) -> Response {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let firefox = headers
      .get(header::USER_AGENT)
      .and_then(|value| value.to_str().ok())
      .is_some_and(crate::views::preferences::is_firefox);
   let warning_hidden = jar
      .get("hideFirefoxWarning")
      .is_some_and(|cookie| cookie.value() == "on");
   let hub = html! {
       section class="landing-hub" aria-label="Popular profiles" {
           h2 { "Browse public profiles" }
           p { "Every public timeline is crawlable. Start with these, or search any @username." }
           ul class="landing-hub-list" {
               @for handle in HOME_PROFILES {
                   li {
                       a href=(format!("/{handle}")) { "@" (handle) }
                   }
               }
           }
           p class="landing-hub-more" {
               a href="/explore" { "Explore" }
               " · "
               a href="/about" { "About this instance" }
               " · "
               a href="/search" { "Search posts" }
               " · "
               a href="/sitemap.xml" { "Sitemap" }
           }
       }
   };
   let (trends, progress) = tokio::join!(
      super::explore::trending_preview(&state),
      crate::donations::month_progress(&state.donations, state.config.donations.goal_cents),
   );
   let preview = explore_view::render_explore_preview(&trends);
   let below = html! {
       (mirror_announcement())
       (hub)
       (preview)
   };
   let drive = Some(donate_view::donate_drive_banner(&progress));
   let firefox_note = (firefox && !prefs.proxy_media && !warning_hidden)
      .then(|| crate::views::preferences::firefox_media_warning(false));
   let notice = if drive.is_none() && firefox_note.is_none() {
      None
   } else {
      Some(html! {
          @if let Some(drive) = drive { (drive) }
          @if let Some(firefox_note) = firefox_note { (firefox_note) }
      })
   };
   let content = search_view::render_search_page(notice, Some(below));
   let (origin, host) = request_host(&state.config);
   let title = format!("{host} — Nitter replacement");
   let description = home_description(&host);
   let json_ld = home_json_ld(&origin, &host);
   let head = html! {
       meta property="og:type" content="website";
       script type="application/ld+json" {
           (PreEscaped(json_ld))
       }
   };

   let markup = PageLayout::new(&state.config, &title, content)
      .description(&description)
      .prefs(&prefs)
      .referer("/")
      .head_extra(&head)
      .render();
   let mut response = Html(markup.into_string()).into_response();
   // The warning depends on the browser and a dismiss cookie, so a shared cache
   // must not store one visitor's copy for everyone else.
   response.headers_mut().insert(
      header::CACHE_CONTROL,
      HeaderValue::from_static("private, no-store"),
   );
   response
}

fn mirror_announcement() -> maud::Markup {
   html! {
       aside class="home-announce" {
           strong { "Spare addresses" }
           p {
               "This site is also "
               a href="https://nitter.gq" { "nitter.gq" }
               ", "
               a href="https://xitter.gq" { "xitter.gq" }
               ", "
               a href="https://teapawt.net" { "teapawt.net" }
               ", "
               a href="https://teapawt.org" { "teapawt.org" }
               ", and "
               a href="https://teapaw.org" { "teapaw.org" }
               ". If a spam filter blocks nitter.cf or xitter.cf, open one of those. "
               a href="https://d420.net/" { "d420.net" }
               " lists which names are up."
           }
           p {
               "429s from X are watched. When they start, more accounts and proxy IPs go into rotation."
           }
       }
   }
}

async fn about(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let (_, host) = request_host(&state.config);
   let title = format!("About {host}");
   let description = about_description(&host);
   let mirrors: Vec<&str> = crate::utils::host::APEX_HOSTS
      .iter()
      .copied()
      .filter(|item| *item != host)
      .collect();
   let head = html! {
       meta property="og:type" content="website";
   };
   let content = html! {
       div class="overlay-panel" {
           h1 { "About " (host) }

           p {
               strong { (host) }
               " is a public Nitter replacement — a privacy-focused Twitter/X frontend. "
               "nitter.net shut down; this instance is meant as a drop-in successor. "
               "Same URL style ("
               code { "/username" }
               ", "
               code { "/username/status/id" }
               "), no JavaScript required, and Twitter never sees your IP or fingerprint. "
               "Also available at "
               @for (i, mirror) in mirrors.iter().enumerate() {
                   @if i > 0 { ", " }
                   a href=(format!("https://{mirror}/")) { (*mirror) }
               }
               "."
           }

           p {
               "This site runs "
               a href="https://github.com/4cuck/teapawt" { "teapawt" }
               ", a privacy-focused Twitter/X frontend."
           }

           ul {
               li { "No third-party JavaScript or ads" }
               li { "All requests go through the backend, client never talks to Twitter" }
               li { "Prevents Twitter from tracking your IP or JavaScript fingerprint" }
               li { "Uses Twitter's unofficial API (no developer account required)" }
               li { "Lightweight" }
               li { "RSS feeds" }
               li { "Themes" }
               li { "Mobile support (responsive design)" }
               li { "AGPLv3 licensed, no proprietary instances permitted" }
           }

           h2 { "Why use teapawt?" }

           p {
               "It's impossible to use Twitter without JavaScript enabled, and as of 2024 you need to sign up. "
               "For privacy-minded folks, preventing JavaScript analytics and IP-based tracking is important, "
               "but apart from using a VPN and uBlock/uMatrix, it's impossible. Despite being behind a VPN and "
               "using heavy-duty adblockers, you can get accurately tracked with your "
               a href="https://restoreprivacy.com/browser-fingerprinting/" { "browser's fingerprint" }
               ", "
               a href="https://noscriptfingerprint.com/" { "no JavaScript required" }
               ". This all became particularly important after Twitter "
               a href="https://www.eff.org/deeplinks/2020/04/twitter-removes-privacy-option-and-shows-why-we-need-strong-privacy-laws" { "removed the ability" }
               " for users to control whether their data gets sent to advertisers."
           }

           p {
               "Using an instance of teapawt (hosted on a VPS for example), you can browse Twitter without "
               "JavaScript while retaining your privacy. In addition to respecting your privacy, teapawt is on "
               "average around 15 times lighter than Twitter, and in most cases serves pages faster "
               "(eg. timelines load 2-4x faster)."
           }

           h2 { "Donate" }
           p {
               "Running a public instance costs money for servers, bandwidth and the accounts "
               "and exits that keep it working. "
               a href="/donate" { "Donate by card, crypto, or LZT Market" }
               ". Nothing is required."
           }
           ul class="donate-list" {
               @for &(coin, address) in DONATION_ADDRESSES {
                   li {
                       strong { (coin) }
                       code { (address) }
                   }
               }
           }

           h2 { "Instance info" }
           p {
               "Version: teapawt " (env!("CARGO_PKG_VERSION"))
           }
       }
   };

   let markup = PageLayout::new(&state.config, &title, content)
      .description(&description)
      .prefs(&prefs)
      .referer("/about")
      .head_extra(&head)
      .render();
   Html(markup.into_string())
}
