//! Who hosts an address's mail, so adding an account goes the right way.
//!
//! Known personal domains first; then the domain's MX record, asked of the computer's own DNS
//! (the question any mail server asks: nothing is sent to Microsoft or Google). Microsoft 365
//! domains receive mail at `*.mail.protection.outlook.com`, Google Workspace at Google's servers.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MailHost {
    /// Outlook.com / Hotmail or Microsoft 365: sign in with Microsoft.
    Microsoft,
    /// Gmail or Google Workspace.
    Google,
}

/// Microsoft's personal mail domains (Outlook.com and its older names).
const MICROSOFT_PERSONAL: &[&str] = &[
    "outlook.com", "hotmail.com", "live.com", "msn.com", "passport.com", "windowslive.com",
    "outlook.co.uk", "hotmail.co.uk", "live.co.uk", "outlook.fr", "hotmail.fr", "live.fr", "outlook.de", "hotmail.de", "live.de",
    "outlook.it", "hotmail.it", "live.it", "outlook.es", "hotmail.es", "outlook.com.au", "live.com.au", "hotmail.ca", "live.ca",
    "outlook.jp", "hotmail.co.jp", "live.jp", "outlook.com.br", "hotmail.com.br", "outlook.in", "live.in", "outlook.ie", "live.ie",
    "outlook.be", "hotmail.be", "live.be", "outlook.nl", "hotmail.nl", "live.nl", "outlook.at", "outlook.dk", "hotmail.dk",
    "outlook.se", "hotmail.se", "live.se", "outlook.cl", "outlook.com.ar", "live.com.ar", "outlook.com.mx", "live.com.mx",
];

const GOOGLE_PERSONAL: &[&str] = &["gmail.com", "googlemail.com"];

/// Whether `address` is a personal Microsoft account (Outlook.com, Hotmail, Live, MSN…).
pub fn is_microsoft_personal(address: &str) -> bool {
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or(address).trim().trim_end_matches('.').to_ascii_lowercase();
    MICROSOFT_PERSONAL.contains(&domain.as_str())
}

/// Who hosts mail for `address` (None: someone else, or it couldn't be told).
pub fn mail_host(address: &str) -> Option<MailHost> {
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or(address).trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    if MICROSOFT_PERSONAL.contains(&domain.as_str()) {
        return Some(MailHost::Microsoft);
    }
    if GOOGLE_PERSONAL.contains(&domain.as_str()) {
        return Some(MailHost::Google);
    }
    host_of_exchangers(&mx_records(&domain))
}

/// The host the mail exchangers (MX names) belong to.
fn host_of_exchangers(exchangers: &[String]) -> Option<MailHost> {
    let first = exchangers.iter().map(|e| e.trim_end_matches('.').to_ascii_lowercase()).find(|e| !e.is_empty())?;
    if first.ends_with(".mail.protection.outlook.com") || first.ends_with(".olc.protection.outlook.com") {
        Some(MailHost::Microsoft)
    } else if first.ends_with(".google.com") || first.ends_with(".googlemail.com") || first == "smtp.google.com" {
        Some(MailHost::Google)
    } else {
        None
    }
}

/// The domain's mail exchangers, most preferred first (empty when there are none or DNS
/// couldn't be asked). One piece of code on every system: `hickory-resolver` reads the
/// computer's own DNS settings (Windows, the Mac, Linux) and asks those servers.
fn mx_records(domain: &str) -> Vec<String> {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return Vec::new() };
    runtime.block_on(async {
        let Ok(builder) = hickory_resolver::Resolver::builder_tokio() else { return Vec::new() };
        let Ok(resolver) = builder.build() else { return Vec::new() };
        let Ok(Ok(found)) = tokio::time::timeout(std::time::Duration::from_secs(5), resolver.mx_lookup(domain)).await else { return Vec::new() };
        let mut all: Vec<(u16, String)> = found
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                hickory_resolver::proto::rr::RData::MX(mx) => Some((mx.preference, mx.exchange.to_ascii())),
                _ => None,
            })
            .collect();
        all.sort_by_key(|(preference, _)| *preference);
        all.into_iter().map(|(_, name)| name).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_domains_and_mail_exchangers_say_who_hosts_the_mail() {
        assert_eq!(mail_host("someone@Outlook.com"), Some(MailHost::Microsoft));
        assert_eq!(mail_host("someone@hotmail.co.uk"), Some(MailHost::Microsoft));
        assert_eq!(mail_host("someone@gmail.com"), Some(MailHost::Google));
        assert_eq!(host_of_exchangers(&["contoso-com.mail.protection.outlook.com".into()]), Some(MailHost::Microsoft));
        assert_eq!(host_of_exchangers(&["aspmx.l.google.com.".into(), "alt1.aspmx.l.google.com".into()]), Some(MailHost::Google));
        assert_eq!(host_of_exchangers(&["smtp.google.com".into()]), Some(MailHost::Google));
        assert_eq!(host_of_exchangers(&["mail.example.org".into()]), None);
        assert_eq!(host_of_exchangers(&[]), None);
    }

    /// Asks real DNS (network needed): `cargo test -p eudora-core mail_host -- --ignored`.
    #[test]
    #[ignore]
    fn real_domains_are_told_apart_by_their_mail_exchangers() {
        assert_eq!(mail_host("someone@microsoft.com"), Some(MailHost::Microsoft), "{:?}", mx_records("microsoft.com"));
        assert_eq!(mail_host("someone@google.com"), Some(MailHost::Google), "{:?}", mx_records("google.com"));
        assert_eq!(mail_host("someone@example.org"), None, "{:?}", mx_records("example.org"));
    }
}
