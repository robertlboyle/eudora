//! What Eudora knows from the start about well-known senders, and how a subject tells a
//! notification from marketing.
//!
//! Bulk mail is split into notifications and marketing (`triage_decide::is_marketing`) by what
//! mass mailings carry: an unsubscribe link, Precedence: bulk, a marketing service. Some senders
//! don't follow that: Starlink sends offers with no unsubscribe link, Reddit sends "replied to
//! your comment" with one. So, in this order: a known sender's address says the kind; else, for
//! a sender that mixes both, the subject's words; else the marks.

use crate::triage::Signals;

/// Which kind of bulk mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Notification,
    Marketing,
}

/// One well-known sender: its domain (an address's domain or any domain above it), the part of
/// the address before the @ it applies to (`""` = any), what its mail is (`None` = it sends both:
/// the subject decides), and the name its folder gets.
pub struct Known {
    pub domain: &'static str,
    pub local: &'static str,
    pub kind: Option<Kind>,
    pub name: &'static str,
}

const fn k(domain: &'static str, local: &'static str, kind: Option<Kind>, name: &'static str) -> Known {
    Known { domain, local, kind, name }
}

const N: Option<Kind> = Some(Kind::Notification);
const M: Option<Kind> = Some(Kind::Marketing);
const BOTH: Option<Kind> = None;

/// The table, most specific first (an address part before "" for the same domain).
pub const KNOWN: &[Known] = &[
    // shopping and deliveries
    k("amazon.com", "order-update", N, "Amazon"),
    k("amazon.com", "auto-confirm", N, "Amazon"),
    k("amazon.com", "shipment-tracking", N, "Amazon"),
    k("amazon.com", "ship-confirm", N, "Amazon"),
    k("amazon.com", "payments-messages", N, "Amazon"),
    k("amazon.com", "account-update", N, "Amazon"),
    k("amazon.com", "digital-no-reply", N, "Amazon"),
    k("amazon.com", "store-news", M, "Amazon"),
    k("amazon.com", "", BOTH, "Amazon"),
    k("ups.com", "", N, "UPS"),
    k("fedex.com", "", N, "FedEx"),
    k("usps.com", "", N, "USPS"),
    k("dhl.com", "", N, "DHL"),
    k("squarespace.com", "", BOTH, "Squarespace"),
    k("shopify.com", "", BOTH, "Shopify"),
    k("ebay.com", "", BOTH, "eBay"),
    // signatures, work tools, HR
    k("docusign.net", "", N, "DocuSign"),
    k("docusign.com", "", N, "DocuSign"),
    k("adobesign.com", "", N, "Adobe Sign"),
    k("echosign.com", "", N, "Adobe Sign"),
    k("atlassian.net", "jira", N, "Jira"),
    k("atlassian.net", "confluence", N, "Confluence"),
    k("atlassian.net", "", N, "Atlassian"),
    k("atlassian.com", "", BOTH, "Atlassian"),
    k("bamboohr.com", "", N, "BambooHR"),
    k("github.com", "notifications", N, "GitHub"),
    k("github.com", "noreply", N, "GitHub"),
    k("github.com", "", BOTH, "GitHub"),
    k("gitlab.com", "", BOTH, "GitLab"),
    k("slack.com", "", BOTH, "Slack"),
    k("zoom.us", "", BOTH, "Zoom"),
    k("calendly.com", "", N, "Calendly"),
    k("dropbox.com", "", BOTH, "Dropbox"),
    k("box.com", "", BOTH, "Box"),
    // accounts, security, money
    k("accountprotection.microsoft.com", "", N, "Microsoft"),
    k("microsoft.com", "", BOTH, "Microsoft"),
    k("accounts.google.com", "", N, "Google"),
    k("google.com", "calendar-notification", N, "Google Calendar"),
    k("google.com", "", BOTH, "Google"),
    k("apple.com", "", BOTH, "Apple"),
    k("paypal.com", "service", N, "PayPal"),
    k("paypal.com", "", BOTH, "PayPal"),
    k("intuit.com", "", BOTH, "Intuit"),
    k("notification.intuit.com", "", N, "QuickBooks"),
    k("stripe.com", "", BOTH, "Stripe"),
    k("brex.com", "", BOTH, "Brex"),
    k("chase.com", "", BOTH, "Chase"),
    k("americanexpress.com", "", BOTH, "American Express"),
    k("aexp.com", "", BOTH, "American Express"),
    k("bankofamerica.com", "", BOTH, "Bank of America"),
    k("wellsfargo.com", "", BOTH, "Wells Fargo"),
    k("capitalone.com", "", BOTH, "Capital One"),
    k("citi.com", "", BOTH, "Citi"),
    k("td.com", "", BOTH, "TD Bank"),
    k("netflix.com", "", BOTH, "Netflix"),
    k("spotify.com", "", BOTH, "Spotify"),
    k("verizon.com", "", BOTH, "Verizon"),
    k("cynet.com", "cynet-soc", N, "Cynet SOC"),
    k("huntress.io", "", N, "Huntress"),
    k("arcticwolf.com", "", BOTH, "Arctic Wolf"),
    k("sentinelone.com", "", BOTH, "SentinelOne"),
    k("crowdstrike.com", "", BOTH, "CrowdStrike"),
    k("sophos.com", "", BOTH, "Sophos"),
    // social
    k("linkedin.com", "messages-noreply", N, "LinkedIn"),
    k("linkedin.com", "invitations", N, "LinkedIn"),
    k("linkedin.com", "notifications-noreply", N, "LinkedIn"),
    k("linkedin.com", "security-noreply", N, "LinkedIn"),
    k("linkedin.com", "jobs-listings", M, "LinkedIn"),
    k("linkedin.com", "editors-noreply", M, "LinkedIn"),
    k("linkedin.com", "", BOTH, "LinkedIn"),
    k("facebookmail.com", "", BOTH, "Facebook"),
    k("redditmail.com", "", BOTH, "Reddit"),
    k("reddit.com", "", BOTH, "Reddit"),
    k("nextdoor.com", "", BOTH, "Nextdoor"),
    k("x.com", "", BOTH, "X"),
    k("twitter.com", "", BOTH, "X"),
    k("youtube.com", "", BOTH, "YouTube"),
    // services that send offers without an unsubscribe link, or both kinds
    k("starlink.com", "", BOTH, "Starlink"),
    k("tesla.com", "", BOTH, "Tesla"),
    k("uber.com", "", BOTH, "Uber"),
    k("doordash.com", "", BOTH, "DoorDash"),
    k("united.com", "", BOTH, "United"),
    k("delta.com", "", BOTH, "Delta"),
    k("aa.com", "", BOTH, "American Airlines"),
    // marketing first and foremost
    k("travelzoo.com", "", M, "Travelzoo"),
    k("priceline.com", "", BOTH, "Priceline"),
    k("groupon.com", "", M, "Groupon"),
];

/// The known sender for an address: its domain or any domain above it, the address's own part
/// first.
pub fn known(address: &str) -> Option<&'static Known> {
    let address = address.to_lowercase();
    let (local, domain) = address.split_once('@')?;
    KNOWN.iter().find(|k| {
        (domain == k.domain || domain.ends_with(&format!(".{}", k.domain))) && (k.local.is_empty() || local == k.local || local.starts_with(&format!("{}+", k.local)))
    })
}

/// Words in a subject that mean a notification: something that happened to you or your account.
pub const NOTIFICATION_WORDS: &[&str] = &[
    "order", "orders", "shipped", "shipment", "shipping", "delivered", "delivery", "out for delivery", "arriving", "tracking",
    "receipt", "invoice", "payment", "payments", "statement", "bill", "billing", "refund", "charge", "deposit",
    "password", "verification", "verify", "verification code", "security alert", "sign-in", "signed in", "new sign-in", "login", "two-factor",
    "signature", "please sign", "signed", "completed", "document", "envelope",
    "ticket", "case", "request", "reminder", "appointment", "confirmed", "confirmation", "reservation", "itinerary", "booking", "check-in", "boarding",
    "replied", "replied to", "mentioned you", "commented", "comment", "new message", "message from", "invited you", "invitation", "shared with you",
    "reacted to", "your reply", "your post", "your comment", "found your", "liked your", "new device", "snapshot", "deposit", "declined",
    "your account", "account", "maintenance", "outage", "incident", "alert", "backup", "report", "renewal", "renews", "expires", "expiring", "expired",
    "subscription", "trial", "access", "update on", "status",
];

/// Words in a subject that mean marketing: an offer or a mass mailing.
pub const MARKETING_WORDS: &[&str] = &[
    "% off", "off sitewide", "sale", "free", "deal", "deals", "offer", "offers", "save", "savings", "discount", "coupon", "promo",
    "limited time", "exclusive", "new arrivals", "shop", "shop now", "buy", "bestsellers", "gift", "gifts",
    "webinar", "event", "join us", "register", "upgrade", "discover", "introducing", "new feature", "don't miss", "dont miss", "last chance",
    "ends tonight", "today only", "special", "black friday", "cyber monday", "newsletter", "digest", "trending", "top stories", "popular",
    "recommended", "you might like", "inspiration", "tips", "weekly", "monthly", "roundup", "eligible", "installation",
];

fn has_word(subject: &str, word: &str) -> bool {
    let s = subject.to_lowercase();
    let mut from = 0;
    while let Some(at) = s[from..].find(word) {
        let start = from + at;
        let end = start + word.len();
        let before = s[..start].chars().next_back();
        let after = s[end..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        // "% off" starts with a symbol: a digit before it is fine ("20% off")
        if (boundary(before) || word.starts_with('%')) && boundary(after) {
            return true;
        }
        from = end;
    }
    false
}

/// What the subject's words say: one kind when its words are of one kind only.
pub fn subject_kind(subject: &str) -> Option<(Kind, &'static str)> {
    let note = NOTIFICATION_WORDS.iter().copied().find(|w| has_word(subject, w));
    let promo = MARKETING_WORDS.iter().copied().find(|w| has_word(subject, w));
    match (note, promo) {
        (Some(w), None) => Some((Kind::Notification, w)),
        (None, Some(w)) => Some((Kind::Marketing, w)),
        _ => None,
    }
}

/// Why a hint was given, for the user.
pub struct Hint {
    pub kind: Kind,
    /// The rule it belongs to: "known-senders" or "subject-words".
    pub rule: &'static str,
    pub why: String,
}

/// Whether the message proves it comes from its From domain: the domain's DMARC check passed,
/// or a DKIM signature by that very organization checked out. Anyone can write
/// `order-update@amazon.com` in a From line.
pub fn verified(s: &Signals) -> bool {
    if s.dmarc.as_deref() == Some("pass") {
        return true;
    }
    let org = crate::triage_decide::org_domain(&s.from_domain);
    s.dkim.as_deref() == Some("pass") && !org.is_empty() && s.dkim_domains.iter().any(|d| crate::triage_decide::org_domain(d) == org)
}

/// A message claiming a known sender while its checks failed: (the name it claims). Only real
/// failures count (no results at all, as in old imported mail, proves nothing either way).
pub fn forged(s: &Signals) -> Option<&'static str> {
    let k = known(&s.from)?;
    let failed = s.dmarc.as_deref() == Some("fail") || (s.dkim.as_deref() != Some("pass") && s.spf.as_deref() == Some("fail"));
    (failed && !verified(s)).then_some(k.name)
}

/// The kind a bulk message is, when a known sender or its subject says so (`known_on` /
/// `words_on`: those rules switched on and not excepted for this message). A known sender
/// counts only when the message proves it's from them (`verified`).
pub fn hint(s: &Signals, subject: &str, known_on: bool, words_on: bool) -> Option<Hint> {
    let known = (known_on && verified(s)).then(|| known(&s.from)).flatten();
    if let Some(k) = known {
        if let Some(kind) = k.kind {
            let what = if kind == Kind::Notification { "notifications" } else { "marketing" };
            return Some(Hint { kind, rule: "known-senders", why: format!("{} sends {what} from {}", k.name, s.from) });
        }
    }
    if words_on {
        if let Some((kind, word)) = subject_kind(subject) {
            let what = if kind == Kind::Notification { "a notification" } else { "marketing" };
            let who = known.map(|k| format!("{} sends both kinds; ", k.name)).unwrap_or_default();
            return Some(Hint { kind, rule: "subject-words", why: format!("{who}the subject says “{word}”: {what}") });
        }
    }
    None
}

/// The folder name a known sender's mail gets ("Amazon", "DocuSign").
pub fn folder_name(sender: &str) -> Option<&'static str> {
    // a sender key is an organizational domain (verified known senders) or an address (shared
    // domains, and unverified mail claiming a known sender: never named after it)
    if sender.contains('@') {
        return None;
    }
    let domain = sender.to_lowercase();
    KNOWN.iter().find(|k| domain == k.domain || domain.ends_with(&format!(".{}", k.domain)) || k.domain.ends_with(&format!(".{domain}"))).map(|k| k.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quirks_found_in_real_mail() {
        // Starlink: an offer without an unsubscribe link; Reddit: a reply with one
        assert_eq!(subject_kind("Free Professional Installation in Your Area").map(|k| k.0), Some(Kind::Marketing));
        assert_eq!(subject_kind("u/someone replied to your comment in r/examplenet").map(|k| k.0), Some(Kind::Notification));
        assert_eq!(subject_kind("Your order #123 has shipped").map(|k| k.0), Some(Kind::Notification));
        assert_eq!(subject_kind("20% off everything this weekend").map(|k| k.0), Some(Kind::Marketing));
        assert_eq!(subject_kind("Your order: 20% off your next one"), None, "both kinds: the marks decide");
        assert_eq!(subject_kind("Hello"), None);
        // "sale" isn't found inside "wholesale"
        assert_eq!(subject_kind("Wholesale price list"), None);
    }

    #[test]
    fn known_senders_by_address_then_domain() {
        assert_eq!(known("order-update@amazon.com").map(|k| (k.kind, k.name)), Some((N, "Amazon")));
        assert_eq!(known("store-news@amazon.com").map(|k| k.kind), Some(M));
        assert_eq!(known("dse@docusign.net").map(|k| k.name), Some("DocuSign"));
        assert_eq!(known("jira@acmefiber.atlassian.net").map(|k| k.name), Some("Jira"));
        assert_eq!(known("no-reply@is.email.nextdoor.com").map(|k| k.kind), Some(BOTH));
        assert!(known("someone@notatlassian.net").is_none());
        assert_eq!(folder_name("amazon.com"), Some("Amazon"));
        assert_eq!(folder_name("docusign.net"), Some("DocuSign"));
        assert_eq!(folder_name("unknown.example"), None);
    }
}
