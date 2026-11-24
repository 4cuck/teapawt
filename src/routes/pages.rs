//! Static pages served by teapawt itself.

use axum::{
   Router,
   extract::State,
   http::{
      HeaderMap,
      header,
   },
   response::{
      Html,
      IntoResponse,
      Redirect,
      Response,
   },
   routing::get,
};
use axum_extra::extract::CookieJar;
use maud::{
   PreEscaped,
   html,
};

use super::import::no_store;
use crate::{
   AppState,
   types::Prefs,
   views::{
      donate as donate_view,
      layout::PageLayout,
      search as search_view,
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
      "{host} is a public Nitter replacement — a privacy-focused Twitter/X frontend. nitter.net \
       shut down; this instance is a drop-in successor."
   )
}

fn about_description(host: &str) -> String {
   format!(
      "About {host}, a public Nitter replacement. After nitter.net shut down, this teapawt \
       instance lets you browse Twitter/X without JavaScript or tracking."
   )
}

fn same_apex(host: &str, domain: &str) -> bool {
   let without_port = host.split(':').next().unwrap_or(host);
   let apex = match without_port.get(..4) {
      Some(prefix) if prefix.eq_ignore_ascii_case("www.") => {
         without_port.get(4..).unwrap_or(without_port)
      },
      _ => without_port,
   };
   apex.eq_ignore_ascii_case(domain)
}

fn mirror_hosts(host: &str) -> Vec<&'static str> {
   crate::utils::host::APEX_HOSTS
      .iter()
      .copied()
      .filter(|item| !same_apex(host, item))
      .collect()
}

fn home_json_ld(origin: &str, host: &str) -> String {
   let same_as: Vec<String> = mirror_hosts(host)
      .into_iter()
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
      .route("/pool-counts", get(pool_counts))
      .route("/about", get(about))
      .route("/contact", get(contact))
      .route("/support", get(|| async { Redirect::to("/contact") }))
      .route("/help", get(|| async { Redirect::to("/about") }))
}

async fn pool_counts(State(state): State<AppState>) -> impl IntoResponse {
   use axum::{
      Json,
      http::StatusCode,
   };
   match state.accounts.stats().await {
      Ok(stats) => {
         (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "public, max-age=60")],
            Json(serde_json::json!({
               "search": stats.search,
               "search_pass": stats.search_pass,
               "view": stats.view,
               "ct0_dead": stats.ct0_dead,
               "rejected": stats.rejected,
               "total": stats.total,
            })),
         )
            .into_response()
      },
      Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
   }
}

async fn contact(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
   let prefs = Prefs::from_cookies(&jar, &state.config);
   let (_, host) = request_host(&state.config);
   let title = format!("Contact {host}");
   let description =
      format!("Support and contact details for {host}, and how donors get a search pass.");
   let price = crate::donations::format_usd(state.passes.price_cents());
   let content = crate::views::contact::render_contact(&state.config.contact, &price);
   let markup = PageLayout::new(&state.config, &title, content)
      .description(&description)
      .prefs(&prefs)
      .referer("/contact")
      .render();
   Html(markup.into_string())
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
   let pool_counts = state.accounts.stats().await.ok();
   let hub = html! {
       @if let Some(stats) = pool_counts {
           section class="pool-counter" aria-label="Account inventory" {
               span class="pool-counter-item" title="Accounts in the search pool" { (stats.search) " search" }
               @if stats.search_pass > 0 {
                   span class="pool-counter-sep" aria-hidden="true" { "·" }
                   span class="pool-counter-item" title="Accounts in the search-pass pool" { (stats.search_pass) " search-pass" }
               }
               span class="pool-counter-sep" aria-hidden="true" { "·" }
               span class="pool-counter-item" title="Accounts in the view pool" { (stats.view) " view" }
               span class="pool-counter-sep" aria-hidden="true" { "·" }
               span class="pool-counter-item pool-counter-total" { (stats.total) " accounts" }
               a class="pool-counter-link" href="/import" { "details" }
           }
       }
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
               a href="/contact" { "Contact" }
               " · "
               a href="/search" { "Search posts" }
               " · "
               a href="/sitemap.xml" { "Sitemap" }
           }
       }
   };
   let (origin, host) = request_host(&state.config);
   let progress =
      crate::donations::month_progress(&state.donations, state.config.donations.goal_cents).await;
   let below = html! {
       (mirror_announcement(&host))
       (hub)
   };
   let drive = Some(donate_view::donate_drive_banner(&progress));
   let firefox_note = (firefox && !prefs.proxy_media && !warning_hidden)
      .then(|| crate::views::preferences::firefox_media_warning(false));
   let gate = &state.config.anti_ddos;
   let flood_share = gate
      .automatic
      .then(|| state.api.pressure().flood_share(crate::pressure::now()))
      .flatten();
   let gate_active_globally = gate.enabled
      && state.config.turnstile.configured()
      && (gate.gates_everyone() || flood_share.is_some());
   let gate_note = gate_active_globally.then(|| {
      let check_minutes = (!super::anti_ddos::cleared(&state, &jar) && gate.pass_minutes > 0)
         .then_some(gate.pass_minutes);
      crate::views::anti_ddos::gate_notice(check_minutes)
   });
   let price = crate::donations::format_usd(state.passes.price_cents());
   let search_pass_note = search_pass_pool_announcement(&price);
   let search_note = state
      .config
      .turnstile
      .guards_search()
      .then(|| search_announcement(&price));
   let notice = Some(html! {
       (search_pass_note)
       @if let Some(search_note) = search_note { (search_note) }
       @if let Some(gate_note) = gate_note { (gate_note) }
       @if let Some(drive) = drive { (drive) }
       @if let Some(firefox_note) = firefox_note { (firefox_note) }
   });
   let content = search_view::render_search_page(notice, Some(below));
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
   // Notices and preferences depend on cookies, User-Agent, and gate state, so a
   // shared or CDN cache must not store one visitor's copy for everyone else.
   no_store(response.headers_mut());
   response
}

fn search_pass_pool_announcement(price: &str) -> maud::Markup {
   html! {
       aside class="home-announce" {
           strong { "Dedicated account pool for search pass holders" }
           p {
               "People who pay for a "
               a href="/search-pass" { "search pass" }
               " (" (price) " a month) now get a dedicated search-pass account pool for better "
               "response times and eventually zero rate limits. If the search-pass pool ever "
               "hits its rate limit, searches fall back to the public pool automatically."
           }
           p {
               "Right now accounts keep getting suspended on X, so keeping a separate pool makes "
               "sure paid users always have a good experience without competing with the public pool."
           }
       }
   }
}

fn search_announcement(price: &str) -> maud::Markup {
   html! {
       aside class="home-announce" {
           strong { "Search checkbox is staying" }
           p {
               "Cloudflare Turnstile on search is staying on. "
               "Another 300 accounts were added, and then someone flooded the site with search requests."
           }
           p {
               "To scrape the site, buy a "
               a href="/search-pass" { "search pass" }
               " (" (price) " a month) and use that. "
               "A normal search still gets 5 minutes after the checkbox."
           }
       }
   }
}

fn mirror_announcement(host: &str) -> maud::Markup {
   let mirrors = mirror_hosts(host);
   html! {
       aside class="home-announce" {
           strong { "Spare addresses" }
           p {
               @if !mirrors.is_empty() {
                   "This site is also "
                   @for (i, mirror) in mirrors.iter().enumerate() {
                       @if i > 0 {
                           @if i + 1 == mirrors.len() {
                               @if mirrors.len() > 2 { ", and " } @else { " and " }
                           } @else {
                               ", "
                           }
                       }
                       a href=(format!("https://{mirror}")) { (*mirror) }
                   }
                   ". If a spam filter blocks this address, open one of those. "
               }
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
   let mirrors = mirror_hosts(&host);
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

           h2 { "Contact" }
           p {
               "Support, broken pages, or a search pass for a past donation: see the "
               a href="/contact" { "contact page" } "."
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

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn mirror_lists_exclude_current_apex_and_www_host() {
      for (host, excluded) in [
         ("www.teapawt.org", "teapawt.org"),
         ("teapawt.org", "teapawt.org"),
         ("nitter.cf", "nitter.cf"),
         ("www.nitter.cf:443", "nitter.cf"),
      ] {
         assert!(!mirror_hosts(host).contains(&excluded), "{host}");
         let json_ld = home_json_ld(&format!("https://{host}"), host);
         assert!(
            !json_ld.contains(&format!("\"https://{excluded}\"")),
            "{host}"
         );
         let announce = mirror_announcement(host).into_string();
         assert!(!announce.contains(excluded), "{host}");
      }
   }

   #[test]
   fn front_page_says_search_turnstile_stays_and_scraping_needs_a_pass() {
      let html = search_announcement("$15").into_string();
      assert!(html.contains("Cloudflare Turnstile on search is staying on."));
      assert!(html.contains("Another 300 accounts were added"));
      assert!(html.contains("flooded the site with search requests."));
      assert!(html.contains("href=\"/search-pass\""));
      assert!(html.contains("$15 a month"));
      assert!(html.contains("5 minutes after the checkbox."));
   }

   #[test]
   fn front_page_announces_dedicated_search_pass_pool() {
      let html = search_pass_pool_announcement("$15").into_string();
      assert!(html.contains("Dedicated account pool for search pass holders"));
      assert!(html.contains("better response times and eventually zero rate limits"));
      assert!(html.contains("accounts keep getting suspended"));
      assert!(html.contains("paid users always have a good experience"));
      assert!(html.contains("href=\"/search-pass\""));
      assert!(html.contains("$15 a month"));
   }
}
