//! The page a scraping network or a surging country gets instead of content,
//! the home notice that explains the slowdown, and the contact page.

use maud::{Markup, html};

use crate::config::ContactConfig;

/// Why this visitor is being checked.
pub enum Why {
   /// A network listed in `scrape.blockedAsns`.
   Network { asn: u32, per_minute: u32 },
   /// A watched country above its usual rate.
   Surge,
   /// `scrape.everyone`: every visitor, until the month's goal is met.
   Everyone,
}

impl Why {
   #[must_use]
   pub const fn title(&self) -> &'static str {
      match self {
         Self::Everyone => "Quick check",
         Self::Network { .. } | Self::Surge => "Slow down",
      }
   }
}

pub struct GatePage<'a> {
   pub site_key:     &'a str,
   pub action:       &'a str,
   pub resume:       &'a str,
   pub why:          Why,
   pub price:        &'a str,
   /// The monthly donation goal, formatted.
   pub goal:         &'a str,
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
       div class="turnstile-panel scrape-gate" {
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
                       "Bots are loading far more pages than people are, so for now every visitor is asked "
                       "to pass a Cloudflare check. We will turn it off once donations reach this month's "
                       (page.goal) " " a href="/donate" { "goal" } "."
                   }
               }
           }
           p {
               "Each page costs a request on one of a limited number of X accounts. "
               "Scrapers use them up, so profiles and RSS feeds stall for everyone else, "
               "for 10 to 40 seconds at a time."
           }
           @if let Why::Network { asn: TENCENT_ASN, .. } = page.why {
               p class="scrape-gate-ask" {
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

/// The home page notice while `scrape.everyone` is on.
pub fn everyone_notice(price: &str, goal: &str, pass_minutes: i64) -> Markup {
   html! {
       aside class="home-announce scrape-announce" {
           strong { "We had to turn on a Cloudflare check for everyone" }
           p {
               "Bots are loading far more pages than people are. Every page costs a request on a "
               "limited pool of X accounts, so profiles and feeds were stalling for real readers. "
               "For now, every visitor is asked to pass a Cloudflare Turnstile check, "
               "once every " (pass_minutes) " minutes."
           }
           p {
               "We will turn it off once we reach this month's " (goal) " "
               a href="/donate" { "donation goal" } ". That money buys the accounts the bots burn through."
           }
           p {
               "A " a href="/search-pass" { "search pass" } " (" (price) " a month) skips the check: "
               a href="/search-pass/login" { "log in" } ", or add your address to the pass to skip it "
               "on every domain. Donated already? " a href="/contact" { "Ask for a pass" } "."
           }
       }
   }
}

/// The home page notice about the Tencent Cloud scraper, now that it is gated.
pub fn scrape_notice(price: &str, goal: &str) -> Markup {
   html! {
       aside class="home-announce scrape-announce" {
           strong { "Profiles and RSS are fast again" }
           p {
               "A scraper on Tencent Cloud's network in Singapore (AS132203) was loading "
               "over 6,000 pages a minute from more than a thousand rotating addresses. "
               "Every page costs a request on a limited pool of X accounts, so profiles and feeds "
               "were waiting 10 to 40 seconds. That network now has to pass a Cloudflare check "
               "on every address, and pages load at normal speed again."
           }
           p {
               "Tencent Cloud, and whoever owns those addresses: buy a "
               a href="/search-pass" { "search pass" } " (" (price) " a month) and "
               a href="/donate" { "donate" } " so this site reaches its " (goal) " monthly goal. "
               "That money pays for more X accounts, keeps the site up, "
               "and then you can scrape as much as you want."
           }
           p {
               a href="/donate" { "Donations" } " buy accounts for everyone. Donated already and want a search pass? "
               a href="/contact" { "Get in touch" } "."
           }
       }
   }
}

pub fn render_contact(contact: &ContactConfig, price: &str) -> Markup {
   html! {
       div class="overlay-panel contact-page" {
           h1 { "Contact and support" }
           p { "Questions, broken pages, or a search pass for a past donation: reach the operator here." }
           @if contact.any() {
               ul class="contact-list" {
                   @if !contact.telegram.is_empty() {
                       li {
                           span class="contact-label" { "Telegram" }
                           a class="contact-value" href=(contact.telegram) rel="noopener" { (telegram_label(&contact.telegram)) }
                       }
                   }
                   @if !contact.matrix.is_empty() {
                       li {
                           span class="contact-label" { "Matrix" }
                           a class="contact-value" href=(format!("https://matrix.to/#/{}", contact.matrix)) rel="noopener" { (contact.matrix) }
                       }
                   }
                   @if !contact.session.is_empty() {
                       li {
                           span class="contact-label" { "Session" }
                           code class="contact-value contact-session" { (contact.session) }
                       }
                   }
                   @if !contact.email.is_empty() {
                       li {
                           span class="contact-label" { "Email" }
                           a class="contact-value" href=(format!("mailto:{}", contact.email)) { (contact.email) }
                       }
                   }
               }
           } @else {
               p { "No contact details are set up on this instance." }
           }

           h2 { "Already donated? Get a search pass" }
           p {
               "If you donated by card, you do not have to pay again for a "
               a href="/search-pass" { "search pass" } " (" (price) " a month for everyone else). "
               "Send the email address you donated with through any contact above, "
               "with the amount and roughly when, so it is quick to find."
           }
           p {
               "Once the donation is found, you get a username and password for a pass."
           }

           h2 { "Scraping at volume" }
           p {
               "Scrapers that load thousands of pages a minute use up the X accounts everyone shares. "
               "If you need that volume, get in touch and pay for it instead of rotating addresses."
           }
       }
   }
}

fn telegram_label(url: &str) -> String {
   url.rsplit_once("t.me/")
      .map_or_else(|| url.to_owned(), |(_, name)| format!("@{name}"))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn the_contact_page_lists_only_what_is_set() {
      let contact = ContactConfig {
         telegram: "https://t.me/someone".to_owned(),
         matrix: "@someone:example.org".to_owned(),
         session: String::new(),
         email: "support@example.org".to_owned(),
      };
      let page = render_contact(&contact, "$15").into_string();
      assert!(page.contains("@someone</a>"));
      assert!(page.contains("https://matrix.to/#/@someone:example.org"));
      assert!(page.contains("mailto:support@example.org"));
      assert!(!page.contains("contact-session"));
      assert!(page.contains("$15 a month"));
   }

   #[test]
   fn the_gate_names_tencent_and_carries_the_resume_path() {
      let page = render_gate(&GatePage {
         site_key: "key",
         action: "/.scrape-check",
         resume: "/jack?x=1",
         why: Why::Network { asn: 132_203, per_minute: 6100 },
         price: "$15",
         goal: "$300",
         pass_minutes: 60,
         failed: false,
      })
      .into_string();
      assert!(page.contains("Tencent Cloud (AS132203)"));
      assert!(page.contains("6100"));
      assert!(page.contains("value=\"/jack?x=1\""));
      let surge = render_gate(&GatePage {
         site_key: "key",
         action: "/.scrape-check",
         resume: "/",
         why: Why::Surge,
         price: "$15",
         goal: "$300",
         pass_minutes: 60,
         failed: true,
      })
      .into_string();
      assert!(!surge.contains("To Tencent Cloud"));
      assert!(surge.contains("did not go through"));
      let everyone = render_gate(&GatePage {
         site_key: "key",
         action: "/.scrape-check",
         resume: "/",
         why: Why::Everyone,
         price: "$15",
         goal: "$300",
         pass_minutes: 60,
         failed: false,
      })
      .into_string();
      assert!(everyone.contains("Quick check"));
      assert!(everyone.contains("$300"));
      assert!(!everyone.contains("To Tencent Cloud"));
   }
}