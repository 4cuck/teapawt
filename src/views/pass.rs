//! Search pass pages: purchase, account (with the admin's issuing card),
//! checkout return, login and recovery.

use maud::{
   Markup,
   html,
};

/// Payment methods this instance has keys for.
pub struct Methods {
   pub card:   bool,
   pub crypto: bool,
   pub lzt:    bool,
}

impl Methods {
   const fn any(&self) -> bool {
      self.card || self.crypto || self.lzt
   }
}

#[derive(Clone, Copy)]
pub enum Tone {
   Good,
   Bad,
   Info,
}

impl Tone {
   const fn class(self) -> &'static str {
      match self {
         Self::Good => "pass-notice is-good",
         Self::Bad => "pass-notice is-bad",
         Self::Info => "pass-notice is-info",
      }
   }
}

/// A one-line message at the top of a page.
#[derive(Clone, Copy)]
pub struct Notice {
   pub tone: Tone,
   pub text: &'static str,
}

pub struct BuyPage<'a> {
   pub price:         &'a str,
   pub methods:       &'a Methods,
   pub mail:          bool,
   pub max_addresses: usize,
   pub notice:        Option<Notice>,
}

pub struct AccountPage<'a> {
   pub username:      &'a str,
   pub email:         &'a str,
   pub paid_until:    &'a str,
   pub active:        bool,
   pub renews:        bool,
   pub card:          bool,
   pub addresses:     &'a [String],
   pub current:       Option<&'a str>,
   pub max_addresses: usize,
   pub price:         &'a str,
   pub methods:       &'a Methods,
   pub mail:          bool,
   pub notice:        Option<Notice>,
   /// Set only for the admin account.
   pub admin:         Option<Admin<'a>>,
}

pub struct Admin<'a> {
   pub default_days: i64,
   pub max_days:     i64,
   pub issued:       Option<&'a Issued<'a>>,
}

/// A pass the admin just issued. `password` is `None` when an existing pass
/// on that email was extended and keeps its login.
pub struct Issued<'a> {
   pub username:   &'a str,
   pub password:   Option<&'a str>,
   pub paid_until: &'a str,
   pub days:       i64,
   pub origin:     &'a str,
}

/// What the page after checkout says.
pub enum Return<'a> {
   Waiting,
   Issued {
      username:   &'a str,
      password:   &'a str,
      paid_until: &'a str,
      emailed:    Option<&'a str>,
   },
   SignedIn {
      username:   &'a str,
      paid_until: &'a str,
   },
   Renewed {
      paid_until: &'a str,
   },
   Emailed {
      to: &'a str,
   },
   OtherBrowser,
   Unknown,
}

// ---------------------------------------------------------------------------
// Building blocks

fn shell(narrow: bool, body: Markup) -> Markup {
   let class = if narrow { "pass-page is-narrow" } else { "pass-page" };
   html! { div class=(class) { (body) } }
}

fn notice(notice: Option<Notice>) -> Markup {
   html! {
       @if let Some(notice) = notice {
           p class=(notice.tone.class()) role="status" { (notice.text) }
       }
   }
}

/// A one-button form.
fn action(path: &str, class: &str, label: &str) -> Markup {
   html! {
       form method="post" action=(path) {
           button type="submit" class=(class) { (label) }
       }
   }
}

#[derive(Default)]
struct Card<'a> {
   title: &'a str,
   class: &'a str,
   hint:  Option<Markup>,
   /// Right side of the header: a count, a tag or a single button.
   aside: Option<Markup>,
}

impl Card<'_> {
   fn wrap(self, body: Markup) -> Markup {
      html! {
          section class={ "pass-card " (self.class) } {
              header class="pass-card-head" {
                  div {
                      h2 { (self.title) }
                      @if let Some(hint) = self.hint { p class="pass-hint" { (hint) } }
                  }
                  @if let Some(aside) = self.aside { (aside) }
              }
              (body)
          }
      }
   }
}

fn optional_label(id: &str, text: &str) -> Markup {
   html! { label for=(id) { (text) " " span class="pass-optional" { "optional" } } }
}

fn methods_form(methods: &Methods, price: &str, card: bool) -> Markup {
   let tiles = [
      (methods.card && card, "stripe", "Card", format!("{price} a month, renews")),
      (methods.crypto, "nowpayments", "Crypto", "One month".to_owned()),
      (methods.lzt, "lzt", "LZT Market", "One month".to_owned()),
   ];
   html! {
       div class="pass-methods" {
           @for (_, provider, name, terms) in tiles.iter().filter(|tile| tile.0) {
               button type="submit" class="pass-method" name="provider" value=(provider) {
                   strong { (name) }
                   span { (terms) }
               }
           }
       }
   }
}

fn login_box(username: &str, password: &str) -> Markup {
   html! {
       dl class="pass-login" {
           dt { "Username" }
           dd { code { (username) } }
           dt { "Password" }
           dd { code { (password) } }
       }
   }
}

// ---------------------------------------------------------------------------
// Purchase

pub fn render_buy(page: &BuyPage<'_>) -> Markup {
   let checkout = Card {
      title: "Get a pass",
      hint: Some(html! {
          "Card renews every month until you stop it. Crypto and LZT Market pay for one month at a time."
      }),
      ..Card::default()
   }
   .wrap(html! {
       form class="pass-stack" method="post" action="/search-pass/buy" {
           @if page.mail {
               div class="pass-field" {
                   (optional_label("pass-email", "Email"))
                   input id="pass-email" name="email" type="email" autocomplete="email"
                       maxlength="254" placeholder="you@example.com";
                   p class="pass-hint" { "Your login is sent there in case you lose it. Leave it empty to buy anonymously." }
               }
           }
           (methods_form(page.methods, page.price, true))
           p class="pass-hint" {
               "Anonymous passes live in this browser's cookies. The login is shown once after payment, "
               "so do not block cookies for this site."
           }
       }
   });

   shell(false, html! {
       section class="pass-hero" {
           span class="pass-eyebrow" { "Search pass" }
           h1 { "Search without the checkbox" }
           p class="pass-lede" { "Skip the Cloudflare check on search and on search RSS feeds." }
           p class="pass-price" { strong { (page.price) } span { "a month" } }
       }
       ul class="pass-perks" {
           li {
               strong { "No checkbox" }
               span { "Search and search RSS open straight away in this browser." }
           }
           li {
               strong { "Feed readers" }
               span { "Whitelist up to " (page.max_addresses) " IP addresses so search RSS works without cookies." }
           }
           li {
               strong { "No sign-up" }
               span { "Pay and get a username and password. This browser stays signed in." }
           }
       }
       (notice(page.notice))
       @if page.methods.any() {
           (checkout)
       } @else {
           p class="pass-notice is-info" { "Payments are not set up on this instance." }
       }
       p class="pass-foot" {
           "Already have a pass? "
           a href="/search-pass/login" { "Log in" }
           " · "
           a href="/search-pass/forgot" { "Forgot your login?" }
       }
   })
}

// ---------------------------------------------------------------------------
// Account

pub fn render_account(page: &AccountPage<'_>) -> Markup {
   let blocks = [
      Some(status(page)),
      page.admin.as_ref().map(admin_card),
      page.admin.is_none().then(|| add_time(page)),
      Some(addresses(page)),
      page.mail.then(|| recovery_email(page)),
      Some(password_card()),
   ];
   shell(false, html! {
       (notice(page.notice))
       @for block in blocks.into_iter().flatten() { (block) }
   })
}

fn status(page: &AccountPage<'_>) -> Markup {
   let (badge, state, until) = if page.active {
      ("pass-badge is-good", "Active", "Skips the checkbox until")
   } else {
      ("pass-badge is-bad", "Expired", "Ran out on")
   };
   let renewal = match (page.renews, page.card) {
      (true, _) => "Card, monthly",
      (false, true) => "Stopped",
      (false, false) => "Pay as you go",
   };
   html! {
       section class="pass-hero" {
           div class="pass-hero-top" {
               span class=(badge) { (state) }
               (action("/search-pass/logout", "pass-btn is-ghost is-small", "Log out"))
           }
           h1 { "Your search pass" }
           p class="pass-lede" { "Signed in as " strong { (page.username) } "." }
           @if !page.active {
               p class="pass-lede" { "Add time below to use it again." }
           }
           dl class="pass-facts" {
               div { dt { (until) } dd { (page.paid_until) } }
               div { dt { "Renewal" } dd { (renewal) } }
               div { dt { "IP addresses" } dd { (page.addresses.len()) " of " (page.max_addresses) } }
           }
       }
   }
}

fn admin_card(admin: &Admin<'_>) -> Markup {
   Card {
      title: "Issue a pass",
      class: "is-admin",
      hint: Some(html! {
          "With an email that already has a pass, the days are added to it. "
          "Otherwise a new username and password are made."
      }),
      aside: Some(html! { span class="pass-tag" { "Admin" } }),
   }
   .wrap(html! {
       @if let Some(issued) = admin.issued { (issued_box(issued)) }
       form class="pass-grid" method="post" action="/search-pass/grant" autocomplete="off" {
           div class="pass-field" {
               (optional_label("grant-email", "Donor email"))
               input id="grant-email" name="email" type="email" maxlength="254"
                   placeholder="donor@example.com";
           }
           div class="pass-field" {
               label for="grant-days" { "Days" }
               input id="grant-days" name="days" type="number" min="1" max=(admin.max_days)
                   value=(admin.default_days) required;
           }
           button type="submit" { "Issue pass" }
       }
   })
}

fn issued_box(issued: &Issued<'_>) -> Markup {
   html! {
       div class="pass-issued" {
           @if let Some(password) = issued.password {
               p { strong { "New pass, " (issued.days) " days." } " Send them this. The password is shown only here." }
               pre class="pass-copy" {
                   "Search pass for " (issued.origin) "\n"
                   "Username: " (issued.username) "\n"
                   "Password: " (password) "\n"
                   "Sign in at " (issued.origin) "/search-pass/login"
               }
           } @else {
               p {
                   strong { "Added " (issued.days) " days" }
                   " to " code { (issued.username) } ", the pass already on that email. Its password still works."
               }
           }
           p class="pass-hint" { "Runs until " (issued.paid_until) "." }
       }
   }
}

fn add_time(page: &AccountPage<'_>) -> Markup {
   let hint = if page.renews {
      "Your card renews this pass every month. Crypto and LZT Market add one month at a time."
   } else {
      "Each payment adds a month. Card renews every month until you stop it."
   };
   Card {
      title: "Add time",
      hint: Some(html! { (hint) }),
      ..Card::default()
   }
   .wrap(html! {
       @if page.methods.any() {
           form method="post" action="/search-pass/buy" {
               (methods_form(page.methods, page.price, !page.renews))
           }
       } @else {
           p class="pass-empty" { "Payments are not set up on this instance." }
       }
       @if page.renews {
           div class="pass-row" {
               p class="pass-hint" { "Stopping keeps the time already paid for." }
               (action("/search-pass/renewal", "pass-btn is-ghost is-danger is-small", "Stop card renewal"))
           }
       } @else if page.card {
           p class="pass-hint" { "Card renewal is stopped. The pass ends on the date above." }
       }
   })
}

fn addresses(page: &AccountPage<'_>) -> Markup {
   let full = page.addresses.len() >= page.max_addresses;
   let current = page
      .current
      .filter(|current| !page.addresses.iter().any(|address| address == current));
   Card {
      title: "Whitelisted IP addresses",
      hint: Some(html! {
          "Requests from these addresses skip the checkbox while the pass is paid up, with no cookies. "
          "Use it for a feed reader or a script. An IPv6 address covers its whole /64."
      }),
      aside: Some(html! {
          span class="pass-count" { (page.addresses.len()) " / " (page.max_addresses) }
      }),
      ..Card::default()
   }
   .wrap(html! {
       @if page.addresses.is_empty() {
           p class="pass-empty" { "No addresses yet." }
       } @else {
           ul class="pass-ips" {
               @for address in page.addresses {
                   li {
                       code { (address) }
                       form method="post" action="/search-pass/ips" {
                           input type="hidden" name="action" value="remove";
                           input type="hidden" name="ip" value=(address);
                           button type="submit" class="pass-btn is-ghost is-danger is-small"
                               aria-label={ "Remove " (address) } { "Remove" }
                       }
                   }
               }
           }
       }
       @if full {
           p class="pass-hint" { "The list is full. Remove an address to add another." }
       } @else {
           form class="pass-inline" method="post" action="/search-pass/ips" {
               input type="hidden" name="action" value="add";
               input id="pass-ip" name="ip" type="text" autocomplete="off" maxlength="64"
                   aria-label="IP address" placeholder="203.0.113.7 or 2001:db8::1" required;
               button type="submit" { "Add" }
           }
           @if let Some(current) = current {
               form method="post" action="/search-pass/ips" {
                   input type="hidden" name="action" value="current";
                   button type="submit" class="pass-btn is-ghost is-small" {
                       "Add this connection " span class="pass-mono" { (current) }
                   }
               }
           }
       }
   })
}

fn recovery_email(page: &AccountPage<'_>) -> Markup {
   Card {
      title: "Recovery email",
      hint: Some(html! { "A forgotten login is sent here. Leave it empty to remove it." }),
      ..Card::default()
   }
   .wrap(html! {
       form class="pass-inline" method="post" action="/search-pass/email" {
           input id="pass-email" name="email" type="email" autocomplete="email" maxlength="254"
               aria-label="Recovery email" value=(page.email) placeholder="you@example.com";
           button type="submit" { "Save" }
       }
   })
}

fn password_card() -> Markup {
   Card {
      title: "Password",
      hint: Some(html! { "The old password stops working. Signed-in browsers stay signed in." }),
      aside: Some(action("/search-pass/password", "pass-btn is-ghost", "Make a new password")),
      ..Card::default()
   }
   .wrap(html! {})
}

// ---------------------------------------------------------------------------
// Checkout return, login and recovery

pub fn render_return(view: &Return<'_>) -> Markup {
   let manage = html! { a class="pass-link" href="/search-pass" { "Manage the pass and its IP addresses →" } };
   let body = match view {
      Return::Waiting => html! {
          span class="pass-spinner" aria-hidden="true" {}
          h1 { "Waiting for the payment" }
          p class="pass-hint" { "This page checks again every 10 seconds. Crypto can take a while to confirm." }
          p class="pass-hint" {
              "You can close it. Opening " a href="/search-pass" { "the search pass page" }
              " in this browser later picks up the login."
          }
      },
      Return::Issued { username, password, paid_until, emailed } => html! {
          span class="pass-badge is-good" { "Paid" }
          h1 { "Your search pass is ready" }
          p class="pass-hint" { "Search skips the checkbox until " strong { (paid_until) } ". This browser is signed in." }
          (login_box(username, password))
          @if let Some(to) = emailed {
              p class="pass-notice is-info" { "A copy is on its way to " (to) "." }
          } @else {
              p class="pass-notice is-bad" { "Save this login now. It is shown once, and with no email it cannot be sent again." }
          }
          (manage)
      },
      Return::SignedIn { username, paid_until } => html! {
          span class="pass-badge is-good" { "Paid" }
          h1 { "Your search pass is ready" }
          p class="pass-hint" {
              "Signed in as " strong { (username) } ". Search skips the checkbox until " strong { (paid_until) } "."
          }
          p class="pass-hint" { "The password was shown or emailed already. You can make a new one on the pass page." }
          (manage)
      },
      Return::Renewed { paid_until } => html! {
          span class="pass-badge is-good" { "Paid" }
          h1 { "Thank you" }
          p class="pass-hint" { "The pass now runs until " strong { (paid_until) } "." }
          a class="pass-link" href="/search-pass" { "Back to the pass →" }
      },
      Return::Emailed { to } => html! {
          span class="pass-badge is-good" { "Paid" }
          h1 { "Payment received" }
          p class="pass-hint" { "The login is on its way to " (to) ". It can take a few minutes." }
          a class="pass-link" href="/search-pass/login" { "Log in →" }
      },
      Return::OtherBrowser => html! {
          span class="pass-badge is-good" { "Paid" }
          h1 { "Payment received" }
          p class="pass-hint" {
              "Open this page in the browser you paid from. An anonymous pass is handed to that browser's cookies only."
          }
      },
      Return::Unknown => html! {
          h1 { "Order not found" }
          a class="pass-link" href="/search-pass" { "Back to the search pass →" }
      },
   };
   shell(true, html! { section class="pass-card pass-center" { (body) } })
}

pub fn render_new_password(username: &str, password: &str) -> Markup {
   shell(true, html! {
       section class="pass-card" {
           h1 { "New password" }
           (login_box(username, password))
           p class="pass-notice is-bad" { "Save it now. It is shown once." }
           a class="pass-link" href="/search-pass" { "Back to the pass →" }
       }
   })
}

pub fn render_login(message: Option<Notice>) -> Markup {
   shell(true, html! {
       section class="pass-card" {
           h1 { "Log in" }
           p class="pass-hint" { "Use the username and password from your search pass." }
           (notice(message))
           form class="pass-stack" method="post" action="/search-pass/login" {
               div class="pass-field" {
                   label for="pass-username" { "Username" }
                   input id="pass-username" name="username" type="text" autocomplete="username"
                       autocapitalize="none" spellcheck="false" maxlength="64" required;
               }
               div class="pass-field" {
                   label for="pass-password" { "Password" }
                   input id="pass-password" name="password" type="password" autocomplete="current-password"
                       maxlength="256" required;
               }
               button type="submit" class="pass-btn is-block" { "Log in" }
           }
       }
       p class="pass-foot" {
           a href="/search-pass/forgot" { "Forgot your login?" }
           " · "
           a href="/search-pass" { "Get a search pass" }
       }
   })
}

pub fn render_forgot(mail: bool, sent: bool) -> Markup {
   let body = if !mail {
      html! { p class="pass-notice is-info" { "Email is not set up on this instance, so a lost login cannot be sent." } }
   } else if sent {
      html! {
          p class="pass-notice is-good" {
              "If a pass uses that email, a new password is on its way. "
              "It can take a few minutes, and only one is sent every 10 minutes."
          }
          a class="pass-link" href="/search-pass/login" { "Log in →" }
      }
   } else {
      html! {
          p class="pass-hint" { "Enter the email you gave when buying. A new password is sent there; the old one stops working." }
          form class="pass-stack" method="post" action="/search-pass/forgot" {
              div class="pass-field" {
                  label for="pass-email" { "Email" }
                  input id="pass-email" name="email" type="email" autocomplete="email" maxlength="254" required;
              }
              button type="submit" class="pass-btn is-block" { "Send a new password" }
          }
          p class="pass-hint" { "Anonymous passes have no email. Sign in from the browser you paid from instead." }
      }
   };
   shell(true, html! {
       section class="pass-card" {
           h1 { "Forgot your login" }
           (body)
       }
       p class="pass-foot" { a href="/search-pass/login" { "Back to log in" } }
   })
}