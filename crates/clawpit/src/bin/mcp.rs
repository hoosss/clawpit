//! clawpit-mcp：clawpit 的 MCP 服务器（stdio / ndjson JSON-RPC，手写协议层）。
//!
//! agent CLI（claude/codex/gemini…）把它配成 MCP server 后，agent 即获得三个工具：
//!   clawpit_list  看车间成员
//!   clawpit_send  给别的 agent（或 human）发消息
//!   clawpit_inbox 取自己的待收消息（取走即清）
//! 身份：默认用父进程 pid 匹配 hub 注册表——谁拉起我，我就是谁；
//! 经 npx 等包装器拉起时父 pid 是包装器，可用 CLAWPIT_PID 显式指定 agent 进程 pid。
//! 环境变量：CLAWPIT_HUB 覆盖 hub 地址（默认 127.0.0.1:7664）；CLAWPIT_PID 覆盖身份。

use std::{
    io::{BufRead, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    os::unix::process::parent_id,
    time::Duration,
};

use serde_json::{json, Value};

/// 本实现支持的 MCP 协议版本（不识别的请求版本一律回这个）。
const SUPPORTED_PROTOCOL: &str = "2025-06-18";
const KNOWN_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

fn main() {
    if let Err(e) = run() {
        eprintln!("clawpit-mcp: {e}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let hub = std::env::var("CLAWPIT_HUB").unwrap_or_else(|_| "127.0.0.1:7664".into());
    // 身份：显式 override 优先，否则取父 pid（原始父，用于存活检测）
    let pid_override = std::env::var("CLAWPIT_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok());
    let origin_parent = if pid_override.is_none() {
        Some(parent_id())
    } else {
        None
    };

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // 父进程已死（agent 崩溃）且未显式指定身份 → 退出，避免 pid 复用后冒名顶替
        if let Some(ppid) = origin_parent {
            if !std::path::Path::new("/proc")
                .join(ppid.to_string())
                .exists()
            {
                break;
            }
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Err(_) => json!({
                "jsonrpc": "2.0", "id": Value::Null,
                "error": { "code": -32700, "message": "parse error" }
            }),
            Ok(req) => respond(&hub, req, pid_override),
        };
        // 通知（无 id）返回 Null，不产生输出帧
        if resp.is_null() {
            continue;
        }
        println!("{resp}");
        std::io::stdout().flush()?;
    }
    Ok(())
}

fn respond(hub: &str, req: Value, pid_override: Option<u32>) -> Value {
    // 通知（无 id）不回应
    let Some(id) = req.get("id").cloned() else {
        return Value::Null;
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Some(initialize_result(&req)),
        "ping" => Some(json!({})),
        "tools/list" => Some(json!({ "tools": tools_desc() })),
        "tools/call" => Some(call_tool(hub, &req, pid_override)),
        _ => None,
    };
    match result {
        Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        None => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("unknown method: {method}") }
        }),
    }
}

fn initialize_result(req: &Value) -> Value {
    json!({
        // 不认识的版本一律回自己支持的版本（MCP 规范要求），认识的才回显
        "protocolVersion": negotiate_version(req.pointer("/params/protocolVersion").and_then(Value::as_str)),
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "clawpit", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "你在像素车间里。clawpit_list 查看车间成员；clawpit_send(to,text) 给同事或 human 发消息；clawpit_inbox 取你的待收消息。"
    })
}

fn negotiate_version(requested: Option<&str>) -> &str {
    match requested {
        Some(v) if KNOWN_PROTOCOLS.contains(&v) => v,
        _ => SUPPORTED_PROTOCOL,
    }
}

fn tools_desc() -> Value {
    json!([
        {
            "name": "clawpit_list",
            "description": "列出像素车间里当前所有 agent（id/名称/状态/provider）",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "clawpit_send",
            "description": "给另一个 agent（to=agent id，如 cc-1234 / sp-1）或 human 发消息。发给 hub 宿主的活 worker 会直接注入其会话，否则进收件箱。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "接收者 agent id 或 \"human\"" },
                    "text": { "type": "string" }
                },
                "required": ["to", "text"]
            }
        },
        {
            "name": "clawpit_inbox",
            "description": "取走自己名下的待收消息（取走即清）",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn call_tool(hub: &str, req: &Value, pid_override: Option<u32>) -> Value {
    let name = req
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = req
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(json!({}));
    let pid = pid_override.unwrap_or_else(parent_id);
    let (text, is_err) = match name {
        "clawpit_list" => match http(hub, "GET", "/agents", None) {
            Ok(body) => (format_agents(&body), false),
            Err(e) => (format!("连不上 hub（{hub}）: {e}"), true),
        },
        "clawpit_send" => {
            // 参数校验：缺参/类型错/空串都要让 agent 拿到明确的 isError，而不是"成功投递到空"
            match (
                args.get("to").and_then(Value::as_str),
                args.get("text").and_then(Value::as_str),
            ) {
                (Some(to), Some(msg)) if !to.trim().is_empty() && !msg.trim().is_empty() => {
                    let body = json!({ "from_pid": pid, "to": to, "text": msg }).to_string();
                    match http(hub, "POST", "/msg", Some(&body)) {
                        Ok(b) => (format!("已投递 → {to}（{b}）"), false),
                        Err(e) => (format!("发送失败: {e}"), true),
                    }
                }
                _ => (
                    "参数错误：to 与 text 必须是非空字符串（to=agent id 或 \"human\"）".to_string(),
                    true,
                ),
            }
        }
        "clawpit_inbox" => match http(hub, "GET", &format!("/inbox?pid={pid}"), None) {
            Ok(body) => (format_inbox(&body), false),
            Err(e) => (format!("连不上 hub（{hub}）: {e}"), true),
        },
        _ => (format!("unknown tool: {name}"), true),
    };
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_err })
}

fn format_agents(body: &str) -> String {
    let Ok(Value::Array(agents)) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if agents.is_empty() {
        return "(车间空无一人)".into();
    }
    agents
        .iter()
        .map(|a| {
            format!(
                "{}  {:?}  {}  {}",
                a.get("id").and_then(Value::as_str).unwrap_or("?"),
                a.get("provider").and_then(Value::as_str).unwrap_or("?"),
                a.get("name").and_then(Value::as_str).unwrap_or("?"),
                a.get("state").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_inbox(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let msgs = v.get("messages").and_then(Value::as_array);
    let Some(msgs) = msgs else {
        return "(空)".into();
    };
    if msgs.is_empty() {
        return "(空)".into();
    }
    msgs.iter()
        .map(|m| {
            format!(
                "[{}] {}",
                m.get("from_name").and_then(Value::as_str).unwrap_or("?"),
                m.get("text").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析 HTTP 响应：2xx 返回 body；否则 Err("HTTP <status>: <body>")（hub 的错误文案在 body 里）。
fn parse_http_response(resp: &str) -> Result<String, String> {
    let (head, resp_body) = resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed http response".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "malformed status line".to_string())?;
    if (200..300).contains(&status) {
        Ok(resp_body.to_string())
    } else {
        Err(format!("HTTP {status}: {resp_body}"))
    }
}

/// 极简 HTTP/1.1 客户端（仅本机直连；Connection: close，读 body 到 EOF）。
/// 带 connect/read 超时——hub 卡死时不能把整个 MCP 进程拖死。
fn http(host_port: &str, method: &str, path: &str, body: Option<&str>) -> anyhow::Result<String> {
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let addr = host_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid hub address"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    parse_http_response(&resp).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_status_handling() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(parse_http_response(ok).unwrap(), "[]");
        let bad = "HTTP/1.1 400 Bad Request\r\nContent-Length: 9\r\n\r\n收件人不存在: x";
        assert_eq!(
            parse_http_response(bad).unwrap_err(),
            "HTTP 400: 收件人不存在: x"
        );
        assert!(parse_http_response("garbage").is_err());
    }

    #[test]
    fn negotiate_known_vs_unknown() {
        assert_eq!(negotiate_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_version(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate_version(Some("1999-99-99")), SUPPORTED_PROTOCOL);
        assert_eq!(negotiate_version(None), SUPPORTED_PROTOCOL);
    }
}
