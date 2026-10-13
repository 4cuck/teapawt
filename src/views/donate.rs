use maud::{Markup, html};

use crate::donations::{self, format_usd};

pub fn donate_drive_banner(raised_cents: i64, goal_cents: i64) -> Markup {
   html! {
       aside class="donate-drive" {
           strong { "nitter.cf needs " (format_usd(goal_cents)) " by October 23 to stay online." }
           p {
               "Servers, bandwidth, and the accounts that keep this instance up are due then. "
               @if raised_cents > 0 {
                   "Raised " (format_usd(raised_cents)) " of " (format_usd(goal_cents)) ". "
               }
               a href="/donate" { "Donate any amount" }
               " up to $10,000."
           }
       }
   }
}

pub fn render_donate_page(
   raised_cents: i64,
   goal_cents: i64,
   stripe: bool,
   crypto: bool,
   lzt: bool,
   message: Option<&str>,
   canceled: bool,
) -> Markup {
   let any = stripe || crypto || lzt;
   html! {
       div class="overlay-panel donate-page" {
           h1 { "Donate" }
           p {
               "nitter.cf needs " (format_usd(goal_cents)) " by October 23, 2026 to keep running. "
               @if raised_cents > 0 {
                   "Raised " (format_usd(raised_cents)) " so far. "
               }
               "Choose an amount from $1 to $10,000."
           }
           @if canceled {
               p class="donate-status" { "The payment was canceled. Nothing was charged." }
           }
           @if let Some(message) = message {
               p class="donate-status donate-error" { (message) }
           }
           @if any {
               form class="donate-form" method="post" action="/donate" {
                   label for="donate-amount" { "Amount in USD" }
                   input id="donate-amount" name="amount" type="text" inputmode="decimal"
                       autocomplete="off" required placeholder="25.00" value="25";
                   div class="donate-methods" {
                       @if stripe {
                           button type="submit" name="provider" value="stripe" { "Card" }
                       }
                       @if crypto {
                           button type="submit" name="provider" value="nowpayments" { "Crypto" }
                       }
                       @if lzt {
                           button type="submit" name="provider" value="lzt" { "LZT Market" }
                       }
                   }
               }
               p class="donate-note" {
                   "Card payments go through Stripe. Crypto goes through NOWPayments. "
                   "LZT Market uses a balance on that site. "
                   "The minimum is $1 and the maximum is $10,000."
               }
           } @else {
               p class="donate-status" { "Donations are not set up on this instance yet." }
           }
       }
   }
}

pub fn render_thanks(confirmed: bool, provider: &str) -> Markup {
   html! {
       div class="overlay-panel donate-page" {
           h1 { "Thank you" }
           @if confirmed {
               p { "Your donation was received. It keeps nitter.cf online." }
           } @else if provider == "nowpayments" {
               p { "If the crypto payment has been sent, it shows up here after the network confirms it." }
           } @else {
               p { "If the payment went through, it will show up in a moment. You can close this page." }
           }
           p { a href="/" { "Back to the front page" } }
       }
   }
}

pub fn campaign_is_open(deadline: &str) -> bool {
   donations::campaign_open(deadline)
}
