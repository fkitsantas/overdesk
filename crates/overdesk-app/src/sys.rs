// SPDX-License-Identifier: AGPL-3.0-only
//! The operating system as the app meets it: starting the platform and controller, the
//! computer's name and address, permissions, and relaunching.

use std::net::{IpAddr, SocketAddr};

use overdeskd::controller::{self, AppState, Command, StartError};
use overdeskd::native::{self, NativePlatform};
use overdeskd::{Me, store_dir};
use tokio::sync::mpsc;

use super::{PORT, Permission, SPAWN, STATE};

/// Starts the native platform and the controller, and hands back its command sender
/// and first state.
pub(super) async fn start() -> Result<(mpsc::UnboundedSender<Command>, AppState), String> {
    let spawn = SPAWN.get().ok_or("the app was started without a controller")?;
    // Both can block (the OS grants capture and injection, `scutil` answers), and neither needs
    // the other: start them together, off the async threads.
    let (platform, name) = iced::futures::future::join(
        tokio::task::spawn_blocking(NativePlatform::start),
        tokio::task::spawn_blocking(device_name),
    )
    .await;
    let platform = platform
        .map_err(|e| e.to_string())?
        .map_err(|e| start_error_text(&StartError::Input(e)))?;
    let (width, height) = overdeskd::native::main_display_size();
    overdesk_proto::log::info(format_args!(
        "displays: {}",
        overdeskd::native::monitors()
            .iter()
            .map(|m| format!("{}x{}", m.w, m.h))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let config = controller::Config {
        store_dir: store_dir(),
        listen: SocketAddr::from(([0, 0, 0, 0], PORT)),
        discovery: true,
        admit: None,
        me: Me {
            name: name.unwrap_or_else(|_| "This computer".into()),
            os: overdeskd::this_os(),
            width,
            height,
            features: 0,
        },
    };
    let (commands, state) = spawn(config, platform).await.map_err(|e| start_error_text(&e))?;
    let now = state.borrow().clone();
    // The subscription reads the state from here; it is set once per launch.
    let _ = STATE.set(state);
    Ok((commands, now))
}

/// What a person should read when the controller could not start, in place of the raw error.
pub(super) fn start_error_text(e: &StartError) -> String {
    overdesk_proto::log::warn(format_args!("could not start: {e}"));
    match e {
        StartError::Bind(_) => format!(
            "Overdesk couldn't open its network port ({PORT}). Another copy of Overdesk may already be \
             running. Close it and try again."
        ),
        StartError::Store(_) => "Overdesk couldn't open its settings folder, where it keeps this computer's \
             identity and the computers you paired. Check that your user can write to it."
            .into(),
        StartError::Input(native::StartError::PermissionDenied) => {
            "Overdesk needs permission to see and control the keyboard and mouse. Allow it in System Settings, \
             then try again."
                .into()
        }
        StartError::Input(_) => "Overdesk couldn't set up keyboard and mouse sharing on this computer.".into(),
        StartError::Layout(_) => {
            "Overdesk couldn't set up the screen layout of this computer. Check that a monitor is \
             connected, then try again."
                .into()
        }
    }
}

/// What `program` prints on success, trimmed; `None` if it fails or prints nothing.
#[cfg(any(target_os = "macos", all(free_unix, not(target_os = "linux"))))]
pub(super) fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// This device's name as people know it: the macOS computer name, the Windows one, or the
/// Linux or BSD hostname.
pub(super) fn device_name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = command_output("scutil", &["--get", "ComputerName"]) {
        return name;
    }
    #[cfg(free_unix)]
    if let Some(name) = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
    {
        return name;
    }
    // FreeBSD has no /etc/hostname and a desktop session rarely exports HOSTNAME.
    #[cfg(all(free_unix, not(target_os = "linux")))]
    if let Some(name) = command_output("hostname", &[]) {
        return name;
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This computer".into())
}

/// Whether Input Monitoring and Accessibility are granted.
#[cfg(target_os = "macos")]
pub(super) fn permissions() -> (bool, bool) {
    overdeskd::native::permissions()
}

/// Other operating systems need no permissions, so both count as granted.
#[cfg(not(target_os = "macos"))]
pub(super) fn permissions() -> (bool, bool) {
    (true, true)
}

/// Opens System Settings at the pane that grants `which`, on macOS.
pub(super) fn open_settings(which: Permission) {
    #[cfg(target_os = "macos")]
    {
        overdeskd::native::request_permissions();
        let pane = match which {
            Permission::InputMonitoring => "Privacy_ListenEvent",
            Permission::Accessibility => "Privacy_Accessibility",
        };
        let _ = std::process::Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{pane}"
            ))
            .spawn();
    }
    // Other platforms have no settings pane to open.
    let _ = which;
}

/// Opens `target` (a web address) in the default browser. Returns once the opener has started.
pub(super) fn open(target: &str) -> std::io::Result<()> {
    use std::process::Command;
    // No shell on Windows: `&` in the address would end a `cmd` command.
    let mut opener = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    opener.arg(target).spawn().map(drop)
}

/// Shows `file` in the file manager: selected in Finder or Explorer, or its folder on Linux and the BSDs.
pub(super) fn reveal(file: &std::path::Path) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    return Command::new("open").arg("-R").arg(file).spawn().map(drop);
    // Raw, since std would quote `/select,<path>` as one word, which Explorer misreads.
    #[cfg(windows)]
    return std::os::windows::process::CommandExt::raw_arg(&mut Command::new("explorer"), select_arg(file))
        .spawn()
        .map(drop);
    #[cfg(not(any(target_os = "macos", windows)))]
    Command::new("xdg-open")
        .arg(file.parent().unwrap_or(file))
        .spawn()
        .map(drop)
}

/// Explorer's argument that selects `file`: `/select,` then the path in quotes on its own, so spaces
/// and commas stay in the path, with `\` separators (Explorer ignores the request given `/`).
#[cfg(any(windows, test))]
fn select_arg(file: &std::path::Path) -> String {
    format!("/select,\"{}\"", file.display().to_string().replace('/', "\\"))
}

/// Starts a fresh process, since permissions take effect for a new process only. Inside a
/// macOS `.app` bundle it goes through `open -n`, so LaunchServices attributes the
/// permissions to the bundle rather than to the terminal or parent that started it. An
/// AppImage runs from a temporary mount, so it restarts the file `$APPIMAGE` names instead
/// (which may just have been replaced). Never returns: this process
/// exits, whether or not the new one started.
pub fn relaunch() -> ! {
    let exe = std::env::var_os("APPIMAGE")
        .filter(|_| cfg!(target_os = "linux"))
        .map(std::path::PathBuf::from)
        .map_or_else(std::env::current_exe, Ok);
    if let Ok(exe) = exe {
        let path = exe.to_string_lossy();
        let bundle = if cfg!(target_os = "macos") {
            path.split_once(".app/Contents/MacOS/")
                .map(|(prefix, _)| format!("{prefix}.app"))
        } else {
            None
        };
        let _ = match bundle {
            Some(bundle) => std::process::Command::new("open").arg("-n").arg(bundle).spawn(),
            None => std::process::Command::new(exe).spawn(),
        };
    }
    overdesk_proto::log::flush(); // a pending "repeated N times" is not lost at exit
    std::process::exit(0);
}

/// Parses `ip:port`, or a bare IP on the default port.
pub(super) fn parse_address(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    s.parse::<SocketAddr>()
        .ok()
        .or_else(|| s.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, PORT)))
}

/// This computer's address on the local network, as the route to a public address would use
/// it. A UDP `connect` sends nothing; it only makes the OS pick the interface.
pub(super) fn lan_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?; // TEST-NET-1: never routed, so nothing leaves
    socket
        .local_addr()
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_unspecified())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_may_leave_out_the_port() {
        assert_eq!(
            parse_address(" 10.0.0.2:7 "),
            Some(SocketAddr::from(([10, 0, 0, 2], 7)))
        );
        assert_eq!(parse_address("10.0.0.2"), Some(SocketAddr::from(([10, 0, 0, 2], PORT))));
        assert_eq!(parse_address("my-mac.local"), None);
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn each_start_failure_has_its_own_sentence_without_the_raw_error() {
        let detail = "raw-os-detail";
        let errors = [
            StartError::Bind(detail.into()),
            StartError::Store(detail.into()),
            StartError::Layout(detail.into()),
            StartError::Input(native::StartError::PermissionDenied),
            StartError::Input(native::StartError::Capture(detail.into())),
        ];
        let texts: Vec<_> = errors.iter().map(start_error_text).collect();
        for (i, text) in texts.iter().enumerate() {
            assert!(!text.contains(detail), "{text}");
            assert!(!texts[..i].contains(text), "two failures read the same: {text}");
        }
        assert!(
            texts[0].contains(&PORT.to_string()),
            "the port in use is named: {}",
            texts[0]
        );
    }

    #[test]
    fn explorer_gets_the_path_quoted_on_its_own_with_backslashes() {
        let arg = |p: &str| select_arg(std::path::Path::new(p));
        assert_eq!(
            arg(r"C:\Users\Ada B\Downloads\d.txt"),
            r#"/select,"C:\Users\Ada B\Downloads\d.txt""#
        );
        assert_eq!(arg(r"D:\A, B\x,y.txt"), r#"/select,"D:\A, B\x,y.txt""#);
        assert_eq!(arg("C:/Users/Ada/d.txt"), r#"/select,"C:\Users\Ada\d.txt""#);
    }
}
