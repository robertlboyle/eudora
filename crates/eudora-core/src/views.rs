//! Synthetic Views (agreed 2026-10-14): Receipts, Invoices, Renewals, Bank Transactions, New
//! Accounts, 2FA codes… Each view is a list of messages tagged in the database (`tags`), not a
//! folder: a message stays where it's filed and shows in every view that fits it.
//!
//! Tagging reads only what the database keeps of each message (subject, sender, the preview
//! text), never the packs, so the whole library is tagged in seconds; new mail is tagged as it
//! arrives (the download), older mail a batch at a time. What the spam filter judged spam isn't
//! tagged (a phishing "verify your account" isn't a New Account). The rules are words and
//! senders for now; the built-in language model may decide later (the project's design notes).

use rusqlite::params;
use serde::Serialize;

use crate::error::Result;
use crate::library::{Library, MessageRow, PageCursor};

/// A view: its key (stored in `tags`), its name, and what puts a message in it.
pub struct View {
    pub key: &'static str,
    pub name: &'static str,
    /// Phrases in the subject (lowercase; whole words at their ends).
    subject: &'static [&'static str],
    /// Phrases in the preview text, counted only with `text_needs` (a code, a sender).
    text: &'static [&'static str],
    /// Only from these senders' domains (empty: anyone).
    domains: &'static [&'static str],
    /// The preview must hold a number of 4 to 8 digits (a code).
    needs_code: bool,
    /// Not when the subject has any of these ("payment declined" isn't a receipt).
    not: &'static [&'static str],
}

/// Raised whenever the rules below change: the library is tagged again from scratch.
pub const RULES_VERSION: &str = "15";

/// The setting naming the mailboxes the combined Views show (addresses, lowercase; JSON list).
/// Unset: the user's own accounts (not shared mailboxes: a colleague's mail isn't the user's
/// receipts).
pub const VIEWS_ACCOUNTS: &str = "views-accounts";

/// Banks, cards and payment services (by their domain and its subdomains).
const MONEY: &[&str] = &[
    "chase.com", "americanexpress.com", "aexp.com", "bankofamerica.com", "bofa.com", "wellsfargo.com", "capitalone.com", "citi.com", "citibank.com",
    "td.com", "tdbank.com", "usbank.com", "pnc.com", "discover.com", "ally.com", "schwab.com", "fidelity.com", "vanguard.com", "synchrony.com",
    "barclaycardus.com", "navyfederal.org", "santander.com", "citizensbank.com", "keybank.com", "regions.com", "truist.com", "paypal.com",
    "venmo.com", "zellepay.com", "stripe.com", "brex.com", "mercury.com", "wise.com", "intuit.com", "squareup.com",
];

/// Card processors and payment services telling a merchant (the user's own business) that money came
/// in: Payments Received.
const PROCESSORS: &[&str] = &[
    "authorize.net", "ippay.com", "paypal.com", "stripe.com", "squareup.com", "square.com", "braintreegateway.com", "braintreepayments.com",
    "intuit.com", "invoicecloud.net", "elavon.com", "worldpay.com", "fisglobal.com", "firstdata.com", "clover.com", "heartland.us", "paymentech.com",
    "2checkout.com", "verifone.com", "gocardless.com", "bill.com", "melio.com", "adyen.com", "nmi.com", "globalpayments.com",
    "tsys.com", "shopify.com", "checkout.com", "helcim.com", "payrix.com", "paysafe.com",
];

/// Brand names as they're written (a sender's organization domain → its name), for the views'
/// groups; others are taken from the spam filter's well-known senders, else the domain's name.
const BRANDS: &[(&str, &str)] = &[
    ("authorize.net", "Authorize.Net"), ("paypal.com", "PayPal"), ("stripe.com", "Stripe"), ("squareup.com", "Square"),
    ("braintreegateway.com", "Braintree"), ("ebay.com", "eBay"), ("amazon.com", "Amazon"), ("homedepot.com", "Home Depot"), ("facebook.com", "Meta"),
    ("facebookmail.com", "Meta"), ("united.com", "United"), ("gocardless.com", "GoCardless"), ("globalpayments.com", "Global Payments"),
];

/// The group a sender belongs to in a view: its brand ("Authorize.Net", "eBay"), from its
/// organization's domain (mail.authorize.net → authorize.net).
pub fn brand(from: &str) -> String {
    let domain = from.rsplit('@').next().unwrap_or("").to_lowercase();
    if domain.is_empty() {
        return "Unknown sender".into();
    }
    let org = crate::triage_decide::org_domain(&domain);
    if let Some((_, name)) = BRANDS.iter().find(|(d, _)| *d == org) {
        return (*name).into();
    }
    if let Some(k) = crate::triage_known::KNOWN.iter().find(|k| k.domain == org || org.ends_with(&format!(".{}", k.domain))) {
        return k.name.into();
    }
    let label = org.split('.').next().unwrap_or(&org);
    let mut c = label.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
}

/// Something that didn't happen isn't a receipt, a bill paid or a transaction.
const NOT_DONE: &[&str] = &["declined", "failed", "unsuccessful", "was not processed", "couldn't be processed", "could not be processed"];

/// Not money that moved: not done (`NOT_DONE`), or someone asking to be paid.
const NOT_PAID: &[&str] = &[
    "declined", "failed", "unsuccessful", "was not processed", "couldn't be processed", "could not be processed",
    "payment request", "request for payment", "requested a payment", "requests a payment", "requesting a payment", "requesting payment", "payment requested",
    "amount due", "payment due", "past due", "pay now", "please pay",
];

// ------------------------------------------------------------------ Red Alert

/// Red Alert. Found in two steps: phrases (or a government sender) make a message a candidate
/// (`ALERT_CHECK`); reading its headers then confirms it (`Library::confirm_alerts`), so a fake
/// "IRS" email or a marketer's "URGENT" never is one.
pub const ALERT_CHECK: &str = "red-alert?";

/// Alarm phrases in a subject (whole words).
const ALARMS: &[&str] = &[
    // shut-offs
    "disconnect notice", "disconnection notice", "notice of disconnection", "will be disconnected", "scheduled for disconnection", "shut off", "shutoff", "shut-off",
    "termination of service", "service will be terminated", "service termination", "suspension of service", "service will be suspended",
    // defaults, collections, missed payments
    "notice of default", "default notice", "in default", "past due", "seriously past due", "final notice", "final demand", "collections", "collection agency",
    "sent to collections", "delinquent", "delinquency", "missed payment", "payment missed", "missed your payment", "late payment", "payment is late", "overdue",
    "returned payment", "payment returned", "nsf", "insufficient funds",
    // legal
    "lawsuit", "summons", "subpoena", "court date", "notice of hearing", "legal action", "judgment", "lien", "levy", "garnishment", "complaint filed",
    "notice of intent", "cease and desist", "demand letter",
    // taxes and regulators
    "irs", "internal revenue service", "notice cp", "tax due", "taxes due", "balance due", "delinquent tax", "late filing", "penalty", "penalties",
    "regulatory fee", "red light", "form 499", "universal service", "federal communications commission", "department of revenue", "department of taxation",
    "franchise tax", "comptroller", "secretary of state", "good standing", "revocation", "forfeiture", "license suspended", "license revoked",
];

/// Phrases that count only from people the user corresponds with ("someone saying it is an
/// emergency").
// ("critical" isn't one: security alerts and maintenance notices say it all day)
const PEOPLE_ALARMS: &[&str] = &["emergency", "urgent", "asap", "911", "help needed"];

/// Government senders: any notice from them is a candidate (confirmed only when it proves
/// it's theirs).
fn government(domain: &str) -> bool {
    let d = domain.to_lowercase();
    // (states, counties and towns: co.sample.ca.us, twp.sample.pa.us, state.ny.us)
    let local_us = d.strip_suffix(".us").and_then(|rest| rest.rsplit('.').next()).is_some_and(|state| state.len() == 2 && state.chars().all(|c| c.is_ascii_alphabetic()))
        && d.matches('.').count() >= 2;
    d.ends_with(".gov") || d.ends_with(".mil") || local_us || d.ends_with(".gov.us")
        || ["irs.gov", "ssa.gov", "ftc.gov", "sec.gov", "fcc.gov", "dol.gov", "usac.org", "finra.org"].iter().any(|g| d == *g || d.ends_with(&format!(".{g}")))
}

/// Government:
/// mail from a government sender that proves it's theirs (`proven`: DMARC or their own
/// signature; old mail with no checks at all counts too), so a fake "IRS" never lands there.
pub fn is_government(from: &str, proven: bool) -> bool {
    proven && government(from.rsplit('@').next().unwrap_or(""))
}

/// A government message is a candidate when it names a problem (a meeting notice isn't one).
const GOVERNMENT_NOTICES: &[&str] = &["past due", "penalty", "penalties", "delinquent", "action required", "deadline", "red light", "suspension", "suspended", "audit", "revocation", "revoked", "debt", "balance due", "final notice", "noncompliance", "non-compliance", "violation", "default", "lien", "levy"];

/// Security notices: advisories and patch notices in the subject.
const SECURITY_NOTICES: &[&str] = &[
    "security advisory", "security advisories", "security alert", "security alerts", "security bulletin", "security bulletins", "security update", "security updates",
    "security patch", "security patches", "security fix", "security fixes", "security release", "security notice", "security notification", "psirt",
    "vulnerability", "vulnerabilities", "known exploited", "actively exploited", "exploited in the wild", "zero-day", "zero day", "0-day", "cve",
    "patch tuesday", "critical patch", "critical update", "critical updates", "emergency patch", "out-of-band update", "hotfix", "patch released", "patches released",
    "patch now", "patch available", "firmware update", "ics advisory", "ics advisories", "cybersecurity advisory", "cisa",
];

/// What a security notice isn't: a security product's detections ("files found on servers"),
/// and vendors' marketing about security.
const NOT_SECURITY_NOTICES: &[&str] = &[
    "malicious", "malware detected", "threat detected", "infected", "quarantined", "suspicious file", "suspicious activity", "binary", "risk -",
    "webinar", "register", "registration", "sale", "free trial", "ebook", "e-book", "whitepaper", "white paper", "survey", "summit", "conference", "podcast",
    "newsletter", "digest", "weekly", "monthly", "roundup", "trends", "predictions", "report:", "demo", "save your seat", "join us",
];

/// Managed detection and response (SOC) services, whose alerts are about the company's own
/// computers, not advisories.
fn security_product(domain: &str) -> bool {
    let d = domain.to_lowercase();
    [
        "cynet.com", "cymail.io", "huntress.io", "huntress.com", "arcticwolf.com", "blackpointcyber.com", "expel.io", "expel.com",
        "redcanary.com", "esentire.com", "todyl.com", "coro.net",
    ]
    .iter()
    .any(|p| d == *p || d.ends_with(&format!(".{p}")))
}

/// Whether the subject reads like a security advisory or patch notice (the first, cheap step).
fn security_subject(subject: &str) -> bool {
    let s = subject.to_lowercase();
    SECURITY_NOTICES.iter().any(|p| has_phrase(&s, p)) && !NOT_SECURITY_NOTICES.iter().any(|p| has_phrase(&s, p))
}

/// A forward or reply ("Fwd:", "FW:", "Re:"…) at the start of a subject.
fn forwarded(subject: &str) -> bool {
    let s = subject.trim_start().to_lowercase();
    ["fwd:", "fw:", "re:", "tr:", "wg:", "fwd :", "fw :"].iter().any(|p| s.starts_with(p))
}

/// A security notice: an advisory or patch notice from a sender that proves who it is (CISA,
/// a vendor), or one forwarded by someone the user writes to or a colleague (`own_domains`).
/// Never the user's own, never one failing its checks, never a security product's detections.
pub fn security_notice(subject: &str, s: &crate::triage::Signals, whitelisted: bool, own_domains: &[String], mine: bool) -> bool {
    if mine || !security_subject(subject) || s.dmarc.as_deref() == Some("fail") || security_product(&s.from_domain) {
        return false;
    }
    if crate::triage_known::forged(s).is_some() {
        return false;
    }
    let colleague = own_domains.iter().any(|d| crate::triage_decide::org_domain(&s.from_domain) == crate::triage_decide::org_domain(d));
    if forwarded(subject) && (whitelisted || colleague) {
        return true;
    }
    // a proven sender (CISA through GovDelivery, a vendor's PSIRT list); a colleague's own notice
    crate::triage_known::verified(s) || (whitelisted && !s.marketer) || colleague
}

/// Whether a message may be a Red Alert (the first, cheap step): what it says, or who says it.
pub fn alert_candidate(subject: &str, from: &str) -> bool {
    let s = subject.to_lowercase();
    let domain = from.rsplit('@').next().unwrap_or("");
    ALARMS.iter().chain(PEOPLE_ALARMS).any(|p| has_phrase(&s, p))
        || (government(domain) && GOVERNMENT_NOTICES.iter().any(|p| has_phrase(&s, p)))
        || (security_subject(subject) && !security_product(domain))
}

/// The second step, on the message's headers: whether a candidate really is a Red Alert.
/// `whitelisted`: the user corresponds with the sender; `mine`: the user sent it (never one:
/// their own replies and forwards aren't alerts to them).
pub fn confirm_alert(subject: &str, s: &crate::triage::Signals, whitelisted: bool, own_domains: &[String], mine: bool) -> bool {
    if mine {
        return false;
    }
    if security_notice(subject, s, whitelisted, own_domains, mine) {
        return true;
    }
    let subject = subject.to_lowercase();
    // failed its own checks: never (forged, or worse)
    if s.dmarc.as_deref() == Some("fail") {
        return false;
    }
    let proven = crate::triage_known::verified(s);
    if government(&s.from_domain) {
        return proven; // a real government notice proves it's theirs; a fake "IRS" doesn't
    }
    let alarm = ALARMS.iter().any(|p| has_phrase(&subject, p));
    let people_alarm = PEOPLE_ALARMS.iter().any(|p| has_phrase(&subject, p));
    // a notice, not a mailing: no unsubscribe, no list, not bulk, not a marketer (a known
    // sender that proves it's itself may still have an unsubscribe link: a bank, a utility)
    let mass = s.unsubscribe || s.list_id.is_some() || s.marketer || s.precedence.as_deref().is_some_and(|p| p.eq_ignore_ascii_case("bulk") || p.eq_ignore_ascii_case("list"));
    if whitelisted && !mass && (alarm || people_alarm) {
        return true; // someone the user writes to (a newsletter they once answered isn't)
    }
    if !alarm {
        return false; // "urgent" from a stranger isn't
    }
    let known = crate::triage_known::known(&s.from).is_some() && proven;
    !mass || known
}

pub const VIEWS: &[View] = &[
    View {
        key: "red-alert",
        name: "Red Alert",
        // (found by `alert_candidate` and `confirm_alerts`, not these rules)
        subject: &[],
        text: &[],
        domains: &[],
        needs_code: false,
        not: &[],
    },
    View {
        key: "maybe-spam",
        name: "Maybe Spam",
        // (mail the spam filter left in the Inbox though it wasn't sure: `Decision::is_maybe`)
        subject: &[],
        text: &[],
        domains: &[],
        needs_code: false,
        not: &[],
    },
    View {
        key: "payments",
        name: "Payments Received",
        subject: &["merchant email receipt", "billing receipt", "settlement report", "payment received", "you received a payment", "you've received a payment", "you've got money", "instant payment received", "notification of an instant payment", "notification of payment received", "new payment", "successful payment", "payment successful", "payout", "deposit notification", "transaction receipt"],
        text: &[],
        domains: PROCESSORS,
        needs_code: false,
        not: NOT_PAID,
    },
    View {
        key: "payments-sent",
        name: "Payments Sent",
        subject: &["you sent", "you paid", "you've sent", "payment sent", "money sent", "receipt for your payment", "your payment to", "your payment of", "payment to", "you made a payment", "bill payment", "payment confirmation", "payment scheduled", "autopay", "we received your payment", "thank you for your payment", "thanks for your payment", "zelle payment"],
        text: &[],
        domains: MONEY,
        needs_code: false,
        not: NOT_PAID,
    },
    View {
        key: "receipts",
        name: "Receipts",
        subject: &["receipt", "your order", "order confirmation", "order confirmed", "thank you for your order", "thanks for your order", "thank you for your purchase", "purchase confirmation", "payment received", "payment confirmation", "we received your payment", "your purchase"],
        // in the text: a store's e-receipt whose subject says only "Thank you for shopping at
        // Walgreens"
        text: &["your purchase receipt", "purchase receipt", "e-receipt", "ereceipt", "digital receipt", "electronic receipt", "receipt number", "receipt #", "purchase summary", "order receipt"],
        domains: &[],
        needs_code: false,
        not: NOT_PAID,
    },
    View {
        key: "invoices",
        name: "Invoices",
        subject: &["invoice", "your bill", "bill is ready", "bill is available", "statement is ready", "statement is available", "new statement", "amount due", "payment due", "past due", "billing statement", "e-bill", "payment request", "request for payment", "requested a payment", "requests a payment", "requesting a payment", "requesting payment", "payment requested"],
        text: &[],
        domains: &[],
        needs_code: false,
        not: NOT_DONE,
    },
    View {
        key: "renewals",
        name: "Renewals",
        subject: &["renewal", "renew", "renews", "auto-renew", "will renew", "subscription will", "expires soon", "is expiring", "will expire", "has expired", "expiration notice", "membership expires"],
        text: &[],
        domains: &[],
        needs_code: false,
        not: NOT_DONE,
    },
    View {
        key: "bank",
        name: "Bank Transactions",
        subject: &["transaction", "deposit", "withdrawal", "transfer", "payment", "purchase", "charge", "card was used", "balance", "alert", "you sent", "you received", "you've received", "money"],
        text: &[],
        domains: MONEY,
        needs_code: false,
        not: NOT_PAID,
    },
    View {
        key: "new-accounts",
        name: "New Accounts",
        subject: &["welcome to", "confirm your email", "verify your email", "confirm your email address", "verify your email address", "confirm your account", "activate your account", "account created", "your new account", "thanks for signing up", "thank you for signing up", "complete your registration", "finish setting up"],
        text: &[],
        domains: &[],
        needs_code: false,
        not: NOT_DONE,
    },
    View {
        key: "2fa",
        name: "Verification Codes",
        subject: &["verification code", "security code", "one-time code", "one time code", "one-time password", "one-time passcode", "passcode", "sign-in code", "login code", "log-in code", "your code", "authentication code", "2-step", "two-step", "two-factor", "otp"],
        text: &["verification code", "security code", "one-time", "your code is", "use this code", "enter this code"],
        domains: &[],
        needs_code: true,
        not: &[],
    },
    View {
        key: "legal",
        name: "Legal",
        // (found by `is_legal`: disclaimers, law firms, dockets and court dates)
        subject: &[],
        text: &[],
        domains: &[],
        needs_code: false,
        not: &[],
    },
    View {
        key: "government",
        name: "Government",
        // (found by `is_government`: a government sender that proves it's theirs)
        subject: &[],
        text: &[],
        domains: &[],
        needs_code: false,
        not: &[],
    },
    View {
        key: "travel",
        name: "Travel",
        // (found by `is_travel`: reservations, itineraries, boarding passes, rides, rentals)
        subject: &[],
        text: &[],
        domains: &[],
        needs_code: false,
        not: &[],
    },
];

/// Whole words: the phrase in the text, not inside a longer word ("renew" isn't "renewable").
fn has_phrase(text: &str, phrase: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(phrase) {
        let start = from + i;
        let end = start + phrase.len();
        let before = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = text[end..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// A code to type: 4 to 8 digits, not a year (1900-2099: dates are in nearly every message).
fn has_code(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_digit()).any(|run| match run.len() {
        4 => !(run.starts_with("19") || run.starts_with("20")),
        5..=8 => true,
        _ => false,
    })
}

/// Not a sign-in code: a meeting's ID and passcode (Teams, Zoom, Meet, Webex, webinars,
/// dial-ins), or a promotion's code.
const NOT_SIGN_IN: &[&str] = &[
    "meeting id", "conference id", "join the meeting", "join meeting", "join microsoft teams meeting", "microsoft teams meeting", "join zoom meeting", "zoom.us/j/",
    "join with google meet", "meet.google.com", "webex", "gotowebinar", "join the webinar", "join webinar", "dial-in", "dial in", "join by phone",
    "promo code", "promotional code", "discount code", "coupon code", "coupon", "offer code", "use code", "gift code", "% off", "redeem",
];

fn domain_in(address: &str, domains: &[&str]) -> bool {
    let domain = address.rsplit('@').next().unwrap_or("").to_lowercase();
    domains.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}


// ------------------------------------------------------------------ Legal and Travel

/// Legal. A disclaimer sits at the end of a message: the whole text is read.
const LEGAL_TEXT: &[&str] = &[
    "privileged and confidential", "privileged & confidential", "attorney-client privilege", "attorney client privilege", "attorney-client privileged",
    "attorney work product", "attorney work-product", "confidential attorney", "this message may contain privileged", "may contain confidential and privileged",
    "contains privileged and confidential", "legally privileged",
];
const LEGAL_MATTERS: &[&str] = &[
    "docket", "case no", "case number", "case #", "court date", "hearing date", "hearing on", "court hearing", "summons", "subpoena", "deposition",
    "complaint filed", "notice of motion", "motion to", "order to show cause", "judgment", "settlement agreement", "litigation", "lawsuit", "arbitration",
    "court appearance", "trial date", "superior court", "district court", "civil action", "notice of appearance",
    // deals and contracts (a firm's threads say these more than "court")
    "signature pages", "signature page", "execution version", "redline", "term sheet", "purchase agreement", "operating agreement", "indemnification",
    "escrow agreement", "non-disclosure agreement", "power of attorney", "affidavit", "engagement letter", "cease and desist", "closing documents", "closing checklist",
];

/// A law firm's address: its organization's name ends in or is made of law words (marlowelaw.com,
/// jones-legal.com, doe-llp.com, attorneys…), never a few look-alikes.
fn law_firm(from: &str) -> bool {
    let domain = from.rsplit('@').next().unwrap_or("").to_lowercase();
    let org = crate::triage_decide::org_domain(&domain);
    let label = org.split('.').next().unwrap_or("");
    if ["outlaw", "bylaw", "bylaws", "inlaw", "inlaws", "claw", "lawn", "lawncare"].contains(&label) || label.ends_with("claw") || label.ends_with("outlaw") {
        return false; // (a "Redclaw" foundation is not a firm)
    }
    label.split('-').any(|t| ["law", "legal", "llp", "llc-law", "attorney", "attorneys", "lawyer", "lawyers", "lawfirm", "esq", "counsel"].contains(&t))
        || ((label.ends_with("law") || label.ends_with("legal") || label.ends_with("llp") || label.ends_with("attorneys") || label.ends_with("lawyers")) && label.len() > 5)
}

/// Whether a message belongs in Legal (its subject, sender's address and text).
///
/// Points, and one thing alone is never enough: 2 points or more.
pub fn is_legal(subject: &str, from: &str, text: &str) -> bool {
    legal_points(subject, from, text) >= 2.0
}

/// What makes a message look legal, and how much.
pub fn legal_points(subject: &str, from: &str, text: &str) -> f32 {
    legal_points_why(subject, from, text).0
}

/// `legal_points` with each reason.
pub fn legal_points_why(subject: &str, from: &str, text: &str) -> (f32, Vec<String>) {
    let mut points = 0.0;
    let mut why = Vec::new();
    // a law firm's address
    if law_firm(from) {
        points += 1.5;
        why.push("+1.5 from a law firm's address".to_string());
    }
    // a legal matter named in the subject, or in the text (each counts once)
    if let Some(p) = LEGAL_MATTERS.iter().find(|p| has_phrase(subject, p)) {
        points += 1.5;
        why.push(format!("+1.5 its subject names a legal matter (“{p}”)"));
    }
    let matters: Vec<&&str> = LEGAL_MATTERS.iter().filter(|p| has_phrase(text, p)).collect();
    if !matters.is_empty() {
        let add = (matters.len() as f32 * 0.5).min(1.0);
        points += add;
        why.push(format!("+{add} its text names {}", matters.iter().take(2).map(|p| format!("“{p}”")).collect::<Vec<_>>().join(", ")));
    }
    // a lawyer's signature in the text: a firm (LLP), Esq., law offices
    if let Some(p) = LAWYER_SIGNATURE.iter().find(|p| has_phrase(text, p)) {
        points += 1.0;
        why.push(format!("+1 a lawyer's signature (“{p}”)"));
    }
    // disclaimers: the attorney-client kind says more than the boilerplate companies add
    if let Some(p) = ATTORNEY_DISCLAIMER.iter().find(|p| has_phrase(text, p)) {
        points += 1.0;
        why.push(format!("+1 an attorney's disclaimer (“{p}”)"));
    } else if let Some(p) = LEGAL_TEXT.iter().find(|p| has_phrase(text, p)) {
        points += 0.5;
        why.push(format!("+0.5 a confidentiality footer (“{p}”)"));
    }
    (points, why)
}

/// A lawyer's or a firm's signature.
const LAWYER_SIGNATURE: &[&str] = &["llp", "esq", "esq.", "attorney at law", "attorneys at law", "law offices", "law office of", "law firm", "counsel for", "of counsel", "partner, litigation"];
/// The disclaimers only lawyers use.
const ATTORNEY_DISCLAIMER: &[&str] = &["attorney-client privilege", "attorney client privilege", "attorney-client privileged", "attorney work product", "attorney work-product", "confidential attorney"];

/// Travel: hotels, Airbnb, airlines, trains, rides, car rentals, booking sites.
pub(crate) const TRAVEL_DOMAINS: &[&str] = &[
    "airbnb.com", "vrbo.com", "booking.com", "expedia.com", "hotels.com", "priceline.com", "kayak.com", "orbitz.com", "travelocity.com", "hopper.com", "tripadvisor.com", "agoda.com",
    "marriott.com", "hilton.com", "hyatt.com", "ihg.com", "wyndhamhotels.com", "choicehotels.com", "bestwestern.com", "accor.com", "radissonhotels.com",
    "united.com", "aa.com", "delta.com", "southwest.com", "jetblue.com", "alaskaair.com", "spirit.com", "flyfrontier.com", "aircanada.com", "britishairways.com", "lufthansa.com", "airfrance.fr", "klm.com", "ita-airways.com", "aerlingus.com", "emirates.com", "qatarairways.com", "ryanair.com", "easyjet.com",
    "flybreeze.com", "aveloair.com", "allegiantair.com", "suncountry.com", "hawaiianairlines.com", "westjet.com", "flypeach.com",
    "amtrak.com", "trenitalia.com", "italotreno.it", "eurostar.com", "sncf-connect.com", "nationalrail.co.uk", "njtransit.com",
    "gobrightline.com", "viarail.ca", "mta.info", "septa.org", "greyhound.com", "flixbus.com", "megabus.com", "peterpanbus.com", "ourbus.com", "wanderu.com", "busbud.com",
    "seastreak.com", "nywaterway.com", "cmlf.com", "steamshipauthority.org", "hylinecruises.com", "bcferries.com", "wsdot.wa.gov", "staten-island-ferry.com",
    "uber.com", "lyft.com",
    "hertz.com", "avis.com", "budget.com", "enterprise.com", "nationalcar.com", "alamo.com", "sixt.com", "thrifty.com", "dollar.com", "turo.com", "zipcar.com", "getaround.com", "foxrentacar.com", "paylesscar.com",
];
const TRAVEL_WORDS: &[&str] = &[
    "reservation", "booking", "itinerary", "your trip", "trip to", "your stay", "check-in", "check in", "boarding pass", "flight", "confirmation number",
    "confirmation code", "e-ticket", "eticket", "your ride", "ride receipt", "trip receipt", "rental", "pick-up", "pickup", "drop-off", "seat", "gate",
];
/// Strong enough alone, from anyone (a travel agent, a company's travel desk).
const TRAVEL_STRONG: &[&str] = &[
    "boarding pass", "flight itinerary", "travel itinerary", "your itinerary", "booking confirmation", "reservation confirmation", "hotel reservation",
    "hotel confirmation", "flight confirmation", "car rental confirmation", "rental car reservation", "your reservation is confirmed", "e-ticket receipt",
];

/// A travel company's marketing (sales, fares from, deals) isn't a reservation.
const TRAVEL_NOT: &[&str] = &["sale", "% off", "deal", "deals", "save", "fares from", "fares as low", "from $", "book now", "offer", "bonus", "miles", "points", "win ", "sweepstakes", "explore", "inspiration", "discover", "getaway", "newsletter", "tell us about", "how was", "survey", "rate your", "review your", "protect your", "coverage", "upgrade your"];

/// Whether a message belongs in Travel.
pub fn is_travel(subject: &str, from: &str, text: &str) -> bool {
    travel_points(subject, from, text).0 >= VIEW_POINTS
}

/// What makes a message look like a reservation, and how much: a strong subject (a boarding pass,
/// a booking confirmation) 3 from anyone; a travel company 1, its reservation words 1.5 in the
/// subject, 1 in the text; the same words from anyone else count half; sales, surveys and
/// marketing addresses take 3.
pub fn travel_points(subject: &str, from: &str, text: &str) -> (f32, Vec<String>) {
    let mut points = 0.0;
    let mut why = Vec::new();
    if let Some(p) = TRAVEL_STRONG.iter().find(|p| has_phrase(subject, p)) {
        points += 3.0;
        why.push(format!("+3 its subject says “{p}”"));
    }
    let company = domain_in(from, TRAVEL_DOMAINS);
    let scale = if company { 1.0 } else { 0.5 };
    if company {
        points += 1.0;
        why.push("+1 from a travel company".to_string());
    }
    if let Some(p) = TRAVEL_WORDS.iter().find(|p| has_phrase(subject, p)) {
        points += 1.5 * scale;
        why.push(format!("+{} its subject says “{p}”", 1.5 * scale));
    }
    let start: String = text.chars().take(1500).collect();
    if let Some(p) = TRAVEL_WORDS.iter().find(|p| has_phrase(&start, p)) {
        points += scale;
        why.push(format!("+{scale} its text says “{p}”"));
    }
    if points == 0.0 {
        return (0.0, why);
    }
    let selling = TRAVEL_NOT.iter().find(|p| has_phrase(subject, p)).map(|p| p.trim().to_string()).or_else(|| {
        (subject.contains(" off your") || (subject.contains('$') && subject.contains(" off"))).then(|| "… off".to_string())
    });
    if let Some(p) = selling {
        points -= 3.0;
        why.push(format!("−3 its subject sells (“{p}”)"));
    }
    // a travel company's marketing and survey addresses (discover@airbnb.com, marketing@…)
    let local = from.split('@').next().unwrap_or("").to_lowercase();
    if ["discover", "marketing", "deals", "offers", "promo", "newsletter", "feedback", "survey"].iter().any(|w| local.contains(w))
        || from.to_lowercase().contains("@feedback.")
    {
        points -= 3.0;
        why.push("−3 a marketing or survey address".to_string());
    }
    (points, why)
}

/// The views a message belongs in (its subject, sender's address, preview text).
pub fn views_of(subject: &str, from: &str, preview: &str) -> Vec<&'static str> {
    views_of_proven(subject, from, preview, false)
}

/// `views_of`, knowing whether the sender proved it's theirs (`proven`: Government needs it).
pub fn views_of_proven(subject: &str, from: &str, preview: &str, proven: bool) -> Vec<&'static str> {
    view_scores(subject, from, preview, proven).into_iter().filter(|s| s.hit).map(|s| s.key).collect::<Vec<_>>().pipe_payments()
}

/// Points a message needs to be in a view.
pub const VIEW_POINTS: f32 = 2.0;

/// One view's score for a message: its points, each reason, and whether it's in.
#[derive(Clone, Debug, Serialize)]
pub struct ViewScore {
    pub key: &'static str,
    pub name: &'static str,
    pub points: f32,
    pub reasons: Vec<String>,
    pub hit: bool,
}

/// Every view's score for a message (the views with any reason at all). Subject wording counts
/// most (2, a little more for each further phrase), the text's wording 1 a phrase (2 at most),
/// words saying it isn't one take 3; a view for some senders only (payments, banks) needs one of
/// them, 2FA codes need a code. Legal, Travel and Government have signs of their own.
pub fn view_scores(subject: &str, from: &str, text: &str, proven: bool) -> Vec<ViewScore> {
    let subject = subject.to_lowercase();
    let text = text.to_lowercase();
    let mut out = Vec::new();
    for v in VIEWS.iter().filter(|v| !v.subject.is_empty() || !v.text.is_empty()) {
        let mut points = 0.0;
        let mut reasons = Vec::new();
        if !v.domains.is_empty() {
            if !domain_in(from, v.domains) {
                continue;
            }
            reasons.push(format!("from {} (a sender this view is for)", from.rsplit('@').next().unwrap_or("")));
        }
        let in_subject: Vec<&&str> = v.subject.iter().filter(|p| has_phrase(&subject, p)).collect();
        if let Some(first) = in_subject.first() {
            let more = (in_subject.len() - 1).min(2) as f32 * 0.5;
            points += 2.0 + more;
            reasons.push(format!("+{} its subject says “{first}”{}", 2.0 + more, if in_subject.len() > 1 { format!(" (and {} more)", in_subject.len() - 1) } else { String::new() }));
        }
        let in_text: Vec<&&str> = v.text.iter().filter(|p| has_phrase(&text, p)).collect();
        if !in_text.is_empty() {
            let add = in_text.len().min(2) as f32;
            points += add;
            reasons.push(format!("+{add} its text says {}", in_text.iter().take(2).map(|p| format!("“{p}”")).collect::<Vec<_>>().join(", ")));
        }
        if v.needs_code {
            if points == 0.0 || !(has_code(&subject) || has_code(&text)) {
                continue;
            }
            points += 1.0;
            reasons.push("+1 a code in it".into());
            if let Some(not) = NOT_SIGN_IN.iter().find(|p| subject.contains(**p) || text.contains(**p)) {
                points -= 3.0;
                reasons.push(format!("−3 it says “{not}” (a meeting's or a promotion's code, not a sign-in)"));
            }
        }
        if let Some(not) = v.not.iter().find(|p| has_phrase(&subject, p)) {
            points -= 3.0;
            reasons.push(format!("−3 its subject says “{not}” (not one)"));
        }
        if points > 0.0 || reasons.iter().any(|r| r.starts_with('−')) {
            out.push(ViewScore { key: v.key, name: v.name, points, hit: points >= VIEW_POINTS, reasons });
        }
    }
    let (points, reasons) = legal_points_why(&subject, from, &text);
    if points > 0.0 {
        out.push(ViewScore { key: "legal", name: "Legal", points, hit: points >= VIEW_POINTS, reasons });
    }
    let (points, reasons) = travel_points(&subject, from, &text);
    if points != 0.0 {
        out.push(ViewScore { key: "travel", name: "Travel", points, hit: points >= VIEW_POINTS, reasons });
    }
    if is_government(from, proven) {
        out.push(ViewScore { key: "government", name: "Government", points: 2.0, hit: true, reasons: vec!["+2 a government sender that proves it's theirs".into()] });
    }
    out
}

/// Money that came in (Payments Received) is neither a receipt for something bought nor one of
/// the user's own bank transactions.
trait PipePayments {
    fn pipe_payments(self) -> Vec<&'static str>;
}

impl PipePayments for Vec<&'static str> {
    fn pipe_payments(mut self) -> Vec<&'static str> {
        if self.contains(&"payments") {
            // (paid for an invoice: the payment, not the invoice)
            self.retain(|v| *v != "receipts" && *v != "bank" && *v != "payments-sent" && *v != "invoices");
        }
        // a payment the user made (PayPal, the bank, a card) is a Payment Sent, not a receipt
        // from a merchant nor another bank transaction
        if self.contains(&"payments-sent") {
            self.retain(|v| *v != "receipts" && *v != "bank");
        }
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewInfo {
    pub key: String,
    pub name: String,
    pub messages: u64,
}

/// Whether a folder holds the user's sent mail (by its role, else its name: Sent, Sent Items,
/// classic Eudora's Out and its Old Sent / OldOut folders).
pub fn sent_folder(path: &str, role: Option<&str>) -> bool {
    if role == Some("sent") {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    let name = name.trim_end_matches(".mbx").trim_end_matches(".fol");
    let whole = path.to_lowercase();
    matches!(name, "sent" | "sent items" | "sent messages" | "sent mail" | "out" | "outbox")
        || name.starts_with("sent ") || name.starts_with("out1") || name.starts_with("out2")
        || whole.contains("old sent") || whole.contains("oldout")
}

/// `AND the message is in one of these accounts` (empty: any), for a view's queries.
/// The top-level folder names that are a Trash.
const TRASH_NAMES: [&str; 3] = ["deleted items", "trash", "deleted messages"];

impl Library {
    /// Every Trash folder: "Deleted Items", "Trash" (and what's inside them), the archive's too.
    fn trash_folders(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, path, role IS 'trash' FROM folders")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, path, role) = row?;
            let path = path.strip_prefix("Online Archive/").unwrap_or(&path);
            let top = path.split('/').next().unwrap_or("").trim().to_lowercase();
            if role || TRASH_NAMES.contains(&top.as_str()) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// What a view's messages must also be (on `t`): in a folder other than a Trash, in these
    /// accounts when given.
    fn view_scope(&self, accounts: &[i64]) -> Result<String> {
        let list = |ids: &[i64]| ids.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",");
        let mut w = String::from(" AND EXISTS (SELECT 1 FROM locations l");
        if accounts.is_empty() {
            w.push_str(" WHERE l.message = t.message");
        } else {
            w.push_str(&format!(" JOIN folders f ON f.id = l.folder WHERE l.message = t.message AND f.account IN ({})", list(accounts)));
        }
        let trash = self.trash_folders()?;
        if !trash.is_empty() {
            w.push_str(&format!(" AND l.folder NOT IN ({})", list(&trash)));
        }
        w.push(')');
        Ok(w)
    }

    /// The accounts the combined Views show (`VIEWS_ACCOUNTS`; unset: the user's own).
    pub fn views_accounts(&self) -> Result<Vec<(i64, String, String, bool)>> {
        let chosen: Option<Vec<String>> = self.setting(VIEWS_ACCOUNTS)?.and_then(|j| serde_json::from_str(&j).ok());
        Ok(self
            .accounts_counted(false)?
            .into_iter()
            .map(|a| {
                let address = a.address.clone().unwrap_or_default().to_lowercase();
                let on = match &chosen {
                    Some(list) => list.contains(&address),
                    None => a.kind != "graph-shared",
                };
                (a.id, address, a.shown(), on)
            })
            .collect())
    }

    pub fn set_views_accounts(&self, addresses: &[String]) -> Result<()> {
        let list: Vec<String> = addresses.iter().map(|a| a.to_lowercase()).collect();
        self.set_setting(VIEWS_ACCOUNTS, &serde_json::to_string(&list)?)
    }

    /// The account ids the combined Views show.
    pub fn combined_view_accounts(&self) -> Result<Vec<i64>> {
        Ok(self.views_accounts()?.into_iter().filter(|a| a.3).map(|a| a.0).collect())
    }

    /// Tags up to `limit` messages not looked at yet (newest mail first, then older mail a
    /// batch at a time, from the newest down). Returns how many were looked at (0: all done).
    /// Why a message is (or isn't) in each view: every view's points and reasons, read the way
    /// tagging reads it (its subject, first sender, whole text, whether the sender proved it).
    pub fn view_scores_of(&self, id: i64) -> Result<Vec<ViewScore>> {
        let (subject, from): (String, String) = self.conn.query_row(
            "SELECT m.subject, COALESCE(ad.address, '') FROM messages m LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0
             LEFT JOIN addresses ad ON ad.id = ma.address WHERE m.id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let raw = self.raw_message(id)?;
        let text = crate::parse::parse(&raw, crate::parse::DateHints::default(), crate::library::now()).body_text;
        let s = crate::triage::signals(&crate::header_index::read_headers(&raw));
        let proven = crate::triage_known::verified(&s) || (s.dmarc.is_none() && s.dkim.is_none() && s.spf.is_none());
        let mut scores = view_scores(&subject, &from, &text, proven);
        scores.sort_by(|a, b| b.points.total_cmp(&a.points));
        Ok(scores)
    }

    pub fn tag_views(&self, limit: usize) -> Result<u64> {
        // two marks: everything above `high` is new since; everything below `low` is still to do
        if self.setting("views-rules")?.as_deref() != Some(RULES_VERSION) {
            // the rules changed: tagged again from scratch
            // (Maybe Spam is the spam filter's, not these rules': it stays)
            self.conn.execute("DELETE FROM tags WHERE tag <> ?1", [crate::triage_decide::MAYBE_VIEW])?;
            self.set_setting("views-high", "0")?;
            self.set_setting("views-low", &i64::MAX.to_string())?;
            self.set_setting("views-rules", RULES_VERSION)?;
        }
        let high: i64 = self.setting("views-high")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let low: i64 = self.setting("views-low")?.and_then(|v| v.parse().ok()).unwrap_or(i64::MAX);
        let newest: i64 = self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?;
        let (sql, a, b, new) = if newest > high && high > 0 {
            ("SELECT m.id, m.date, m.subject, m.snippet, COALESCE(ad.address, '') FROM messages m LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0
              LEFT JOIN addresses ad ON ad.id = ma.address WHERE m.id > ?1 AND m.id <= ?2 ORDER BY m.id LIMIT ?3", high, newest, true)
        } else {
            let top = if low == i64::MAX { newest + 1 } else { low };
            ("SELECT m.id, m.date, m.subject, m.snippet, COALESCE(ad.address, '') FROM messages m LEFT JOIN message_addresses ma ON ma.message = m.id AND ma.role = 0 AND ma.position = 0
              LEFT JOIN addresses ad ON ad.id = ma.address WHERE m.id < ?2 AND m.id > ?1 ORDER BY m.id DESC LIMIT ?3", 0, top, false)
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows: Vec<(i64, i64, String, String, String)> =
            stmt.query_map(params![a, b, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            if high == 0 {
                self.set_setting("views-high", &newest.to_string())?; // (an empty library)
            }
            return Ok(0);
        }
        let mut spam = self.conn.prepare_cached("SELECT 1 FROM triage WHERE message = ?1 AND category IN ('spam', 'blocked')")?;
        // the user's own mail isn't in views:
        // from one of their addresses, or filed only in Sent folders
        let own: std::collections::HashSet<String> = self.own_addresses()?.into_iter().map(|a| a.to_lowercase()).collect();
        let sent_folders: std::collections::HashSet<i64> = {
            let mut s = self.conn.prepare("SELECT id, path, role FROM folders")?;
            let rows: Vec<(i64, String, Option<String>)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            rows.into_iter().filter(|(_, p, r)| sent_folder(p, r.as_deref())).map(|(id, _, _)| id).collect()
        };
        // every message is read whole: a store's receipt says so only in its text, a legal
        // disclaimer sits at the end (older mail a batch at a time, in the background, in
        // parallel)
        let bodies: std::collections::HashMap<i64, (String, bool)> = {
            use rayon::prelude::*;
            let mut locate = self.conn.prepare_cached("SELECT pack, pack_offset, pack_len FROM messages WHERE id = ?1")?;
            let places: Vec<(i64, crate::pack::Location)> =
                rows.iter().filter_map(|r| locate.query_row(params![r.0], |x| Library::location(x, 0)).ok().map(|l| (r.0, l))).collect();
            let packs = &self.packs;
            places
                .par_iter()
                .filter_map(|(id, at)| {
                    let raw = packs.read(*at).ok()?.raw;
                    let text = crate::parse::parse(&raw, crate::parse::DateHints::default(), crate::library::now()).body_text;
                    // (its start, and its end where disclaimers sit: memory stays small)
                    let chars: Vec<char> = text.chars().collect();
                    let kept: String = if chars.len() > 48_000 {
                        chars[..40_000].iter().chain(['\n'].iter()).chain(chars[chars.len() - 8_000..].iter()).collect()
                    } else {
                        text
                    };
                    // whether the sender proved it's theirs (old mail with no checks at all counts)
                    let s = crate::triage::signals(&crate::header_index::read_headers(&raw));
                    let unchecked = s.dmarc.is_none() && s.dkim.is_none() && s.spf.is_none();
                    let proven = crate::triage_known::verified(&s) || unchecked;
                    Some((*id, (kept, proven)))
                })
                .collect()
        };
        let mut folders_of = self.conn.prepare_cached("SELECT folder FROM locations WHERE message = ?1")?;
        let mut insert = self.conn.prepare_cached("INSERT OR IGNORE INTO tags (tag, date, message) VALUES (?1, ?2, ?3)")?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for (id, date, subject, snippet, from) in &rows {
                let views = match bodies.get(id) {
                    Some((text, proven)) => views_of_proven(subject, from, text, *proven),
                    None => views_of(subject, from, snippet),
                };
                let candidate = alert_candidate(subject, from);
                if (views.is_empty() && !candidate) || spam.exists(params![id])? || own.contains(&from.to_lowercase()) {
                    continue;
                }
                let folders: Vec<i64> = folders_of.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                if !folders.is_empty() && folders.iter().all(|f| sent_folders.contains(f)) {
                    continue; // filed only as sent mail
                }
                if candidate {
                    insert.execute(params![ALERT_CHECK, date, id])?;
                }
                // the view, and its group by sender ("receipts/eBay")
                let group = brand(from);
                let mut names: Vec<&str> = Vec::new();
                for v in views {
                    // (newly added only: a message read again isn't logged again)
                    if insert.execute(params![v, date, id])? > 0 {
                        names.push(VIEWS.iter().find(|x| x.key == v).map_or(v, |x| x.name));
                    }
                    insert.execute(params![format!("{v}/{group}"), date, id])?;
                }
                if !names.is_empty() {
                    self.log_job("views", Some(*id), &format!("Added to {} ({group})", names.join(", ")))?;
                }
            }
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        if new {
            self.set_setting("views-high", &rows.last().unwrap().0.to_string())?;
        } else {
            let lowest = rows.last().unwrap().0;
            self.set_setting("views-low", &lowest.to_string())?;
            if high == 0 {
                self.set_setting("views-high", &rows[0].0.max(newest).to_string())?;
            }
        }
        Ok(rows.len() as u64)
    }

    /// Confirms up to `limit` Red Alert candidates (their headers read from the packs): each
    /// becomes a Red Alert (tagged, with its sender's group) or not; either way it's checked
    /// once. Returns how many were checked (0: none left).
    pub fn confirm_alerts(&self, limit: usize) -> Result<u64> {
        let mut stmt = self.conn.prepare(
            "SELECT t.message, t.date, m.subject, m.pack, m.pack_offset, m.pack_len FROM tags t JOIN messages m ON m.id = t.message WHERE t.tag = ?1 ORDER BY t.date DESC LIMIT ?2",
        )?;
        let rows: Vec<(i64, i64, String, crate::pack::Location)> =
            stmt.query_map(params![ALERT_CHECK, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, Library::location(r, 3)?)))?.collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(0);
        }
        let lists = self.sender_lists()?;
        let own: Vec<String> = self.own_addresses()?.into_iter().map(|a| a.to_lowercase()).collect();
        let own_domains = self.own_org_domains()?;
        let mut insert = self.conn.prepare_cached("INSERT OR IGNORE INTO tags (tag, date, message) VALUES (?1, ?2, ?3)")?;
        let mut done = self.conn.prepare_cached("DELETE FROM tags WHERE tag = ?1 AND date = ?2 AND message = ?3")?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for (id, date, subject, location) in &rows {
                let raw = self.packs.read(*location)?.raw;
                let headers = crate::header_index::read_headers(&raw);
                let s = crate::triage::signals(&headers);
                let mine = own.contains(&s.from.to_lowercase());
                if confirm_alert(subject, &s, !s.from.is_empty() && lists.is_allowed(&s.from), &own_domains, mine) {
                    insert.execute(params!["red-alert", date, id])?;
                    insert.execute(params![format!("red-alert/{}", brand(&s.from)), date, id])?;
                    self.log_job("alerts", Some(*id), &format!("Red Alert confirmed ({})", brand(&s.from)))?;
                }
                done.execute(params![ALERT_CHECK, date, id])?;
            }
            Ok(())
        })();
        self.conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        Ok(rows.len() as u64)
    }

    /// Every view with how many messages it holds in these accounts (empty: all).
    pub fn views(&self, accounts: &[i64]) -> Result<Vec<ViewInfo>> {
        VIEWS.iter().map(|v| Ok(ViewInfo { key: v.key.into(), name: v.name.into(), messages: self.count_view(v.key, accounts)? })).collect()
    }

    /// A view's groups by sender in these accounts (empty: all), the biggest first: (name, messages).
    pub fn view_groups(&self, view: &str, accounts: &[i64]) -> Result<Vec<(String, u64)>> {
        let sql = format!(
            "SELECT substr(t.tag, length(?1) + 2), COUNT(*) FROM tags t WHERE t.tag > ?1 || '/' AND t.tag < ?1 || '0'{} GROUP BY t.tag ORDER BY 2 DESC",
            self.view_scope(accounts)?
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map(params![view], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// A page of a view in these accounts (empty: all), newest first (keyset paging).
    pub fn list_view(&self, view: &str, accounts: &[i64], after: Option<PageCursor>, limit: usize) -> Result<Vec<MessageRow>> {
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.date, c.id));
        let sql = format!("SELECT t.message FROM tags t WHERE t.tag = ?1 AND (t.date, t.message) < (?2, ?3){} ORDER BY t.date DESC, t.message DESC LIMIT ?4", self.view_scope(accounts)?);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let ids: Vec<i64> = stmt.query_map(params![view, date, id, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        self.rows(&ids)
    }

    pub fn count_view(&self, view: &str, accounts: &[i64]) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM tags t WHERE t.tag = ?1{}", self.view_scope(accounts)?);
        Ok(self.conn.query_row(&sql, params![view], |r| r.get::<_, i64>(0))? as u64)
    }

    /// The cursor `skip` rows into a view (a jump down the scroll bar).
    pub fn cursor_view(&self, view: &str, accounts: &[i64], skip: u64) -> Result<Option<PageCursor>> {
        use rusqlite::OptionalExtension;
        let sql = format!("SELECT t.date, t.message FROM tags t WHERE t.tag = ?1{} ORDER BY t.date DESC, t.message DESC LIMIT 1 OFFSET ?2", self.view_scope(accounts)?);
        Ok(self.conn.query_row(&sql, params![view, skip as i64], |r| Ok(PageCursor { date: r.get(0)?, id: r.get(1)? })).optional()?)
    }

    /// Every message in a view with a folder it's in (in these accounts when given).
    pub fn view_ids(&self, view: &str, accounts: &[i64]) -> Result<Vec<(i64, i64)>> {
        let only = if accounts.is_empty() { String::new() } else { format!(" AND f.account IN ({})", accounts.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")) };
        let sql = format!(
            "SELECT t.message, (SELECT l.folder FROM locations l JOIN folders f ON f.id = l.folder WHERE l.message = t.message{only} LIMIT 1) FROM tags t WHERE t.tag = ?1{} ORDER BY t.date DESC, t.message DESC",
            self.view_scope(accounts)?
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map(params![view], |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))))?.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_go_in_the_views_that_fit() {
        assert_eq!(views_of("Your Amazon.com order #112-334 has shipped", "ship@amazon.com", ""), Vec::<&str>::new());
        assert_eq!(views_of("Your receipt from Apple", "no_reply@email.apple.com", ""), vec!["receipts"]);
        assert_eq!(views_of("Merchant Email Receipt", "noreply@mail.authorize.net", ""), vec!["payments"], "money in, not a receipt");
        // QuickBooks asking to be paid is an invoice, not money in
        assert_eq!(views_of("New Payment Request from Acme Supply - Invoice 1043", "quickbooks@notification.intuit.com", ""), vec!["invoices"]);
        assert_eq!(views_of("Payment received for Invoice 1001", "quickbooks@notification.intuit.com", ""), vec!["payments"], "a real payment still is one");
        assert_eq!(views_of("Receipt for your payment to Shop", "service@paypal.com", ""), vec!["payments-sent"], "paid by PayPal: a payment sent");
        assert_eq!(views_of("Thank you for your payment", "alerts@chase.com", ""), vec!["payments-sent"]);
        // Legal: a disclaimer at the end, a law firm, a docket or court date. Points: one thing
        // alone isn't enough
        let footer = "Your invoice is attached.\n\n--\nThis message is PRIVILEGED AND CONFIDENTIAL and intended only for the addressee.";
        assert!(!views_of("Your Invoice From Larkspur Distribution is Attached", "larkspur-dist@billtrust.example", footer).contains(&"legal"));
        assert!(!views_of("Re: lease", "jdoe@marlowelaw.example", "see attached").contains(&"legal"), "a firm writing about lunch");
        assert!(views_of("Re: lease", "jdoe@marlowelaw.example", "The signature pages are attached; the execution version follows.").contains(&"legal"));
        assert!(views_of("Doe v. Acme Fiber, et. al.", "kpark@marlowelaw.example", "The hearing on the motion to dismiss is set for Nov 3.").contains(&"legal"));
        let firm = "Comments on the redline attached.\n\nPat Brennan\nBrennan, Vale, Ostrow & Kemp LLP\nThis message may contain information that is privileged and confidential.";
        assert!(views_of("RE: Acme/Bluefield - Additional Closing", "pbrennan@brennanvale.example", firm).contains(&"legal"), "{}", legal_points("re: acme/bluefield - additional closing", "pbrennan@brennanvale.example", &firm.to_lowercase()));
        assert!(views_of("Court date moved to Nov 3", "clerk@courts.example", "Your hearing date is now November 3.").contains(&"legal"));
        // Government: a government sender that proves it's theirs
        assert!(views_of_proven("Vendor Invite for Skilled Trades Fair", "jsmith@co.sample.nj.us", "", true).contains(&"government"));
        assert!(views_of_proven("Your notice", "notices@irs.gov", "", true).contains(&"government"));
        assert!(!views_of_proven("Your notice", "notices@irs.gov", "", false).contains(&"government"), "unproven: a fake IRS isn't one");
        assert!(!views_of_proven("Hi", "someone@example.us", "", true).contains(&"government"));
        assert!(!views_of("Spring lawn care special", "deals@greenlawn.example", "").contains(&"legal"));
        assert!(!views_of("Hi", "someone@outlaw.example", "").contains(&"legal"));
        // Travel: reservations and boarding passes, not the airlines' sales
        assert!(views_of("Your reservation is confirmed", "noreply@airbnb.com", "").contains(&"travel"));
        assert!(views_of("Your trip with Uber", "uber.us@uber.com", "").contains(&"travel"));
        assert!(views_of("Boarding pass for your flight to Rome", "agent@travel-desk.example", "").contains(&"travel"));
        assert!(!views_of("Fares from $99: our fall sale", "deals@united.com", "").contains(&"travel"));
        // (marketing that looks like travel)
        assert!(!views_of("Get $25 off your fall road trip", "marketing@emails.dollar.com", "").contains(&"travel"));
        assert!(!views_of("Protect your Miami stay with Accidental Damage Coverage", "discover@airbnb.com", "").contains(&"travel"));
        assert!(!views_of("Reminder – Please tell us about your stay", "noreply@feedback.hilton.com", "").contains(&"travel"));
        assert!(views_of("eTicket Itinerary and Receipt for Confirmation RJ7MXQ", "Receipts@united.com", "").contains(&"travel"));
        assert!(!views_of("Hello from the Redclaw Foundation", "newsletter@mail.redclaw.example", "").contains(&"legal"));
        // a store's e-receipt says so only in its text 
        let walgreens = "Walgreens Thank you for shopping at Walgreens. Your purchase receipt Store #10001 100 MAIN ST Springfield, NJ Purchase summary Subtotal: $6.28 Total: $6.70";
        assert_eq!(views_of("Thank you for shopping at Walgreens.", "walgreens@mktx.walgreens.com", walgreens), vec!["receipts"]);
        assert_eq!(views_of("Earn $15 W Cash rewards on your P&G favorites", "walgreens@eml.walgreens.com", "Shop now and save on favorites"), Vec::<&str>::new(), "its marketing isn't");
        assert_eq!(views_of("Notification of an Instant Payment Received from Ann", "service@paypal.com", ""), vec!["payments"]);
        assert_eq!(brand("noreply@mail.authorize.net"), "Authorize.Net");
        assert_eq!(brand("ebay@ebay.com"), "eBay");
        assert_eq!(brand("orders@order.homedepot.com"), "Home Depot");
        assert_eq!(brand("hello@shop.example"), "Shop");
        assert_eq!(views_of("Payment declined: update your card", "billing@shop.example", ""), Vec::<&str>::new(), "not done: not a receipt");
        assert_eq!(views_of("Invoice INV-2041 from Acme", "billing@acme.example", ""), vec!["invoices"]);
        assert_eq!(views_of("Your domain will renew in 30 days", "billing@registrar.example", ""), vec!["renewals"]);
        assert_eq!(views_of("Renewable energy newsletter", "news@green.example", ""), Vec::<&str>::new(), "whole words");
        assert_eq!(views_of("Your direct deposit has posted", "alerts@notify.chase.com", ""), vec!["bank"]);
        assert_eq!(views_of("Big deposit of savings on shoes!", "deals@shop.example", ""), Vec::<&str>::new(), "a bank's words from a shop");
        assert_eq!(views_of("Welcome to Zoom", "no-reply@zoom.us", ""), vec!["new-accounts"]);
        assert_eq!(views_of("Your verification code", "no-reply@service.example", "Your code is 482913. It expires in 10 minutes."), vec!["2fa"]);
        assert_eq!(views_of("Sign-in attempt", "security@service.example", "Use this code: 4471-ab"), vec!["2fa"]);
        // not sign-in codes: a meeting's passcode, a promotion's code, a year
        assert!(!views_of("Acme Q3 Board Meeting", "alex@example.com", "Microsoft Teams meeting Join on your computer Meeting ID: 123 456 789 Passcode: Ab3cd9 Dial in by phone +1 646-555-0100,,48213955#").contains(&"2fa"));
        assert!(!views_of("Get Early Access to Black Friday", "deals@em.brightshade.example", "Use code BF2026 for 40% off. Your code is SAVE40 until 11/30.").contains(&"2fa"));
        assert!(!views_of("Your code", "a@b.example", "See you in 2026.").contains(&"2fa"), "a year isn't a code");
        assert_eq!(views_of("Your Gobrightline one-time Passcode", "mail@auth.gobrightline.com", "Your passcode is 593812."), vec!["2fa"]);
        assert_eq!(views_of("About your security code question", "ann@friend.example", "no digits here"), Vec::<&str>::new(), "a code needs a number");
    }

    #[test]
    fn sent_folders_are_known_by_role_or_name() {
        assert!(sent_folder("Sent Items", None) && sent_folder("Out.mbx", None) && sent_folder("OldOut.fol/Out1999.mbx", None));
        assert!(sent_folder("Online Archive/Old Sent Items/1997", None) && sent_folder("Whatever", Some("sent")));
        assert!(!sent_folder("Inbox", None) && !sent_folder("Outages", None) && !sent_folder("Projects/Outreach", None));
    }

    #[test]
    fn red_alerts_are_real_notices_not_marketing_or_fakes() {
        use crate::header_index::read_headers;
        use crate::triage::signals;
        let check = |raw: &str, whitelisted: bool| {
            let headers = read_headers(raw.as_bytes());
            let s = signals(&headers);
            let subject = headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| v.clone()).unwrap_or_default();
            let from = s.from.clone();
            alert_candidate(&subject, &from) && confirm_alert(&subject, &s, whitelisted, &["corp.example".to_string()], false)
        };
        let pass = "Authentication-Results: mx.example; dmarc=pass header.from=irs.gov; dkim=pass header.d=irs.gov\r\n";
        assert!(check(&format!("{pass}From: IRS <notices@irs.gov>\r\nSubject: Notice CP14: balance due\r\n\r\nx"), false), "a real IRS notice");
        assert!(!check("From: IRS <notices@irs.gov>\r\nSubject: Notice CP14: balance due\r\n\r\nx", false), "unproven: not");
        assert!(!check("Authentication-Results: mx; dmarc=fail\r\nFrom: IRS Refunds <irs@refund-irs.example>\r\nSubject: IRS final notice\r\n\r\nx", false), "a fake IRS: never");
        assert!(check("From: Power Co <billing@power.example>\r\nSubject: Disconnect notice for your account\r\n\r\nx", false), "a utility's notice");
        assert!(!check("From: Deals <news@shop.example>\r\nList-Unsubscribe: <https://shop.example/u>\r\nSubject: FINAL NOTICE: 50% off ends tonight\r\n\r\nx", false), "marketing");
        assert!(check("From: Ann <ann@friend.example>\r\nSubject: Emergency - call me\r\n\r\nx", true), "someone the user writes to");
        assert!(!check("From: Stranger <x@y.example>\r\nSubject: URGENT business proposal\r\n\r\nx", false), "urgent from a stranger");
        let irs = "Authentication-Results: mx; dmarc=pass header.from=irs.gov\r\nFrom: IRS <notices@irs.gov>\r\nSubject: Notice CP14: balance due\r\n\r\nx";
        assert!(check(irs, false));
        let meeting = "Authentication-Results: mx; dmarc=pass header.from=cpuc.ca.gov\r\nFrom: CPUC <dac@cpuc.ca.gov>\r\nSubject: Notice of Advisory Group Meeting\r\n\r\nx";
        assert!(!check(meeting, false), "a government meeting notice isn't an alert");
        assert!(!check("From: SOC <soc@sec.example>\r\nSubject: Critical Risk - Malicious Binary\r\n\r\nx", true), "critical isn't an alarm");
        assert!(!check("From: News <editor@news.example>\r\nList-Unsubscribe: <https://n.example/u>\r\nSubject: Urgent - polls tighten\r\n\r\nx", true), "a newsletter the user once answered");
    }

    #[test]
    fn security_notices_are_red_alerts_forwarded_too_but_not_detections_or_marketing() {
        use crate::header_index::read_headers;
        use crate::triage::signals;
        let check = |raw: &str, whitelisted: bool| {
            let headers = read_headers(raw.as_bytes());
            let s = signals(&headers);
            let subject = headers.iter().find(|(n, _)| n == "subject").map(|(_, v)| v.clone()).unwrap_or_default();
            alert_candidate(&subject, &s.from) && confirm_alert(&subject, &s, whitelisted, &["corp.example".to_string()], false)
        };
        let cisa = "Authentication-Results: mx; dmarc=pass header.from=cisa.gov\r\nFrom: CISA <cisa@messages.cisa.gov>\r\nList-Unsubscribe: <https://x/u>\r\nSubject: CISA Adds Two Known Exploited Vulnerabilities to Catalog\r\n\r\nx";
        assert!(check(cisa, false), "CISA, through its mailing service");
        let vendor = "Authentication-Results: mx; dmarc=pass header.from=fortinet.com\r\nFrom: Fortinet PSIRT <psirt@fortinet.com>\r\nList-Unsubscribe: <https://x/u>\r\nSubject: FortiOS security advisory: CVE-2026-1234\r\n\r\nx";
        assert!(check(vendor, false), "a vendor's patch notice");
        assert!(check("From: Sam <sam@corp.example>\r\nSubject: FW: Veeam releases security updates for Backup & Replication\r\n\r\nx", false), "forwarded by a colleague");
        assert!(check("From: Ann <ann@friend.example>\r\nSubject: Fwd: Critical update for your firewalls\r\n\r\nx", true), "forwarded by someone the user writes to");
        assert!(!check("From: Stranger <x@y.example>\r\nSubject: Fwd: security update\r\n\r\nx", false), "a stranger's forward proves nothing");
        let soc = "Authentication-Results: mx; dmarc=pass header.from=soc.example\r\nFrom: SOC <alerts@soc.example>\r\nSubject: Security alert: Critical Risk - Malicious Binary on SERVER1\r\n\r\nx";
        assert!(!check(soc, false), "a SOC service's detections aren't security notices");
        let webinar = "Authentication-Results: mx; dmarc=pass header.from=vendor.example\r\nFrom: Vendor <events@vendor.example>\r\nSubject: Webinar: patch Tuesday explained\r\n\r\nx";
        assert!(!check(webinar, false), "marketing about security");
        let fake = "Authentication-Results: mx; dmarc=fail header.from=microsoft.com\r\nFrom: Microsoft <security@microsoft.com>\r\nSubject: Security alert\r\n\r\nx";
        assert!(!check(fake, false), "failing its checks: never");
    }

    #[test]
    fn the_library_is_tagged_new_mail_first_then_older_and_listed() {
        use crate::library::Access;
        use crate::progress::Silent;
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(&dir.path().join("lib"), Access::Write, &Silent).unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mail = |n: u32, subject: &str| {
            let p = src.join(format!("{n}.eml"));
            std::fs::write(&p, format!("From: a@shop.example\r\nSubject: {subject}\r\nDate: Mon, {n} Jan 2024 10:00:00 +0000\r\nMessage-ID: <{n}@x>\r\n\r\nhi\r\n")).unwrap();
            p
        };
        let files: Vec<_> = (1..=5).map(|n| mail(n, if n % 2 == 0 { "Your receipt" } else { "Hello" })).collect();
        crate::import(&mut library, &files, &crate::ImportOptions::default(), &Silent).unwrap();
        while library.tag_views(2).unwrap() > 0 {}
        assert_eq!(library.count_view("receipts", &[]).unwrap(), 2);
        // new mail after: tagged first, not gone back over
        crate::import(&mut library, &[mail(6, "Payment receipt")], &crate::ImportOptions::default(), &Silent).unwrap();
        assert_eq!(library.tag_views(10).unwrap(), 1);
        assert_eq!(library.tag_views(10).unwrap(), 0);
        let page = library.list_view("receipts", &[], None, 2).unwrap();
        assert_eq!(page.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), vec!["Payment receipt", "Your receipt"]);
        let after = library.cursor_view("receipts", &[], 1).unwrap();
        assert_eq!(library.list_view("receipts", &[], after, 5).unwrap().len(), 1);
        assert_eq!(library.views(&[]).unwrap().iter().find(|v| v.key == "receipts").unwrap().messages, 3);
        assert_eq!(library.view_groups("receipts", &[]).unwrap(), vec![("Shop".to_string(), 3)]);
        assert_eq!(library.count_view("receipts/Shop", &[]).unwrap(), 3, "a group lists like a view");

        // per mailbox: a shared mailbox's mail is in its own views, not the combined ones
        let account: i64 = library.conn.query_row("SELECT account FROM folders LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(library.count_view("receipts", &[account]).unwrap(), 3);
        assert_eq!(library.count_view("receipts", &[account + 100]).unwrap(), 0);

        // deleted (in Deleted Items): out of the views; moved back: in again
        let (newest, folder): (i64, i64) = library.conn.query_row("SELECT m.id, l.folder FROM messages m JOIN locations l ON l.message = m.id WHERE m.subject = 'Payment receipt'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let trash = crate::db::ensure_folder(&library.conn, account, "Deleted Items").unwrap();
        library.move_messages(&[newest], folder, trash).unwrap();
        assert_eq!(library.count_view("receipts", &[]).unwrap(), 2);
        assert_eq!(library.count_view("receipts", &[account]).unwrap(), 2);
        assert!(library.list_view("receipts", &[], None, 5).unwrap().iter().all(|r| r.id != newest));
        assert_eq!(library.view_groups("receipts", &[]).unwrap(), vec![("Shop".to_string(), 2)]);
        library.move_messages(&[newest], trash, folder).unwrap();
        assert_eq!(library.count_view("receipts", &[]).unwrap(), 3);

        library.conn.execute("UPDATE accounts SET kind = 'graph-shared', address = 'noc@x.com' WHERE id = ?1", [account]).unwrap();
        assert!(library.combined_view_accounts().unwrap().is_empty(), "shared mailboxes are off by default");
        library.set_views_accounts(&["NOC@x.com".into()]).unwrap();
        assert_eq!(library.combined_view_accounts().unwrap(), vec![account]);
    }
}
