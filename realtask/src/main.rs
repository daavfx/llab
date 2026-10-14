// realtask: REALTASK grader harness in Rust (M4 - replaces the legacy python harness).
//
// Consumes the standing 5-task suite under
//   04_task_suite_harness\tasks\task_*.md
// Each file: ## PROMPT (4-backtick fence), ## EXPECTED ANSWER, ## RUBRIC.
// Runs each task against a live llama-server (via a port), grades with fixed
// per-task graders, writes scores.json. Regression contract: qwen2b must print 6/10.
//
// std-only, no crates. Talks HTTP directly to the server's /v1/chat/completions.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ---------- tiny HTTP POST to llama-server ----------
fn http_chat_opts(port: u16, prompt: &str, max_tokens: u32, seed: u32, no_think: bool) -> Result<(String, String), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let esc = |s: &str| {
        let mut o = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                c => o.push(c),
            }
        }
        o
    };
    let kwargs = if no_think { ",\"chat_template_kwargs\":{\"enable_thinking\":false}" } else { "" };
    let body = format!(
        "{{\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}],\"max_tokens\":{},\"temperature\":0.0,\"seed\":{}{}}}",
        esc(prompt), max_tokens, seed, kwargs
    );
    let addr = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {}: {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_secs(900))).ok();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => { resp.push_str(&String::from_utf8_lossy(&buf[..n])); if resp.len() > 2_000_000 { break; } }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
    let json = resp.find("\r\n\r\n").map(|i| &resp[i + 4..]).unwrap_or(&resp);
    let mut content = extract_str(json, "content").unwrap_or_default();
    let reasoning = extract_str(json, "reasoning_content").unwrap_or_default();
    // thinking-model fallback: empty content at finish=length is the trace eating
    // the budget; fall back to the reasoning text so the grader sees something.
    let split = content.trim().is_empty() && !reasoning.trim().is_empty();
    if split { content = reasoning.clone(); }
    let finish = extract_str(json, "finish_reason").unwrap_or_default();
    let _ = split;
    Ok((content, finish))
}

// pull a "key": "value" string from a flat json blob (llama responses are flat enough here)
fn extract_str(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let i = json.find(&pat)? + pat.len();
    let mut raw = String::new();
    let mut esc = false;
    for c in json[i..].chars() {
        if esc {
            // unescape the sequences that matter for line-based graders
            match c {
                'n' => raw.push('\n'),
                't' => raw.push('\t'),
                'r' => raw.push('\r'),
                '"' => raw.push('"'),
                '\\' => raw.push('\\'),
                '/' => raw.push('/'),
                'u' => {}
                other => { raw.push('\\'); raw.push(other); }
            }
            esc = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '"' => return Some(raw),
            _ => raw.push(c),
        }
    }
    None
}

// ---------- task file parsing ----------
struct Task { id: String, prompt: String }

fn parse_task(path: &Path) -> Option<Task> {
    let txt = fs::read_to_string(path).ok()?;
    let id = txt.lines().find_map(|l| l.strip_prefix("- id: ")).unwrap_or("task").trim().to_string();
    // prompt = between "## PROMPT" and the closing 4-backtick fence
    let p0 = txt.find("## PROMPT")? + "## PROMPT".len();
    let rest = &txt[p0..];
    let fence = rest.find("````")?;
    let after = &rest[fence + 4..];
    let close = after.find("````")?;
    let prompt = after[..close].trim().to_string();
    Some(Task { id, prompt })
}

// ---------- graders ----------

// find the first balanced {...} JSON object in the text
fn first_json_object(s: &str) -> Option<String> {
    let bytes: Vec<char> = s.chars().collect();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc { esc = false; continue; }
            if c == '\\' { esc = true; continue; }
            if c == '"' { in_str = false; }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 { return Some(bytes[start..=i].iter().collect()); }
            }
            _ => {}
        }
    }
    None
}

// naive field getter for flat json objects (string or number)
fn json_field(obj: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let kp = obj.find(&pat)? + pat.len();
    let after = &obj[kp..];
    let ci = after.find(':')? + 1;
    let val = after[ci..].trim_start();
    if let Some(stripped) = val.strip_prefix('"') {
        let mut e = false;
        for (i, c) in stripped.char_indices() {
            if e { e = false; continue; }
            match c { '\\' => e = true, '"' => return Some(stripped[..i].to_string()), _ => {} }
        }
        None
    } else {
        let end = val.find([',', '}', ' ']).unwrap_or(val.len());
        Some(val[..end].trim().to_string())
    }
}

fn grade_task_1(fix: &str) -> (u32, String) { // code fix: expect the corrected comparison
    // accept if the response contains a fixed comparison and not the buggy one
    let has_fix = fix.contains("i > 10") || fix.contains("i >= 10") || fix.to_lowercase().contains("greater than or equal") || fix.contains("> 10");
    let has_bug = fix.contains("i < 10");
    if has_fix && !has_bug { (2, "exact fix".into()) } else if has_fix { (1, "fix present but bug remnant".into()) } else { (0, "no fix".into()) }
}
fn grade_task_2(txt: &str) -> (u32, String) { // reasoning: ANSWER: 8:40
    if let Some(pos) = txt.rfind("8:40") { let _ = pos; (2, "exact ANSWER 8:40".into()) } else if txt.contains("8:4") || txt.contains("8: 40") { (1, "close but not exact".into()) } else { (0, "no answer line".into()) }
}
fn grade_task_3(txt: &str) -> (u32, String) {
    let clean = txt.trim().starts_with('{');
    let obj = match first_json_object(txt) { Some(o) => o, None => return (0, "no parseable json".into()) };
    let name = json_field(&obj, "name").unwrap_or_default();
    let date = json_field(&obj, "date").unwrap_or_default();
    let amount = json_field(&obj, "amount").unwrap_or_default();
    let status = json_field(&obj, "status").unwrap_or_default().to_lowercase();
    let amount_ok = matches!(amount.trim(), "1250" | "1250.0" | "1250.00");
    let all_ok = name == "Elena Vasquez" && date == "2026-09-28" && amount_ok && status == "refunded";
    let keys_ok = ["name", "date", "amount", "status"].iter().all(|k| obj.contains(&format!("\"{}\"", k)));
    if all_ok && keys_ok && clean { (2, "clean exact object".into()) }
    else if keys_ok || all_ok || name == "Elena Vasquez" || status == "refunded" { (1, "parses imperfect".into()) }
    else { (0, "wrong fields".into()) }
}
fn grade_task_4(txt: &str) -> (u32, String) { // summarize: exactly 3 bullets, no fabrication
    let bullets = txt.lines().filter(|l| l.trim_start().starts_with('-') || l.trim_start().starts_with('*') || l.trim_start().starts_with('\u{2022}')).count();
    if bullets == 3 { (2, "exactly 3 bullets".into()) } else if bullets > 0 { (1, format!("{} bullets", bullets)) } else { (0, "no bullets".into()) }
}
fn grade_task_5(txt: &str) -> (u32, String) { // tool plan: 4 numbered steps + PLAN_END
    let steps = txt.lines().filter(|l| { let t = l.trim_start(); t.starts_with("1.") || t.starts_with("2.") || t.starts_with("3.") || t.starts_with("4.") }).count();
    let has_end = txt.contains("PLAN_END");
    if steps == 4 && has_end { (2, "4 steps + PLAN_END".into()) } else if steps >= 3 { (1, format!("{} steps end={}", steps, has_end)) } else { (0, "insufficient plan".into()) }
}

fn grade(id: &str, txt: &str) -> (u32, String) {
    match id {
        "task_1_code_fix" => grade_task_1(txt),
        "task_2_reasoning" => grade_task_2(txt),
        "task_3_json_extract" => grade_task_3(txt),
        "task_4_summarize" => grade_task_4(txt),
        "task_5_tool_plan" => grade_task_5(txt),
        _ => (0, "unknown task".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // usage: realtask --port 8092 --tasks <dir> --out <scores.json> [--label name]
    let mut port: u16 = 8092;
    let mut tasks_dir = String::new();
    let mut out = String::new();
    let mut label = String::from("run");
    let mut no_think = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => { port = args[i + 1].parse().unwrap_or(port); i += 2; }
            "--tasks" => { tasks_dir = args[i + 1].clone(); i += 2; }
            "--out" => { out = args[i + 1].clone(); i += 2; }
            "--label" => { label = args[i + 1].clone(); i += 2; }
            "--no-think" => { no_think = true; i += 1; }
            _ => i += 1,
        }
    }
    if tasks_dir.is_empty() { eprintln!("need --tasks <dir>"); std::process::exit(2); }

    // preflight: wait for the target server to be healthy (a flipped model can
    // take tens of seconds to finish loading; without this every task scores 0)
    {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut ok = false;
        while std::time::Instant::now() < deadline {
            if let Ok(mut s) = TcpStream::connect(format!("127.0.0.1:{}", port)) {
                let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                if s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_ok() {
                    let mut buf = [0u8; 256];
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n > 0 && String::from_utf8_lossy(&buf[..n]).contains("200") { ok = true; break; }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if !ok { eprintln!("target :{} never became healthy - aborting (no fake zeros)", port); std::process::exit(3); }
        println!("preflight ok: :{} healthy", port);
    }

    let mut files: Vec<PathBuf> = fs::read_dir(&tasks_dir).ok()
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "md").unwrap_or(false)).collect())
        .unwrap_or_default();
    files.sort();

    let mut total = 0u32;
    let mut rows = Vec::new();
    let mut transcripts = PathBuf::from(&out);
    transcripts.pop();
    let tdir = transcripts.join(format!("transcript-{}", label));
    let _ = fs::create_dir_all(&tdir);

    for f in files.iter() {
        let Some(task) = parse_task(f) else { continue };
        if !task.id.starts_with("task_") { continue; }
        let budget = if task.id.contains("2") || task.id.contains("4") { 512u32 } else { 256 };
        let t0 = Instant::now();
        let (reply, finish) = match http_chat_opts(port, &task.prompt, budget, 42, no_think) {
            Ok((r, f)) => (r, f),
            Err(e) => { eprintln!("[{}] chat failed: {}", task.id, e); (String::new(), "error".into()) }
        };
        let wall = t0.elapsed().as_secs_f64();
        let (score, note) = grade(&task.id, &reply);
        total += score;
        println!("[{}] score {}/2 finish={} wall={:.1}s - {}", task.id, score, finish, wall, note);
        let _ = fs::write(tdir.join(format!("{}.txt", task.id)), &reply);
        rows.push(format!(
            "{{\"id\":\"{}\",\"score\":{},\"max\":2,\"finish\":\"{}\",\"wall_s\":{:.1},\"grader\":\"{}\"}}",
            task.id, score, finish, wall, note
        ));
    }

    let outp = if out.is_empty() { PathBuf::from("scores.json") } else { PathBuf::from(&out) };
    let scores = format!(
        "{{\"label\":\"{}\",\"port\":{},\"total\":{},\"max\":10,\"tasks\":[{}]}}",
        label, port, total, rows.join(",")
    );
    let _ = fs::write(&outp, &scores);
    println!("TOTAL {}/10 for {} -> {}", total, label, outp.display());
}