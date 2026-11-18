//! What a visitor sees instead of content when the Anti-DDoS gate is up:
//! the gate page itself and the home page notice that explains it.

use maud::{Markup, html};

/// Why this visitor is being checked.
pub enum Why {
   /// A network listed in `antiDdos.blockedAsns`.
   Network { asn: u32, per_minute: u32 },
   /// A watched country above its usual rate.
   Surge,
   /// `antiDdos.everyone`: the operator put the gate up for everyone.
   Everyone,
   /// The automatic switch: `share` percent of first pages left
   /// `UserTweets`, so the account pool is being drained.
   Attack { share: u8 },
}

impl Why {
   #[must_use]
   pub const fn title(&self) -> &'static str {
      match self {
         Self::Everyone => "Quick check",
         Self::Network { .. } | Self::Surge | Self::Attack { .. } => "Slow down",
      }
   }
}

pub struct GatePage<'a> {
   pub site_key:     &'a str,
   pub action:       &'a str,
   pub resume:       &'a str,
   pub why:          Why,
   pub price:        &'a str,
   pub pass_minutes: i64,
   pub failed:       bool,
}

/// Tencent Cloud's international network.
const TENCENT_ASN: u32 = 132_203;

fn network_name(asn: u32) -> String {
   if asn == TENCENT_ASN {
      format!("Tencent Cloud (AS{asn})")
   } else {
      format!("AS{asn}")
   }
}

pub fn render_gate(page: &GatePage<'_>) -> Markup {
   html! {
       div class="turnstile-panel" {
           h1 { (page.why.title()) }
           @match page.why {
               Why::Network { asn, per_minute } => {
                   p {
                       "This request comes from " strong { (network_name(asn)) } ". "
                       "A scraper on that network is loading "
                       @if per_minute > 0 { "about " strong { (per_minute) } " pages a minute" }
                       @else { "thousands of pages a minute" }
                       " from this site, from more than a thousand rotating addresses."
                   }
               }
               Why::Surge => {
                   p {
                       "Far more pages than usual are being loaded from your country right now, "
                       "almost all by one scraper. Every visitor from here is asked to confirm for a while."
                   }
               }
               Why::Everyone => {
                   p {
                       "Anti-DDoS is enabled due to an extremely high spike in traffic. Every "
                       "visitor is asked to pass a Cloudflare check until it drops back to "
                       "normal. A " a href="/search-pass" { "search pass" } " bypasses it."
                   }
               }
               Why::Attack { share } => {
                   p {
                       "The site is being flooded right now: " strong { (share) "%" }
                       " of profile requests are already running past the limit of the X accounts "
                       "behind them. Every visitor is asked to pass a Cloudflare check until the "
                       "flood stops, so real readers still get pages. A "
                       a href="/search-pass" { "search pass" } " skips the check."
                   }
               }
           }
           p {
               "Each page costs a request on one of a limited number of X accounts. "
               "Scrapers use them up, so profiles and RSS feeds stall for everyone else, "
               "for 10 to 40 seconds at a time."
           }
           @if let Why::Network { asn: TENCENT_ASN, .. } = page.why {
               p {
                   strong { "To Tencent Cloud and whoever runs this scraper on it: " }
                   "pay for what you take. Scraping at this rate needs a "
                   a href="/search-pass" { "search pass" } " (" (page.price) " a month), "
                   "or get in touch through the " a href="/contact" { "contact page" } " "
                   "for higher volume. That money buys the accounts you are burning through. "
                   "Until then, solve this check like everyone else, once per address."
               }
           }
           @if page.failed {
               p class="turnstile-error" { "That check did not go through. Press the checkbox and try again." }
           } @else {
               p { "If you are a person, press the checkbox. It lasts " (page.pass_minutes) " minutes." }
           }
           form method="post" action=(page.action) autocomplete="off" {
               input type="hidden" name="resume" value=(page.resume);
               div class="cf-turnstile" data-sitekey=(page.site_key) data-appearance="always" data-theme="auto" {}
               button type="submit" { "Continue" }
           }
           noscript {
               p { "The checkbox needs JavaScript. Turn it on for this page." }
           }
           p class="turnstile-pass" {
               "A " a href="/search-pass" { "search pass" } " (" (page.price) " a month) skips this check. "
               a href="/search-pass/login" { "Log in" } " if you have one. "
               "Donated before? " a href="/contact" { "Ask for a pass" } "."
           }
           script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer {}
       }
   }
}

/// The home page notice while the global Anti-DDoS gate is active.
/// `check_minutes` adds the Turnstile check duration for visitors who have
/// not cleared the gate yet; `None` (or non-positive minutes) omits it.
pub fn gate_notice(check_minutes: Option<i64>) -> Markup {
   html! {
       aside class="home-announce" {
           strong { "Anti-DDoS is enabled due to an extremely high spike in traffic" }
           @if let Some(minutes) = check_minutes.filter(|mins| *mins > 0) {
               p {
                   "Every visitor has to pass a Cloudflare check once every "
                   @if minutes == 1 { "minute" } @else { (minutes) " minutes" }
                   " until the traffic drops back to normal. A "
                   a href="/search-pass" { "search pass" } " skips the check."
               }
           } @else {
               p { "Profiles and feeds may load slower than usual until traffic drops back to normal." }
           }
       }
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn the_gate_names_tencent_and_carries_the_resume_path() {
      let page = render_gate(&GatePage {
         site_key: "key",
         action: "/.ddos-check",
         resume: "/jack?x=1",
         why: Why::Network { asn: 132_203, per_minute: 6100 },
         price: "$15",
         pass_minutes: 60,
         failed: false,
      })
      .into_string();
      assert!(page.contains("Tencent Cloud (AS132203)"));
      assert!(page.contains("6100"));
      assert!(page.contains("value=\"/jack?x=1\""));
      let surge = render_gate(&GatePage {
         site_key: "key",
         action: "/.ddos-check",
         resume: "/",
         why: Why::Surge,
         price: "$15",
         pass_minutes: 60,
         failed: true,
      })
      .into_string();
      assert!(!surge.contains("To Tencent Cloud"));
      assert!(surge.contains("did not go through"));
      let everyone = render_gate(&GatePage {
         site_key: "key",
         action: "/.ddos-check",
         resume: "/",
         why: Why::Everyone,
         price: "$15",
         pass_minutes: 60,
         failed: false,
      })
      .into_string();
      assert!(everyone.contains("Quick check"));
      assert!(everyone.contains("spike in traffic"));
      assert!(everyone.contains("/search-pass"));
      assert!(!everyone.contains("To Tencent Cloud"));
   }

   #[test]
   fn the_automatic_gate_says_how_much_traffic_is_being_flooded() {
      let page = render_gate(&GatePage {
         site_key: "key",
         action: "/.ddos-check",
         resume: "/",
         why: Why::Attack { share: 92 },
         price: "$15",
         pass_minutes: 60,
         failed: false,
      })
      .into_string();
      assert!(page.contains("Slow down"));
      assert!(page.contains("92%"));
      assert!(!page.contains("To Tencent Cloud"));
      let checked = gate_notice(Some(60)).into_string();
      assert!(checked.contains("spike in traffic"));
      assert!(checked.contains("60 minutes"));
      let quiet = gate_notice(None).into_string();
      assert!(quiet.contains("spike in traffic"));
      assert!(!quiet.contains("search-pass"));
      let non_positive = gate_notice(Some(0)).into_string();
      assert!(!non_positive.contains("0 minutes"));
      assert!(!non_positive.contains("search-pass"));
   }

   #[test]
   fn the_gate_notice_never_mentions_an_old_incident() {
      for notice in [gate_notice(None), gate_notice(Some(60))] {
         let notice = notice.into_string();
         assert!(notice.contains("Anti-DDoS is enabled due to an extremely high spike in traffic"));
         assert!(!notice.contains("Tencent"));
         assert!(!notice.contains("AS132203"));
         assert!(!notice.contains("fast again"));
      }
   }
}