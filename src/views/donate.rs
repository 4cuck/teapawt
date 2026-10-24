use maud::{Markup, html};

use crate::donations::{MonthProgress, format_usd};

pub fn donate_drive_banner(progress: &MonthProgress) -> Markup {
   html! {
       div class="donate-drive donate-drive-meter" {
           (money_meter(progress, true))
       }
   }
}

pub fn render_donate_page(
   progress: &MonthProgress,
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
               "xitter.cf needs " (format_usd(progress.goal_cents)) " every month for accounts and proxies. "
               "The counter resets on the 23rd. "
               "About 1,000 accounts are in rotation. Scraping from one IP is allowed. "
               "Cloudflare Turnstile may turn on if someone starts spamming searches. "
               "If you scrape from multiple IPs through proxies, Turnstile turns on for about 3 to 5 minutes, depending on how many requests arrive at once. "
               "Choose an amount from $1 to $10,000."
           }
           (money_meter(progress, false))
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
               p class="donate-note" {
                   "If you are not satisfied with this site and do not want to donate because a feature is missing or it is too slow, "
                   a href="https://github.com/4cuck/teapawt/issues" target="_blank" rel="noopener noreferrer" { "request it on GitHub" }
                   ". I will add it as quickly as I can."
               }
               p class="donate-note" { (posting_plans()) }
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
               p { "Your donation was received. It keeps xitter.cf online." }
           } @else if provider == "nowpayments" {
               p { "If the crypto payment has been sent, it shows up here after the network confirms it." }
           } @else {
               p { "If the payment went through, it will show up in a moment. You can close this page." }
           }
           p { a href="/" { "Back to the front page" } }
       }
   }
}

fn posting_plans() -> Markup {
   html! {
       "It costs about $1,250 to add posting to this Nitter instance. Searching and viewing tweets will use separate account pools, so they do not mix and break the whole site. Posting, liking, and reposting would go through our residential proxies, consequence free. I will also add hundreds of thousands of accounts, about 100,000 of them for posting, so you can ban evade on X through this site. "
       "Posting onto X through those accounts will be added, but slowly. If the money does not come in quickly, expect about 6 to 8 months, or up to a year. Musk's anti-bot system is why that takes so long. "
       a href="https://emusks.tiago.zip/internals/castle" target="_blank" rel="noopener noreferrer" { "This write-up" }
       " is an outdated example of that system."
   }
}

fn money_meter(progress: &MonthProgress, donate_link: bool) -> Markup {
   let raised = format_usd(progress.raised_cents);
   let goal = format_usd(progress.goal_cents);
   let percent = progress.percent();
   let bar = progress.bar_percent();
   let met = if progress.met() { " is-met" } else { "" };
   html! {
       section class=(format!("donate-meter{met}")) aria-label="This month's donations" {
           div class="donate-meter-top" {
               span class="donate-meter-live" { "This month" }
               @if progress.met() {
                   span class="donate-meter-status" { "Goal met" }
               }
               span class="donate-meter-percent" { (percent) "%" }
               @if donate_link {
                   a class="donate-meter-button" href="/donate" { "Donate" }
               }
           }
           div class="donate-meter-track" role="meter"
               aria-valuemin="0"
               aria-valuemax=(progress.goal_cents)
               aria-valuenow=(progress.raised_cents)
               aria-label=(format!("Raised {raised} of {goal} this month")) {
               div class="donate-meter-bar" style=(format!("width: {bar}%")) {}
           }
           p class="donate-meter-figures" {
               strong { (raised) }
               span { "of " span { (goal) } }
           }
           p class="donate-meter-reset" {
               "Resets "
               time datetime=(progress.reset_on) { (progress.reset_label) }
               " (UTC)"
           }
       }
   }
}
