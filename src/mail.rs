//! Outbound email: log-mode by default, real SMTP when configured.
//!
//! Solo-admin phase: nothing here is wired to a provider until the owner
//! sets `SMTP_HOST` (+ `SMTP_PORT`, `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM`).
//! Without it every send is a server-log line (recipient + subject at
//! info, body at debug) and the caller still gets `Ok` — verification
//! and reset flows stay testable end to end with zero credentials.
//!
//! Subjects stay ASCII-only on purpose: the minimal client below does not
//! do RFC-2047 encoding. Bodies are UTF-8 `text/plain`.

use base64::Engine;
use tokio::io::AsyncBufReadExt;

/// SMTP settings, all from the environment. Nothing secret is logged.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

impl SmtpConfig {
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("SMTP_HOST").ok().filter(|h| !h.trim().is_empty())?;
        Some(Self {
            host,
            port: std::env::var("SMTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(587),
            username: std::env::var("SMTP_USER").unwrap_or_default(),
            password: std::env::var("SMTP_PASS").unwrap_or_default(),
            from: std::env::var("SMTP_FROM").unwrap_or_else(|_| "warden@localhost".to_string()),
        })
    }
}

/// Transport picked once at call time so tests can flip env per case.
#[derive(Debug, Clone)]
pub enum Mailer {
    Log,
    Smtp(SmtpConfig),
}

impl Mailer {
    pub fn from_env() -> Self {
        match SmtpConfig::from_env() {
            Some(cfg) => Self::Smtp(cfg),
            None => Self::Log,
        }
    }

    /// Send a UTF-8 plain-text message. Log-mode always succeeds.
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        match self {
            Self::Log => {
                tracing::info!(to = to, subject = subject, "mail (log-mode): not sent");
                tracing::debug!(to = to, body = body, "mail (log-mode) body");
                Ok(())
            }
            Self::Smtp(cfg) => smtp_send(cfg, to, subject, body).await,
        }
    }
}

const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One SMTP command: write the line, read the (possibly multi-line)
/// reply, return its 3-digit code.
async fn cmd<S>(io: &mut tokio::io::BufStream<S>, line: &str) -> Result<u16, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(IO_TIMEOUT, io.write_all(format!("{line}\r\n").as_bytes()))
        .await
        .map_err(|_| "smtp write timed out".to_string())?
        .map_err(|e| format!("smtp write failed: {e}"))?;
    tokio::time::timeout(IO_TIMEOUT, io.flush())
        .await
        .map_err(|_| "smtp flush timed out".to_string())?
        .map_err(|e| format!("smtp flush failed: {e}"))?;
    read_reply(io).await
}

async fn read_reply<S>(io: &mut tokio::io::BufStream<S>) -> Result<u16, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        tokio::time::timeout(IO_TIMEOUT, io.read_line(&mut line))
            .await
            .map_err(|_| "smtp read timed out".to_string())?
            .map_err(|e| format!("smtp read failed: {e}"))?;
        if line.len() >= 4 && line.as_bytes()[3] == b' ' {
            break;
        }
        if line.len() > 8192 {
            return Err("smtp reply too long".to_string());
        }
    }
    line[..3].parse::<u16>().map_err(|_| format!("bad smtp reply: {line:?}"))
}

fn expect(code: u16, want: &[u16], what: &str) -> Result<(), String> {
    if want.contains(&code) {
        Ok(())
    } else {
        Err(format!("smtp {what}: got {code}"))
    }
}

/// Minimal ESMTP client: greeting, EHLO, STARTTLS on 587-style ports
/// (direct TLS on 465), AUTH LOGIN when a username is set, one message.
async fn smtp_send(cfg: &SmtpConfig, to: &str, subject: &str, body: &str) -> Result<(), String> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let tcp = tokio::time::timeout(IO_TIMEOUT, tokio::net::TcpStream::connect(&addr))
        .await
        .map_err(|_| format!("smtp connect to {addr} timed out"))?
        .map_err(|e| format!("smtp connect to {addr} failed: {e}"))?;

    if cfg.port == 465 {
        let tls = tls_upgrade(cfg, tcp).await?;
        let mut io = tokio::io::BufStream::new(tls);
        read_reply(&mut io).await.and_then(|c| expect(c, &[220], "greeting"))?;
        dialogue(&mut io, cfg, to, subject, body).await
    } else {
        let mut io = tokio::io::BufStream::new(tcp);
        read_reply(&mut io).await.and_then(|c| expect(c, &[220], "greeting"))?;
        expect(cmd(&mut io, "EHLO warden-trade").await?, &[250], "ehlo")?;
        // STARTTLS when the server offers it (587 submission); plain
        // otherwise (local relays, tests). A 5xx reply just means "no".
        match cmd(&mut io, "STARTTLS").await {
            Ok(220) => {
                let tls = tls_upgrade(cfg, io.into_inner()).await?;
                let mut tio = tokio::io::BufStream::new(tls);
                dialogue(&mut tio, cfg, to, subject, body).await
            }
            Ok(_) | Err(_) => dialogue(&mut io, cfg, to, subject, body).await,
        }
    }
}

/// Upgrade a connected TCP stream to TLS with native roots.
async fn tls_upgrade(
    cfg: &SmtpConfig,
    tcp: tokio::net::TcpStream,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, String> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        tracing::warn!("tls root load warning: {e:?}");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    if added == 0 && ignored > 0 {
        return Err("tls: no usable root certificates".to_string());
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server = rustls::pki_types::ServerName::try_from(cfg.host.clone())
        .map_err(|_| "smtp host is not a valid DNS name".to_string())?;
    tokio::time::timeout(IO_TIMEOUT, connector.connect(server, tcp))
        .await
        .map_err(|_| "smtp tls handshake timed out".to_string())?
        .map_err(|e| format!("smtp tls handshake failed: {e}"))
}

/// The post-greeting dialogue, generic over plain and TLS streams.
async fn dialogue<S>(
    io: &mut tokio::io::BufStream<S>,
    cfg: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    expect(cmd(io, "EHLO warden-trade").await?, &[250], "ehlo")?;
    if !cfg.username.is_empty() {
        expect(cmd(io, "AUTH LOGIN").await?, &[334], "auth")?;
        let b64 = base64::engine::general_purpose::STANDARD;
        expect(cmd(io, &b64.encode(&cfg.username)).await?, &[334], "auth user")?;
        expect(cmd(io, &b64.encode(&cfg.password)).await?, &[235], "auth pass")?;
    }
    expect(cmd(io, &format!("MAIL FROM:<{}>", cfg.from)).await?, &[250], "mail from")?;
    expect(cmd(io, &format!("RCPT TO:<{to}>")).await?, &[250, 251], "rcpt to")?;
    expect(cmd(io, "DATA").await?, &[354], "data")?;
    // Dot-stuffing per RFC 5321 §4.5.2.
    let stuffed = body.replace("\r\n.", "\r\n..").replace("\n.", "\n..");
    let data = format!(
        "From: {}\r\nTo: {to}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{stuffed}\r\n.",
        cfg.from
    );
    expect(cmd(io, &data).await?, &[250], "message")?;
    let _ = cmd(io, "QUIT").await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_mode_never_touches_network() {
        let mailer = Mailer::Log;
        mailer.send("nobody@example.com", "subject", "body").await.expect("log-mode sends Ok");
    }

    #[test]
    fn smtp_config_absent_without_env() {
        let prev = std::env::var("SMTP_HOST").ok();
        std::env::remove_var("SMTP_HOST");
        assert!(SmtpConfig::from_env().is_none(), "no SMTP_HOST means log-mode");
        assert!(matches!(Mailer::from_env(), Mailer::Log));
        if let Some(v) = prev {
            std::env::set_var("SMTP_HOST", v);
        }
    }

    /// Full dialogue against a scripted fake: greeting, EHLO without
    /// STARTTLS (local-relay shape), AUTH LOGIN, one message, QUIT.
    /// Proves the protocol machine without any network or credentials.
    #[tokio::test]
    async fn smtp_dialogue_against_fake_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind fake smtp");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (sock, _) = listener.accept().await.expect("accept");
            let (r, mut w) = sock.into_split();
            let mut r = tokio::io::BufReader::new(r);
            let mut got_auth = false;
            let mut got_msg = false;
            w.write_all(b"220 fake ESMTP\r\n").await.expect("greet");
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.expect("read") == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                if upper.starts_with("EHLO") {
                    w.write_all(b"250-fake\r\n250 AUTH LOGIN\r\n").await.expect("ehlo");
                } else if upper.starts_with("AUTH LOGIN") {
                    w.write_all(b"334 VXNlcm5hbWU6\r\n").await.expect("u");
                    line.clear();
                    r.read_line(&mut line).await.expect("read");
                    w.write_all(b"334 UGFzc3dvcmQ6\r\n").await.expect("p");
                    line.clear();
                    r.read_line(&mut line).await.expect("read");
                    w.write_all(b"235 ok\r\n").await.expect("ok");
                    got_auth = true;
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    w.write_all(b"250 ok\r\n").await.expect("ok");
                } else if upper.starts_with("DATA") {
                    w.write_all(b"354 end with .\r\n").await.expect("data");
                    // Slurp until the lone dot.
                    loop {
                        line.clear();
                        r.read_line(&mut line).await.expect("read");
                        if line == ".\r\n" || line == ".\n" {
                            got_msg = true;
                            break;
                        }
                    }
                    w.write_all(b"250 queued\r\n").await.expect("queued");
                } else if upper.starts_with("QUIT") {
                    w.write_all(b"221 bye\r\n").await.expect("bye");
                    break;
                } else if line.starts_with("dXNlcg==") || line.starts_with("cGFzcw==") {
                    // Already consumed inline above; ignore strays.
                }
            }
            (got_auth, got_msg)
        });

        // Drive only the dialogue half (no greeting/STARTTLS): connect,
        // consume the 220, then run the shared dialogue function.
        let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.expect("connect");
        let mut io = tokio::io::BufStream::new(tcp);
        read_reply(&mut io).await.expect("greeting");
        let cfg = SmtpConfig {
            host: "127.0.0.1".to_string(),
            port,
            username: "user".to_string(),
            password: "pass".to_string(),
            from: "warden@localhost".to_string(),
        };
        dialogue(&mut io, &cfg, "to@example.com", "Hello", "line one\n.line two").await.expect("dialogue");
        drop(io);
        let (auth, msg) = server.await.expect("server");
        assert!(auth, "fake must see AUTH LOGIN succeed");
        assert!(msg, "fake must receive the message body");
    }
}
