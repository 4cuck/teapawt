//! The contact page: how to reach the operator, and how a past donation
//! becomes a search pass.

use maud::{
   Markup,
   html,
};

use crate::config::ContactConfig;

pub fn render_contact(contact: &ContactConfig, price: &str) -> Markup {
   let telegram = telegram_href(&contact.telegram);
   let matrix = normalize_matrix(&contact.matrix);
   let session = contact.session.trim();
   let email = contact.email.trim();
   let has_any = contact.any()
      && (telegram.is_some() || matrix.is_some() || !session.is_empty() || !email.is_empty());
   html! {
       div class="overlay-panel contact-page" {
           h1 { "Contact and support" }
           p { "Questions, broken pages, or a search pass for a past donation: reach the operator here." }
           @if has_any {
               ul class="contact-list" {
                   @if let Some(tg_href) = telegram {
                       li {
                           span class="contact-label" { "Telegram" }
                           a class="contact-value" href=(tg_href) rel="noopener" { (telegram_label(&contact.telegram)) }
                       }
                   }
                   @if let Some((mx_href, mx_label)) = matrix {
                       li {
                           span class="contact-label" { "Matrix" }
                           a class="contact-value" href=(mx_href) rel="noopener" { (mx_label) }
                       }
                   }
                   @if !session.is_empty() {
                       li {
                           span class="contact-label" { "Session" }
                           code class="contact-value contact-session" { (session) }
                       }
                   }
                   @if !email.is_empty() {
                       li {
                           span class="contact-label" { "Email" }
                           a class="contact-value" href=(format!("mailto:{email}")) { (email) }
                       }
                   }
               }
           } @else {
               p { "No contact details are set up on this instance." }
           }

           h2 { "Already donated? Get a search pass" }
           p {
               "If you donated by card, crypto, or LZT Market, you do not have to pay again for a "
               a href="/search-pass" { "search pass" } " (" (price) " a month for everyone else). "
               "Send the email address, transaction ID, or receipt from your donation through any contact above, "
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

fn telegram_handle(raw: &str) -> Option<&str> {
   let trimmed = raw.trim().trim_end_matches('/');
   if trimmed.is_empty() {
      return None;
   }
   let without_scheme = trimmed
      .strip_prefix("https://")
      .or_else(|| trimmed.strip_prefix("http://"))
      .unwrap_or(trimmed);
   let rest = without_scheme
      .strip_prefix("t.me/")
      .or_else(|| without_scheme.strip_prefix("www.t.me/"))
      .or_else(|| without_scheme.strip_prefix("telegram.me/"))
      .unwrap_or_else(|| {
         if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            ""
         } else {
            trimmed
         }
      });
   let handle = rest.trim_start_matches('@').trim_matches('/');
   (!handle.is_empty()).then_some(handle)
}

fn telegram_href(raw: &str) -> Option<String> {
   if let Some(handle) = telegram_handle(raw) {
      return Some(format!("https://t.me/{handle}"));
   }
   let trimmed = raw.trim().trim_end_matches('/');
   let is_custom_url = (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
      && !matches!(
         trimmed,
         "https://t.me"
            | "http://t.me"
            | "https://www.t.me"
            | "http://www.t.me"
            | "https://telegram.me"
            | "http://telegram.me"
      );
   is_custom_url.then(|| trimmed.to_owned())
}

fn telegram_label(raw: &str) -> String {
   telegram_handle(raw).map_or_else(
      || raw.trim().trim_end_matches('/').to_owned(),
      |handle| format!("@{handle}"),
   )
}

fn normalize_matrix(raw: &str) -> Option<(String, String)> {
   let trimmed = raw.trim().trim_end_matches('/');
   if trimmed.is_empty() {
      return None;
   }
   let without_scheme = trimmed
      .strip_prefix("https://")
      .or_else(|| trimmed.strip_prefix("http://"))
      .unwrap_or(trimmed);
   let target = without_scheme
      .strip_prefix("matrix.to/#/")
      .unwrap_or(trimmed)
      .trim_end_matches('/');
   if target.is_empty()
      || matches!(
         target,
         "https://matrix.to/#" | "http://matrix.to/#" | "matrix.to/#"
      )
   {
      return None;
   }
   if target.starts_with("https://") || target.starts_with("http://") {
      return Some((target.to_owned(), target.to_owned()));
   }
   Some((format!("https://matrix.to/#/{target}"), target.to_owned()))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn the_contact_page_lists_only_what_is_set() {
      let contact = ContactConfig {
         telegram: "https://t.me/someone".to_owned(),
         matrix:   "@someone:example.org".to_owned(),
         session:  String::new(),
         email:    "support@example.org".to_owned(),
      };
      let page = render_contact(&contact, "$15").into_string();
      assert!(page.contains("href=\"https://t.me/someone\""));
      assert!(page.contains("@someone</a>"));
      assert!(page.contains("https://matrix.to/#/@someone:example.org"));
      assert!(page.contains("mailto:support@example.org"));
      assert!(!page.contains("contact-session"));
      assert!(page.contains("$15 a month"));
      assert!(page.contains("donated by card, crypto, or LZT Market"));
   }

   #[test]
   fn telegram_inputs_normalize_href_and_label() {
      for input in [
         "@someone",
         "@someone/",
         "t.me/someone",
         "t.me/someone/",
         "https://t.me/someone",
         "https://t.me/someone/",
         "https://t.me/@someone/",
      ] {
         assert_eq!(
            telegram_href(input).as_deref(),
            Some("https://t.me/someone"),
            "{input}"
         );
         assert_eq!(telegram_label(input), "@someone", "{input}");
      }
      assert_eq!(telegram_href("@"), None);
      assert_eq!(telegram_href("https://t.me/"), None);
   }

   #[test]
   fn matrix_inputs_do_not_duplicate_matrix_to_prefix() {
      for input in [
         "@user:example.org",
         "https://matrix.to/#/@user:example.org",
         "https://matrix.to/#/@user:example.org/",
         "matrix.to/#/@user:example.org",
      ] {
         assert_eq!(
            normalize_matrix(input),
            Some((
               "https://matrix.to/#/@user:example.org".to_owned(),
               "@user:example.org".to_owned(),
            )),
            "{input}"
         );
      }
      let contact = ContactConfig {
         telegram: "@handle/".to_owned(),
         matrix:   "https://matrix.to/#/@user:example.org".to_owned(),
         session:  String::new(),
         email:    String::new(),
      };
      let page = render_contact(&contact, "$15").into_string();
      assert!(page.contains("href=\"https://t.me/handle\""));
      assert!(page.contains(">@handle</a>"));
      assert!(!page.contains("@@"));
      assert!(page.contains("href=\"https://matrix.to/#/@user:example.org\""));
      assert!(!page.contains("https://matrix.to/#/https://matrix.to/#/"));
   }
}
