//! Letting phones on this network reach `eudora-web` through the computer's firewall
//! (Settings ▸ Web access). Windows: Eudora's own Windows Firewall rule, changed through an
//! elevated `netsh` (Windows asks the user). The Mac's application firewall asks by itself the
//! first time `eudora-web` accepts a connection, and Linux distributions differ (ufw, firewalld,
//! or none), so there Eudora only explains (`supported` false). Platform code lives only here
//! (2026-10-14 review: this code once kept the window from compiling on the Mac and Linux).

use serde::Serialize;

/// The Windows Firewall rule that lets phones on this network reach `eudora-web`.
#[cfg(windows)]
const FIREWALL_RULE: &str = "Eudora web access";
/// The rule's name before 1.0.0 (removed whenever the rule is changed).
#[cfg(windows)]
const OLD_FIREWALL_RULE: &str = "Eudora 27 web access";

#[derive(Serialize)]
pub struct FirewallStatus {
    /// Eudora can change this computer's firewall (Windows).
    pub supported: bool,
    /// The rule exists and is on.
    pub allowed: bool,
    /// The port it opens, as the firewall lists it.
    pub ports: String,
    /// For the tooltip: which networks, and who may connect; or, where Eudora can't change the
    /// firewall, what to do instead.
    pub detail: String,
}

/// Runs `netsh` with no window (reading rules needs no administrator rights).
#[cfg(windows)]
fn netsh(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("netsh").args(args).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether Eudora's firewall rule is there, and for which port.
#[cfg(windows)]
pub fn status() -> Result<FirewallStatus, String> {
    let out = netsh(&["advfirewall", "firewall", "show", "rule", &format!("name={FIREWALL_RULE}"), "verbose"])?;
    let field = |name: &str| out.lines().find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string())).unwrap_or_default();
    let allowed = out.contains(FIREWALL_RULE) && field("Enabled:").eq_ignore_ascii_case("yes");
    Ok(FirewallStatus {
        supported: true,
        allowed,
        ports: if allowed { field("LocalPort:") } else { String::new() },
        detail: if allowed { format!("{} networks, from {}", field("Profiles:"), field("RemoteIP:")) } else { String::new() },
    })
}

/// Allows inbound connections to `port` from this local network on Private networks (or,
/// with `allow` false, removes the rule). Changing the firewall needs administrator rights:
/// Windows asks the user (UAC), and nothing changes unless they approve.
#[cfg(windows)]
pub fn set(port: u16, allow: bool) -> Result<FirewallStatus, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if allow && port == 0 {
        return Err("choose a port".into());
    }
    let rule = format!("name=\"{FIREWALL_RULE}\"");
    // (an old rule always goes first, so a changed port replaces it)
    let mut commands = vec![format!("netsh advfirewall firewall delete rule {rule}"), format!("netsh advfirewall firewall delete rule name=\"{OLD_FIREWALL_RULE}\"")];
    if allow {
        commands.push(format!(
            "netsh advfirewall firewall add rule {rule} dir=in action=allow protocol=TCP localport={port} profile=private remoteip=localsubnet description=\"Lets phones and computers on this network reach Eudora on the web (eudora-web). Made in Eudora's Settings.\""
        ));
    }
    let script = commands.join(" & ");
    // an elevated cmd.exe (Windows shows its permission prompt); -Wait so the status read
    // afterwards is current
    let arguments = format!("'/c {}'", script.replace('\'', "''"));
    let done = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("try {{ Start-Process cmd.exe -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList {arguments} -ErrorAction Stop }} catch {{ exit 1 }}")])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if !done.success() {
        return Err("Windows didn't make the change: the permission prompt was declined".into());
    }
    status()
}

#[cfg(not(windows))]
fn elsewhere() -> String {
    if cfg!(target_os = "macos") {
        "The Mac asks by itself (“Accept incoming network connections?”) the first time eudora-web is reached from another device: choose Allow. System Settings ▸ Network ▸ Firewall ▸ Options lists it afterwards.".into()
    } else {
        "If this computer runs a firewall (ufw, firewalld…), open eudora-web's port for the local network there, e.g. `sudo ufw allow from 192.168.0.0/16 to any port 443 proto tcp`.".into()
    }
}

#[cfg(not(windows))]
pub fn status() -> Result<FirewallStatus, String> {
    Ok(FirewallStatus { supported: false, allowed: false, ports: String::new(), detail: elsewhere() })
}

#[cfg(not(windows))]
pub fn set(_port: u16, _allow: bool) -> Result<FirewallStatus, String> {
    Err(elsewhere())
}
