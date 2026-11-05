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

pub struct BuyPage<'a> {
   pub price:   &'a str,
   pub methods: &'a Methods,
   pub mail:    bool,
   pub message: Option<&'a str>,
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
   pub message:       Option<&'a str>,
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

fn methods_form(methods: &Methods, price: &str, card: bool) -> Markup {
   html! {
       div class="donate-methods" {
           @if methods.card && card {
               button type="submit" name="provider" value="stripe" { "Card, " (price) " a month" }
           }
           @if methods.crypto {
               button type="submit" name="provider" value="nowpayments" { "Crypto, one month" }
           }
           @if methods.lzt {
               button type="submit" name="provider" value="lzt" { "LZT Market, one month" }
           }
       }
   }
}

pub fn render_buy(page: &BuyPage<'_>) -> Markup {
   html! {
       div class="overlay-panel donate-page pass-page" {
           h1 { "Search pass" }
           p {
               "A search pass skips the Cloudflare checkbox on search and on search RSS feeds. "
               "It costs " (page.price) " a month."
           }
           p {
               "After paying you get a username and password, and this browser stays signed in. "
               "Signed in, you can whitelist IP addresses, such as the server your feed reader runs on, "
               "so search RSS works there without cookies."
           }
           @if let Some(message) = page.message {
               p class="donate-status donate-error" { (message) }
           }
           @if page.methods.any() {
               form class="donate-form" method="post" action="/search-pass/buy" {
                   @if page.mail {
                       label for="pass-email" { "Email (optional)" }
                       input id="pass-email" name="email" type="email" autocomplete="email"
                           maxlength="254" placeholder="you@example.com";
                       p class="donate-note" {
                           "Your login is sent there in case you lose it. Leave it empty to buy anonymously."
                       }
                   }
                   p class="donate-note" {
                       "Anonymous passes need cookies. The login is shown once after payment and this "
                       "browser's session keeps you signed in, so do not block cookies for this site."
                   }
                   (methods_form(page.methods, page.price, true))
               }
               p class="donate-note" {
                   "Card renews every month until you stop it. Crypto and LZT Market pay for one month at a time."
               }
           } @else {
               p class="donate-status" { "Payments are not set up on this instance." }
           }
           p class="donate-note" {
               a href="/search-pass/login" { "Log in" }
               " to renew a pass or change its addresses. "
               a href="/search-pass/forgot" { "Forgot your login?" }
           }
       }
   }
}

pub fn render_account(page: &AccountPage<'_>) -> Markup {
   let full = page.addresses.len() >= page.max_addresses;
   let current_listed = page
      .current
      .is_some_and(|current| page.addresses.iter().any(|address| address == current));
   html! {
       div class="overlay-panel donate-page pass-page" {
           h1 { "Your search pass" }
           @if let Some(message) = page.message {
               p class="donate-status" { (message) }
           }
           p { "Signed in as " strong { (page.username) } "." }
           @if page.active {
               p { "Search skips the checkbox until " strong { (page.paid_until) } "." }
           } @else {
               p class="donate-error" { "This pass ran out on " (page.paid_until) ". Add time below to use it again." }
           }

           @if page.renews {
               p class="donate-note" { "Your card renews it every month." }
               form method="post" action="/search-pass/renewal" {
                   button type="submit" { "Stop card renewal" }
               }
           } @else if page.card {
               p class="donate-note" { "Card renewal is stopped. The pass ends on the date above." }
           }

           h2 { "Add time" }
           form class="donate-form" method="post" action="/search-pass/buy" {
               (methods_form(page.methods, page.price, !page.renews))
           }

           h2 { "Whitelisted IP addresses" }
           p class="donate-note" {
               "Requests from these addresses skip the checkbox while the pass is paid up, with no cookies. "
               "Use it for a feed reader or a script. An IPv6 address covers its whole /64. "
               "Up to " (page.max_addresses) "."
           }
           @if page.addresses.is_empty() {
               p class="donate-note" { "None yet." }
           } @else {
               ul class="pass-list" {
                   @for address in page.addresses {
                       li {
                           code { (address) }
                           form method="post" action="/search-pass/ips" {
                               input type="hidden" name="action" value="remove";
                               input type="hidden" name="ip" value=(address);
                               button type="submit" { "Remove" }
                           }
                       }
                   }
               }
           }
           @if !full {
               @if let Some(current) = page.current.filter(|_| !current_listed) {
                   form method="post" action="/search-pass/ips" {
                       input type="hidden" name="action" value="current";
                       button type="submit" { "Add this connection (" (current) ")" }
                   }
               }
               form class="donate-form" method="post" action="/search-pass/ips" {
                   input type="hidden" name="action" value="add";
                   label for="pass-ip" { "Another address" }
                   input id="pass-ip" name="ip" type="text" inputmode="text" autocomplete="off"
                       maxlength="64" placeholder="203.0.113.7 or 2001:db8::1" required;
                   button type="submit" { "Add" }
               }
           }

           @if page.mail {
               h2 { "Recovery email" }
               form class="donate-form" method="post" action="/search-pass/email" {
                   label for="pass-email" { "Email" }
                   input id="pass-email" name="email" type="email" autocomplete="email"
                       maxlength="254" value=(page.email) placeholder="you@example.com";
                   p class="donate-note" { "A forgotten login is sent here. Leave it empty to remove it." }
                   button type="submit" { "Save" }
               }
           }

           h2 { "Password" }
           form method="post" action="/search-pass/password" {
               button type="submit" { "Make a new password" }
           }
           p class="donate-note" { "The old one stops working. Signed-in browsers stay signed in." }

           form class="pass-logout" method="post" action="/search-pass/logout" {
               button type="submit" { "Log out" }
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

pub fn render_return(view: &Return<'_>) -> Markup {
   html! {
       div class="overlay-panel donate-page pass-page" {
           @match view {
               Return::Waiting => {
                   h1 { "Waiting for the payment" }
                   p { "This page checks again every 10 seconds. Crypto can take a while to confirm." }
                   p class="donate-note" { "You can close it. Opening " a href="/search-pass" { "the search pass page" } " in this browser later picks up the login." }
               },
               Return::Issued { username, password, paid_until, emailed } => {
                   h1 { "Your search pass is ready" }
                   p { "Search skips the checkbox until " strong { (paid_until) } ". This browser is signed in." }
                   (login_box(username, password))
                   @if let Some(to) = emailed {
                       p class="donate-note" { "A copy is on its way to " (to) "." }
                   } @else {
                       p class="donate-error" { "Save this login now. It is shown once, and with no email it cannot be sent again." }
                   }
                   p { a href="/search-pass" { "Manage the pass and its IP addresses" } }
               },
               Return::SignedIn { username, paid_until } => {
                   h1 { "Your search pass is ready" }
                   p { "Signed in as " strong { (username) } ". Search skips the checkbox until " strong { (paid_until) } "." }
                   p class="donate-note" { "The password was shown or emailed already. You can make a new one on the pass page." }
                   p { a href="/search-pass" { "Manage the pass and its IP addresses" } }
               },
               Return::Renewed { paid_until } => {
                   h1 { "Thank you" }
                   p { "The pass now runs until " strong { (paid_until) } "." }
                   p { a href="/search-pass" { "Back to the pass" } }
               },
               Return::Emailed { to } => {
                   h1 { "Payment received" }
                   p { "The login is on its way to " (to) ". It can take a few minutes." }
                   p { a href="/search-pass/login" { "Log in" } }
               },
               Return::OtherBrowser => {
                   h1 { "Payment received" }
                   p { "Open this page in the browser you paid from. An anonymous pass is handed to that browser's cookies only." }
               },
               Return::Unknown => {
                   h1 { "Order not found" }
                   p { a href="/search-pass" { "Back to the search pass" } }
               },
           }
       }
   }
}

pub fn render_new_password(username: &str, password: &str) -> Markup {
   html! {
       div class="overlay-panel donate-page pass-page" {
           h1 { "New password" }
           (login_box(username, password))
           p class="donate-error" { "Save it now. It is shown once." }
           p { a href="/search-pass" { "Back to the pass" } }
       }
   }
}

pub fn render_login(message: Option<&str>) -> Markup {
   html! {
       div class="overlay-panel donate-page pass-page" {
           h1 { "Log in" }
           @if let Some(message) = message {
               p class="donate-status donate-error" { (message) }
           }
           form class="donate-form" method="post" action="/search-pass/login" {
               label for="pass-username" { "Username" }
               input id="pass-username" name="username" type="text" autocomplete="username"
                   autocapitalize="none" spellcheck="false" maxlength="64" required;
               label for="pass-password" { "Password" }
               input id="pass-password" name="password" type="password" autocomplete="current-password"
                   maxlength="256" required;
               button type="submit" { "Log in" }
           }
           p class="donate-note" {
               a href="/search-pass/forgot" { "Forgot your login?" }
               " · "
               a href="/search-pass" { "Get a search pass" }
           }
       }
   }
}

pub fn render_forgot(mail: bool, sent: bool) -> Markup {
   html! {
       div class="overlay-panel donate-page pass-page" {
           h1 { "Forgot your login" }
           @if !mail {
               p { "Email is not set up on this instance, so a lost login cannot be sent." }
           } @else if sent {
               p { "If a pass uses that email, a new password is on its way. It can take a few minutes, and only one is sent every 10 minutes." }
               p { a href="/search-pass/login" { "Log in" } }
           } @else {
               p { "Enter the email you gave when buying. A new password is sent there; the old one stops working." }
               form class="donate-form" method="post" action="/search-pass/forgot" {
                   label for="pass-email" { "Email" }
                   input id="pass-email" name="email" type="email" autocomplete="email" maxlength="254" required;
                   button type="submit" { "Send a new password" }
               }
               p class="donate-note" { "Anonymous passes have no email. Sign in from the browser you paid from instead." }
           }
       }
   }
}
