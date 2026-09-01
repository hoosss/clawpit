//! clawpit-mcp：clawpit 的 MCP 服务器（stdio / ndjson JSON-RPC，手写协议层）。
//!
//! agent CLI（claude/codex/gemini…）把它配成 MCP server 后，agent 即获得三个工具：
//!   clawpit_list  看车间成员
//!   clawpit_send  给别的 agent（或 human）发消息
//!   clawpit_inbox 取自己的待收消息（取走即清）
//! 身份：用父进程 pid 匹配 hub 注册表——谁拉起我，我就是谁。
//! 环境变量 CLAWPIT_HUB 覆盖 hub 地址（默认 127.0.0.1:7664）。

use std::{
    io::{BufRead, Read, Write},
    net::TcpStream,
    os::unix::process::parent_id,
};

use serde_json::{json, Value};

fn main() {
    if let Err(e) = run() {
        eprintln!("clawpit-mcp: {e}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let hub = std::env::var("CLAWPIT_HUB").unwrap_or_else(|_| "127.0.0.1:7664".into());
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // 通知（无 id）不回应
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => Some(initialize_result(&req)),
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({ "tools": tools_desc() })),
            "tools/call" => Some(call_tool(&hub, &req)),
            _ => None,
        };
        let resp = match result {
            Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("unknown method: {method}") }
            }),
        };
        println!("{resp}");
        std::io::stdout().flush()?;
    }
    Ok(())
}

fn initialize_result(req: &Value) -> Value {
    json!({
        "protocolVersion": req.pointer("/params/protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("2025-06-18"),
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "clawpit", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "你在像素车间里。clawpit_list 查看车间成员；clawpit_send(to,text) 给同事或 human 发消息；clawpit_inbox 取你的待收消息。"
    })
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

fn call_tool(hub: &str, req: &Value) -> Value {
    let name = req
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = req
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(json!({}));
    let pid = parent_id();
    let (text, is_err) = match name {
        "clawpit_list" => match http(hub, "GET", "/agents", None) {
            Ok(body) => (format_agents(&body), false),
            Err(e) => (format!("连不上 hub（{hub}）: {e}"), true),
        },
        "clawpit_send" => {
            let to = args.get("to").and_then(Value::as_str).unwrap_or("");
            let msg = args.get("text").and_then(Value::as_str).unwrap_or("");
            let body = json!({ "from_pid": pid, "to": to, "text": msg }).to_string();
            match http(hub, "POST", "/msg", Some(&body)) {
                Ok(b) => (format!("已投递 → {to}（{b}）"), false),
                Err(e) => (format!("发送失败: {e}"), true),
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

/// 极简 HTTP/1.1 客户端（仅 127.0.0.1 用；Connection: close，读 body 到 EOF）。
fn http(host_port: &str, method: &str, path: &str, body: Option<&str>) -> anyhow::Result<String> {
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(host_port)?;
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let (_, resp_body) = resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed http response"))?;
    Ok(resp_body.to_string())
}
