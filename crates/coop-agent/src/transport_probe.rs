use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use tokio::task;

use crate::provider_spec::{ProviderKind, ProviderSpec};
use crate::request_trace::{
    TransportErrorTrace, TransportProbeTrace, summarize_transport_probe_reqwest_error,
    summarize_transport_probe_response,
};

const MODELS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_PROBE_TIMEOUT_SECS: &str = "2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandProbeTrace {
    pub target: &'static str,
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub related_interface: String,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub error_kind: &'static str,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SocketProbeTrace {
    pub target_host: String,
    pub target_port: u16,
    pub resolved_addrs: String,
    pub selected_local_addr: String,
    pub connect_ok: bool,
    pub connected_peer_addr: String,
    pub connected_local_addr: String,
    pub error_kind: &'static str,
    pub error: String,
}

pub(crate) fn build_probe_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(MODELS_PROBE_TIMEOUT)
        .build()?)
}

pub(crate) async fn probe_transport_failure(
    client: &reqwest::Client,
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
    auth_header: Option<&str>,
) -> Option<TransportProbeTrace> {
    if !should_probe_models_endpoint(kind, spec, transport) {
        return None;
    }

    let url = format!("{}models", spec.normalized_base_url()?);
    let mut request = client.get(&url);
    let has_authorization_override = spec
        .extra_headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case(AUTHORIZATION.as_str()));

    if !has_authorization_override && let Some(auth_header) = auth_header {
        request = request.header(AUTHORIZATION, auth_header);
    }

    for (name, value) in &spec.extra_headers {
        request = request.header(name, value);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            match response.text().await {
                Ok(body) => Some(summarize_transport_probe_response(
                    "models", &url, status, &body,
                )),
                Err(error) => Some(summarize_transport_probe_reqwest_error(
                    "models", &url, &error,
                )),
            }
        }
        Err(error) => Some(summarize_transport_probe_reqwest_error(
            "models", &url, &error,
        )),
    }
}

pub(crate) async fn probe_socket_transport_failure(
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
) -> Option<SocketProbeTrace> {
    if !should_probe_models_endpoint(kind, spec, transport) {
        return None;
    }

    let base_url = spec.normalized_base_url()?;
    task::spawn_blocking(move || probe_socket_target(&base_url))
        .await
        .ok()
}

pub(crate) async fn probe_command_transport_failure(
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
) -> Option<Vec<CommandProbeTrace>> {
    if !should_probe_models_endpoint(kind, spec, transport) {
        return None;
    }

    let base_url = spec.normalized_base_url()?;
    task::spawn_blocking(move || probe_command_targets(&base_url))
        .await
        .ok()
}

fn should_probe_models_endpoint(
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
) -> bool {
    kind == ProviderKind::OpenAiCompatible
        && spec.normalized_base_url().is_some()
        && transport.http_status.is_none()
        && (transport.reqwest_is_connect
            || transport.reqwest_is_timeout
            || transport.reqwest_is_request
            || transport.reqwest_is_body
            || transport.reqwest_is_decode)
}

fn probe_command_targets(base_url: &str) -> Vec<CommandProbeTrace> {
    let Ok(url) = Url::parse(base_url) else {
        return vec![CommandProbeTrace {
            target: "snapshot",
            command: String::new(),
            success: false,
            exit_code: None,
            duration_ms: 0,
            related_interface: String::new(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            error_kind: "invalid_url",
            error: truncate(&format!("failed to parse provider base URL '{base_url}'")),
        }];
    };

    let Some(host) = url.host_str() else {
        return vec![CommandProbeTrace {
            target: "snapshot",
            command: String::new(),
            success: false,
            exit_code: None,
            duration_ms: 0,
            related_interface: String::new(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            error_kind: "missing_host",
            error: truncate(&format!("provider base URL '{base_url}' is missing a host")),
        }];
    };
    let Some(port) = url.port_or_known_default() else {
        return vec![CommandProbeTrace {
            target: "snapshot",
            command: String::new(),
            success: false,
            exit_code: None,
            duration_ms: 0,
            related_interface: String::new(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            error_kind: "missing_port",
            error: truncate(&format!("provider base URL '{base_url}' is missing a port")),
        }];
    };

    let models_url = format!("{base_url}models");
    let mut probes = vec![
        run_probe_command(
            "curl_models",
            command_spec_for_curl(&models_url),
            String::new(),
        ),
        run_probe_command("nc_port", command_spec_for_nc(host, port), String::new()),
    ];

    let route_probe = run_probe_command("route_get", command_spec_for_route(host), String::new());
    let route_interface = extract_route_interface(&route_probe.stdout_excerpt);
    probes.push(route_probe);
    probes.push(run_probe_command(
        "neighbor",
        command_spec_for_neighbor(host),
        route_interface.clone(),
    ));

    if route_interface.is_empty() {
        probes.push(CommandProbeTrace {
            target: "interface",
            command: String::new(),
            success: false,
            exit_code: None,
            duration_ms: 0,
            related_interface: String::new(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            error_kind: "missing_interface",
            error: "route probe did not report an interface".to_owned(),
        });
    } else {
        probes.push(run_probe_command(
            "interface",
            command_spec_for_interface(&route_interface),
            route_interface,
        ));
    }

    probes
}

fn probe_socket_target(base_url: &str) -> SocketProbeTrace {
    let Ok(url) = Url::parse(base_url) else {
        return summarize_socket_probe_error(
            "",
            0,
            "",
            "",
            "invalid_url",
            &format!("failed to parse provider base URL '{base_url}'"),
        );
    };

    let Some(host) = url.host_str() else {
        return summarize_socket_probe_error(
            "",
            0,
            "",
            "",
            "missing_host",
            &format!("provider base URL '{base_url}' is missing a host"),
        );
    };
    let Some(port) = url.port_or_known_default() else {
        return summarize_socket_probe_error(
            host,
            0,
            "",
            "",
            "missing_port",
            &format!("provider base URL '{base_url}' is missing a port"),
        );
    };

    let resolved_addrs = match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(error) => {
            return summarize_socket_probe_error(
                host,
                port,
                "",
                "",
                socket_error_kind(&error),
                &error.to_string(),
            );
        }
    };
    let resolved_addr_list = resolved_addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    if resolved_addrs.is_empty() {
        return summarize_socket_probe_error(
            host,
            port,
            "",
            "",
            "no_addresses",
            "socket probe resolved no addresses",
        );
    }

    let selected_local_addr = select_local_addr(&resolved_addrs).unwrap_or_default();

    for (index, addr) in resolved_addrs.iter().enumerate() {
        match TcpStream::connect_timeout(addr, SOCKET_PROBE_TIMEOUT) {
            Ok(stream) => {
                let connected_local_addr = stream
                    .local_addr()
                    .map_or_else(|_| String::new(), |value| value.to_string());
                return summarize_socket_probe_success(
                    host,
                    port,
                    &resolved_addr_list,
                    &selected_local_addr,
                    &addr.to_string(),
                    &connected_local_addr,
                );
            }
            Err(error) => {
                if index + 1 == resolved_addrs.len() {
                    return summarize_socket_probe_error(
                        host,
                        port,
                        &resolved_addr_list,
                        &selected_local_addr,
                        socket_error_kind(&error),
                        &error.to_string(),
                    );
                }
            }
        }
    }

    summarize_socket_probe_error(
        host,
        port,
        &resolved_addr_list,
        &selected_local_addr,
        "no_addresses",
        "socket probe resolved no addresses",
    )
}

fn select_local_addr(resolved_addrs: &[std::net::SocketAddr]) -> Option<String> {
    for addr in resolved_addrs {
        let bind_addr = if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let Ok(socket) = UdpSocket::bind(bind_addr) else {
            continue;
        };
        if socket.connect(addr).is_err() {
            continue;
        }
        if let Ok(local_addr) = socket.local_addr() {
            return Some(local_addr.to_string());
        }
    }
    None
}

fn summarize_socket_probe_success(
    target_host: &str,
    target_port: u16,
    resolved_addrs: &str,
    selected_local_addr: &str,
    connected_peer_addr: &str,
    connected_local_addr: &str,
) -> SocketProbeTrace {
    SocketProbeTrace {
        target_host: target_host.to_owned(),
        target_port,
        resolved_addrs: resolved_addrs.to_owned(),
        selected_local_addr: selected_local_addr.to_owned(),
        connect_ok: true,
        connected_peer_addr: connected_peer_addr.to_owned(),
        connected_local_addr: connected_local_addr.to_owned(),
        error_kind: "",
        error: String::new(),
    }
}

fn summarize_socket_probe_error(
    target_host: &str,
    target_port: u16,
    resolved_addrs: &str,
    selected_local_addr: &str,
    error_kind: &'static str,
    error: &str,
) -> SocketProbeTrace {
    SocketProbeTrace {
        target_host: target_host.to_owned(),
        target_port,
        resolved_addrs: resolved_addrs.to_owned(),
        selected_local_addr: selected_local_addr.to_owned(),
        connect_ok: false,
        connected_peer_addr: String::new(),
        connected_local_addr: String::new(),
        error_kind,
        error: truncate(error),
    }
}

fn socket_error_kind(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::ConnectionRefused => "connection_refused",
        ErrorKind::ConnectionReset => "connection_reset",
        ErrorKind::ConnectionAborted => "connection_aborted",
        ErrorKind::NotConnected => "not_connected",
        ErrorKind::AddrInUse => "addr_in_use",
        ErrorKind::AddrNotAvailable => "addr_not_available",
        ErrorKind::BrokenPipe => "broken_pipe",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::WouldBlock => "would_block",
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvalidData => "invalid_data",
        ErrorKind::TimedOut => "timed_out",
        ErrorKind::WriteZero => "write_zero",
        ErrorKind::Interrupted => "interrupted",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::UnexpectedEof => "unexpected_eof",
        ErrorKind::OutOfMemory => "out_of_memory",
        ErrorKind::HostUnreachable => "host_unreachable",
        ErrorKind::NetworkUnreachable => "network_unreachable",
        _ => "other",
    }
}

fn truncate(value: &str) -> String {
    const MAX_LEN: usize = 400;
    if value.len() <= MAX_LEN {
        value.to_owned()
    } else {
        format!("{}…", &value[..MAX_LEN])
    }
}

fn run_probe_command(
    target: &'static str,
    spec: CommandSpec,
    related_interface: String,
) -> CommandProbeTrace {
    let CommandSpec { candidates, args } = spec;
    let args_display = args.join(" ");

    for candidate in &candidates {
        let command_display = if args_display.is_empty() {
            (*candidate).to_owned()
        } else {
            format!("{candidate} {args_display}")
        };
        let started = std::time::Instant::now();

        match Command::new(candidate).args(&args).output() {
            Ok(output) => {
                let duration_ms = duration_to_millis_u64(started.elapsed());
                let exit_code = output.status.code();
                let success = output.status.success();
                let stdout_excerpt = truncate(&String::from_utf8_lossy(&output.stdout));
                let stderr_excerpt = truncate(&String::from_utf8_lossy(&output.stderr));
                let (error_kind, error) = if success {
                    ("", String::new())
                } else {
                    (
                        "exit_status",
                        output.status.code().map_or_else(
                            || "process terminated by signal".to_owned(),
                            |code| format!("command exited with status {code}"),
                        ),
                    )
                };

                return CommandProbeTrace {
                    target,
                    command: command_display,
                    success,
                    exit_code,
                    duration_ms,
                    related_interface,
                    stdout_excerpt,
                    stderr_excerpt,
                    error_kind,
                    error,
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return CommandProbeTrace {
                    target,
                    command: command_display,
                    success: false,
                    exit_code: None,
                    duration_ms: duration_to_millis_u64(started.elapsed()),
                    related_interface,
                    stdout_excerpt: String::new(),
                    stderr_excerpt: String::new(),
                    error_kind: socket_error_kind(&error),
                    error: truncate(&error.to_string()),
                };
            }
        }
    }

    CommandProbeTrace {
        target,
        command: if args_display.is_empty() {
            candidates.join(" | ")
        } else {
            format!("{} {}", candidates.join(" | "), args_display)
        },
        success: false,
        exit_code: None,
        duration_ms: 0,
        related_interface,
        stdout_excerpt: String::new(),
        stderr_excerpt: String::new(),
        error_kind: "missing_command",
        error: format!("no command found in [{}]", candidates.join(", ")),
    }
}

fn extract_route_interface(output: &str) -> String {
    if let Some(interface) = output.lines().find_map(|line| {
        line.split_once(':')
            .and_then(|(key, value)| (key.trim() == "interface").then_some(value.trim()))
    }) {
        return interface.to_owned();
    }

    let tokens = output.split_whitespace().map(str::trim).collect::<Vec<_>>();
    for window in tokens.windows(2) {
        if let ["dev", interface] = window {
            return (*interface).to_owned();
        }
    }

    String::new()
}

fn duration_to_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
struct CommandSpec {
    candidates: Vec<&'static str>,
    args: Vec<String>,
}

fn command_spec_for_curl(models_url: &str) -> CommandSpec {
    CommandSpec {
        candidates: vec!["/usr/bin/curl", "/bin/curl", "curl"],
        args: vec![
            "--silent".to_owned(),
            "--show-error".to_owned(),
            "--output".to_owned(),
            "/dev/null".to_owned(),
            "--write-out".to_owned(),
            "http_code=%{http_code} remote_ip=%{remote_ip} local_ip=%{local_ip}\n".to_owned(),
            "--max-time".to_owned(),
            COMMAND_PROBE_TIMEOUT_SECS.to_owned(),
            models_url.to_owned(),
        ],
    }
}

fn command_spec_for_nc(host: &str, port: u16) -> CommandSpec {
    CommandSpec {
        candidates: vec!["/usr/bin/nc", "/bin/nc", "nc"],
        args: vec![
            "-vz".to_owned(),
            "-w".to_owned(),
            COMMAND_PROBE_TIMEOUT_SECS.to_owned(),
            host.to_owned(),
            port.to_string(),
        ],
    }
}

fn command_spec_for_route(host: &str) -> CommandSpec {
    if cfg!(target_os = "macos")
        || cfg!(target_os = "ios")
        || cfg!(target_os = "freebsd")
        || cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
    {
        CommandSpec {
            candidates: vec!["/sbin/route", "route"],
            args: vec!["-n".to_owned(), "get".to_owned(), host.to_owned()],
        }
    } else {
        CommandSpec {
            candidates: vec!["/usr/sbin/ip", "/sbin/ip", "ip"],
            args: vec!["route".to_owned(), "get".to_owned(), host.to_owned()],
        }
    }
}

fn command_spec_for_neighbor(host: &str) -> CommandSpec {
    if cfg!(target_os = "macos")
        || cfg!(target_os = "ios")
        || cfg!(target_os = "freebsd")
        || cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
    {
        CommandSpec {
            candidates: vec!["/usr/sbin/arp", "/sbin/arp", "arp"],
            args: vec!["-n".to_owned(), host.to_owned()],
        }
    } else {
        CommandSpec {
            candidates: vec!["/usr/sbin/ip", "/sbin/ip", "ip"],
            args: vec!["neigh".to_owned(), "show".to_owned(), host.to_owned()],
        }
    }
}

fn command_spec_for_interface(interface: &str) -> CommandSpec {
    if cfg!(target_os = "macos")
        || cfg!(target_os = "ios")
        || cfg!(target_os = "freebsd")
        || cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
    {
        CommandSpec {
            candidates: vec!["/sbin/ifconfig", "ifconfig"],
            args: vec![interface.to_owned()],
        }
    } else {
        CommandSpec {
            candidates: vec!["/usr/sbin/ip", "/sbin/ip", "ip"],
            args: vec![
                "addr".to_owned(),
                "show".to_owned(),
                "dev".to_owned(),
                interface.to_owned(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn only_openai_compatible_transport_failures_probe_models() {
        let spec = ProviderSpec {
            kind: ProviderKind::OpenAiCompatible,
            model: "demo-model".into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            extra_headers: BTreeMap::new(),
            refresh_token: None,
        };
        let transport = TransportErrorTrace {
            variant: "web_model_call",
            kind: "reqwest",
            http_status: None,
            reqwest_is_connect: true,
            reqwest_is_timeout: false,
            reqwest_is_request: true,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: "http://127.0.0.1:11434/v1/chat/completions".into(),
            source_chain: "connect error".into(),
            body_excerpt: String::new(),
        };

        assert!(should_probe_models_endpoint(
            ProviderKind::OpenAiCompatible,
            &spec,
            &transport,
        ));
        assert!(!should_probe_models_endpoint(
            ProviderKind::OpenAi,
            &spec,
            &transport,
        ));
    }

    #[test]
    fn socket_probe_uses_base_url_host_and_port() {
        let trace = probe_socket_target("http://127.0.0.1:9/v1/");
        assert_eq!(trace.target_host, "127.0.0.1");
        assert_eq!(trace.target_port, 9);
    }

    #[test]
    fn extract_route_interface_parses_macos_route_output() {
        let output = "route to: 10.0.0.7\ninterface: en0\nflags: <UP,HOST,DONE>\n";
        assert_eq!(extract_route_interface(output), "en0");
    }

    #[test]
    fn extract_route_interface_parses_linux_ip_route_output() {
        let output = "10.0.0.7 dev eth0 src 10.0.0.5 uid 1000\n    cache\n";
        assert_eq!(extract_route_interface(output), "eth0");
    }
}
