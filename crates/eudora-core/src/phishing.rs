//! Fraud (phishing) checks.
//!
//! The authentication checks only say whether the sending domain is genuine, and a fraud's own
//! domain usually is. What gives it away is a message **claiming to be a brand it isn't sent
//! by**: the brand in the sender's name or the subject, the mail from somewhere else. Then the
//! usual lures (sign a document, reset a password, a full mailbox, an unusual sign-in), and the
//! user's own company's name used by an outside domain.

use crate::triage::Signals;
use crate::triage_decide::org_domain;

/// Brands frauds pretend to be: the words that name them, and the organizational domains their
/// real mail comes from.
struct Brand {
    name: &'static str,
    words: &'static [&'static str],
    domains: &'static [&'static str],
}

const fn b(name: &'static str, words: &'static [&'static str], domains: &'static [&'static str]) -> Brand {
    Brand { name, words, domains }
}

const BRANDS: &[Brand] = &[
    b("DocuSign", &["docusign", "docu-sign", "docu sign"], &["docusign.net", "docusign.com"]),
    b("Adobe Sign", &["adobe sign", "adobesign", "echosign", "adobe acrobat sign"], &["adobe.com", "adobesign.com", "echosign.com"]),
    b("Microsoft", &["microsoft", "office 365", "office365", "microsoft 365", "outlook web", "onedrive", "sharepoint", "microsoft teams", "azure"], &["microsoft.com", "office.com", "office365.com", "microsoftonline.com", "sharepointonline.com", "outlook.com", "live.com", "onedrive.com", "azure.com", "microsoftsupport.com", "skype.com", "teams.mail.microsoft"]),
    b("PayPal", &["paypal"], &["paypal.com", "paypal.me"]),
    b("Apple", &["apple id", "icloud", "apple support", "itunes", "app store"], &["apple.com", "icloud.com", "me.com"]),
    b("Amazon", &["amazon"], &["amazon.com", "amazon.co.uk", "amazon.ca", "amazon.de", "amazon.it", "amazonses.com", "amazonaws.com", "aws.com"]),
    b("Google", &["google", "gmail", "google drive", "google docs"], &["google.com", "gmail.com", "googlemail.com", "youtube.com"]),
    b("Dropbox", &["dropbox"], &["dropbox.com", "dropboxmail.com"]),
    b("Netflix", &["netflix"], &["netflix.com"]),
    b("Wells Fargo", &["wells fargo", "wellsfargo"], &["wellsfargo.com"]),
    b("Chase", &["chase bank", "jpmorgan chase", "chase online"], &["chase.com", "jpmorgan.com", "jpmchase.com"]),
    b("Bank of America", &["bank of america", "bankofamerica"], &["bankofamerica.com", "bofa.com"]),
    b("the IRS", &["irs", "internal revenue service"], &["irs.gov", "treasury.gov"]),
    b("FedEx", &["fedex"], &["fedex.com"]),
    b("UPS", &["ups delivery", "ups package", "ups shipment", "united parcel"], &["ups.com"]),
    b("USPS", &["usps", "postal service"], &["usps.com", "usps.gov"]),
    b("DHL", &["dhl"], &["dhl.com", "dhl.de"]),
    b("Zoom", &["zoom meeting", "zoom video"], &["zoom.us", "zoom.com"]),
    b("Intuit", &["quickbooks", "intuit", "turbotax"], &["intuit.com", "quickbooks.com"]),
    b("Norton", &["norton", "lifelock"], &["norton.com", "nortonlifelock.com", "gen.com"]),
    b("McAfee", &["mcafee"], &["mcafee.com"]),
    b("Geek Squad", &["geek squad", "geeksquad"], &["bestbuy.com", "geeksquad.com"]),
    b("Coinbase", &["coinbase"], &["coinbase.com"]),
    b("LinkedIn", &["linkedin"], &["linkedin.com"]),
    b("Facebook", &["facebook", "meta business", "instagram", "whatsapp"], &["facebook.com", "facebookmail.com", "meta.com", "instagram.com", "whatsapp.com"]),
    b("Venmo", &["venmo"], &["venmo.com"]),
    b("Zelle", &["zelle"], &["zellepay.com"]),
    b("Webmail", &["webmail", "email administrator", "mail administrator", "it helpdesk", "it help desk", "mailbox administrator"], &[]),
];

/// What frauds ask for: a document to sign, a password, a "verification", a full mailbox…
const LURES: &[&str] = &[
    "document available", "documents available", "review document", "review the document", "sign the document", "please sign", "signature required", "e-signature", "esignature",
    "complete via", "complete with", "shared a file", "shared a document", "shared with you", "sent you a file", "secure message", "secure document", "encrypted message",
    "password reset", "reset your password", "password expires", "password expired", "password will expire", "change your password", "verify your account", "verify your identity",
    "confirm your account", "confirm your identity", "account suspended", "account will be suspended", "account has been suspended", "account locked", "account disabled",
    "unusual sign-in", "unusual sign in", "unusual activity", "suspicious activity", "sign-in attempt", "mailbox full", "mailbox is full", "storage full", "storage is full",
    "quota exceeded", "quarantined", "messages on hold", "pending messages", "undelivered messages", "voicemail", "voice message", "fax received", "invoice attached",
    "payment failed", "update your payment", "billing problem", "action required", "immediate action", "final warning", "important document", "important notice",
];

/// What the checks found: each one's spam weight and why, for the decision's list.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub rule: &'static str,
    pub score: f32,
    pub why: String,
}

/// Whether `word` appears in `text` as whole words (both lowercase).
fn has_words(text: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(at) = text[from..].find(word) {
        let start = from + at;
        let end = start + word.len();
        let before = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = text[end..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// The brand a sender name or subject claims, when the mail isn't from that brand.
fn claimed_brand(text: &str, from_org: &str) -> Option<&'static Brand> {
    let text = text.to_lowercase();
    BRANDS.iter().find(|b| b.words.iter().any(|w| has_words(&text, w)) && !b.domains.contains(&from_org))
}

/// The fraud checks for one message. `own_names`: the user's own organizations' names
/// ("example", "planet"), from their domains; `trusted`: the sender is on the whitelist.
///
/// `text`: the start of the message's text: its lures count too, and a brand it names next to a
/// lure.
pub fn check(s: &Signals, subject: &str, text: &str, own_domains: &[String], trusted: bool) -> Vec<Finding> {
    let mut out = Vec::new();
    if s.from.is_empty() || trusted {
        return out;
    }
    let from_org = org_domain(&s.from_domain);
    let name = s.from_name.to_lowercase();
    let subject_lower = subject.to_lowercase();
    let lure = LURES.iter().find(|l| subject_lower.contains(*l) || name.contains(*l));
    let body: String = text.chars().take(3000).collect::<String>().to_lowercase();
    let body_lure = LURES.iter().find(|l| body.contains(*l));
    // 1. a brand in the sender's name, the mail from somewhere else: fraud
    if let Some(brand) = claimed_brand(&s.from_name, &from_org) {
        out.push(Finding { rule: "phishing", score: 6.0, why: format!("Fraud: its sender name says {} but it comes from {from_org}", brand.name) });
    } else if let Some(brand) = claimed_brand(subject, &from_org) {
        // 2. a brand in the subject with a lure ("Complete via DocuSign"): fraud; alone it may
        //    just be news about the brand
        match lure {
            Some(l) => out.push(Finding { rule: "phishing", score: 5.5, why: format!("Fraud: it claims to be {} (“{l}”) but comes from {from_org}", brand.name) }),
            None => out.push(Finding { rule: "phishing", score: 1.0, why: format!("it names {} but comes from {from_org}", brand.name) }),
        }
    }
    // 2b. the same in the text: a brand named next to a lure there, the mail from elsewhere
    else if let (Some(brand), Some(l)) = (claimed_brand(&body, &from_org), body_lure) {
        if !crate::triage_known::verified(s) {
            out.push(Finding { rule: "phishing", score: 3.0, why: format!("its text speaks for {} (“{l}”) but it comes from {from_org}", brand.name) });
        }
    }
    // 3. the user's own company's name in the sender's name, from an outside domain
    let own_orgs: Vec<String> = own_domains.iter().map(|d| org_domain(d)).collect();
    if !own_orgs.contains(&from_org) {
        if let Some(own) = own_orgs.iter().filter_map(|d| d.split('.').next()).find(|label| label.len() >= 4 && has_words(&name, label)) {
            out.push(Finding { rule: "phishing", score: 3.0, why: format!("it uses your organization's name ({own}) but comes from {from_org}") });
        }
    }
    // 4. a lure from a sender that isn't proven (a real service's reset proves its domain): in
    //    the subject or sender name, else in the text (a little less)
    if let (None, Some(l)) = (lure, body_lure) {
        if !crate::triage_known::verified(s) {
            out.push(Finding { rule: "phishing-lure", score: 1.5, why: format!("its text asks for what frauds do (“{l}”)") });
        }
    }
    if let Some(l) = lure {
        if !crate::triage_known::verified(s) || out.iter().any(|f| f.score >= 5.0) {
            out.push(Finding { rule: "phishing-lure", score: 2.0, why: format!("it asks for what frauds do (“{l}”)") });
        }
    }
    out
}

/// The warning to show above a message the checks say is a fraud (None: nothing serious).
pub fn warning(s: &Signals, subject: &str, text: &str, own_domains: &[String], trusted: bool) -> Option<String> {
    let found = check(s, subject, text, own_domains, trusted);
    let total: f32 = found.iter().map(|f| f.score).sum();
    (total >= 5.0).then(|| {
        let first = found.iter().max_by(|a, b| a.score.total_cmp(&b.score)).map(|f| f.why.clone()).unwrap_or_default();
        format!("{first}. Don't click its links, open its attachments or sign in from it.")
    })
}

/// A fraud's HTML with every link made dead: the attributes that lead anywhere (`href`, a
/// form's `action` / `formaction`, `ping`, `xlink:href`) are renamed, so nothing in it can be
/// followed; the text stays readable.
pub fn dead_links(html: &str) -> String {
    const LEADS: &[&str] = &["xlink:href", "formaction", "action", "href", "ping"];
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(html.len() + 64);
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        let before_ok = i > 0 && matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'/');
        if before_ok {
            if let Some(name) = LEADS.iter().find(|n| lower[i..].starts_with(*n)) {
                let mut j = i + name.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    out.push_str(&html[copied..i]);
                    out.push_str("data-dead-");
                    out.push_str(&name.replace(':', "-"));
                    copied = i + name.len();
                    i = copied;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&html[copied..]);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_frauds_links_are_dead() {
        let html = r##"<a href="https://evil.example/login">Review</a> <A HREF = 'x'>U</A><form action="https://evil.example/p"><button formaction="/x">Go</button></form><svg><a xlink:href="#y">s</a></svg><p>href=text stays</p>"##;
        let dead = super::dead_links(html);
        assert!(!dead.to_ascii_lowercase().contains(" href=") && !dead.contains("HREF ="), "{dead}");
        assert!(!dead.contains(" action=") && !dead.contains(" formaction=") && !dead.contains("xlink:href="), "{dead}");
        assert_eq!(dead.matches("data-dead-").count(), 5, "{dead}");
        assert!(dead.contains("Review") && dead.contains("<p>href=text stays</p>"), "{dead}");
    }
}

#[cfg(test)]
mod fraud_tests {
    use super::*;

    fn signals(raw: &str) -> Signals {
        crate::triage::signals(&crate::header_index::read_headers(raw.as_bytes()))
    }

    #[test]
    fn a_fake_docusign_is_fraud_and_a_real_one_isnt() {
        // a DocuSign lure from an unrelated domain, headers as received
        let fake = signals("From: Pat Via Docusign -via docusign Example <Example@docsign-alerts.com>\r\nSubject: Complete via DocuSign:IMPORTANT  DOCUMENT AVAILABLE for Example\r\nAuthentication-Results: example.org header.from=docsign-alerts.com; dkim=pass (good signature)\r\n\r\n");
        let own = vec!["example.org".to_string(), "example.com".to_string()];
        let found = check(&fake, "Complete via DocuSign:IMPORTANT  DOCUMENT AVAILABLE for Example", "", &own, false);
        let total: f32 = found.iter().map(|f| f.score).sum();
        assert!(total >= 5.0, "{found:?}");
        assert!(found[0].why.contains("DocuSign") && found[0].why.contains("docsign-alerts.com"), "{found:?}");
        assert!(found.iter().any(|f| f.why.contains("example")), "your company's name from outside: {found:?}");
        assert!(warning(&fake, "Complete via DocuSign", "", &own, false).is_some());
        // the real thing
        let real = signals("From: Ann Smith via Docusign <dse_NA3@docusign.net>\r\nSubject: Complete with Docusign: Contract.pdf\r\nAuthentication-Results: mx; dkim=pass header.d=docusign.net; dmarc=pass\r\n\r\n");
        let found = check(&real, "Complete with Docusign: Contract.pdf", "", &own, false);
        assert!(found.iter().map(|f| f.score).sum::<f32>() < 5.0, "{found:?}");
        // a newsletter naming a brand isn't fraud
        let news = signals("From: Tech News <news@technews.example>\r\nSubject: Microsoft ships a new Surface\r\n\r\n");
        assert!(check(&news, "Microsoft ships a new Surface", "", &own, false).iter().map(|f| f.score).sum::<f32>() < 5.0);
        // a fake Microsoft password notice
        let ms = signals("From: Microsoft 365 Admin <admin@secure-login.example>\r\nSubject: Your password expires today\r\n\r\n");
        assert!(check(&ms, "Your password expires today", "", &own, false).iter().map(|f| f.score).sum::<f32>() >= 5.0);
        // people the user writes to are never judged this way
        assert!(check(&fake, "Complete via DocuSign", "", &own, true).is_empty());
        // the body counts: a plain subject, Microsoft and a lure only in the text
        let plain = signals("From: Help Desk <help@secure-mail.example>\r\nSubject: Notice\r\n\r\n");
        let text = "Your Microsoft 365 password expires today. Verify your account to keep your mail.";
        let found = check(&plain, "Notice", text, &own, false);
        assert!(found.iter().any(|f| f.why.contains("Microsoft")), "{found:?}");
        assert!(found.iter().map(|f| f.score).sum::<f32>() >= 4.0, "{found:?}");
        // words, not pieces of words: "irs" isn't in "first"
        assert!(claimed_brand("First quarter results", "example.com").is_none());
    }
}
