// llab: local_llm lab control app - prototype-v0 real skeleton.
// std-only (zero crates): mini HTTP server + kernel32 FFI for real CPU/RAM metrics.
// Data flow: this binary reads registry.json (source of truth), computes real
// machine metrics, merges doctor sweep verdicts, and serves one live dashboard.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------- kernel32 FFI (real metrics) ----------
#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page: u64,
    avail_page: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_ext: u64,
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
}

fn ram() -> (u64, u64, u32) {
    unsafe {
        let mut m: MemoryStatusEx = std::mem::zeroed();
        m.length = std::mem::size_of::<MemoryStatusEx>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 { (m.total_phys, m.avail_phys, m.memory_load) } else { (0, 0, 0) }
    }
}
// PDH performance counters: same source Task Manager uses ("Processor(_Total)\% Processor Time").
#[repr(C)]
struct PdhFmtValue { status: u32, double_value: f64, _pad: u64 }
#[link(name = "pdh")]
extern "system" {
    fn PdhOpenQueryA(datasource: *const u8, userdata: usize, query: *mut isize) -> i32;
    fn PdhAddEnglishCounterA(query: isize, path: *const u8, userdata: usize, counter: *mut isize) -> i32;
    fn PdhCollectQueryData(query: isize) -> i32;
    fn PdhGetFormattedCounterValue(counter: isize, fmt_type: u32, cookie: *mut u32, value: *mut PdhFmtValue) -> i32;
}
const PDH_FMT_DOUBLE: u32 = 0x200;

fn cpu_pct_pdh() -> f64 {
    unsafe {
        let mut q: isize = 0;
        if PdhOpenQueryA(std::ptr::null(), 0, &mut q) != 0 { return 0.0; }
        let mut c: isize = 0;
        let ok = PdhAddEnglishCounterA(q, b"\\Processor(_Total)\\% Processor Time\0".as_ptr(), 0, &mut c) == 0;
        let mut pct = 0.0f64;
        if ok {
            let _ = PdhCollectQueryData(q); // first collect seeds; second gives data
            std::thread::sleep(Duration::from_millis(233)); // PDH minimum stable sample gap
            if PdhCollectQueryData(q) == 0 {
                let mut v: PdhFmtValue = std::mem::zeroed();
                if PdhGetFormattedCounterValue(c, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut v) == 0 {
                    pct = v.double_value;
                }
            }
        }
        pct
    }
}

// ---------- tiny registry reader (naive but robust for our own schema) ----------
#[derive(Clone)]
struct ModelRec {
    name: String,
    gguf_rel: String,
    engine: String,
port: u32,
    size_gb: f64,
    think: String,
    status: String,
    exists: bool,
}

fn find_str(txt: &str, from: usize, key: &str) -> Option<(usize, String)> {
    let pat = format!("\"{}\":", key);
    let i = txt[from..].find(&pat)? + from;
    let mut j = i + pat.len();
    while j < txt.len() && (txt.as_bytes()[j] as char).is_whitespace() { j += 1; }
    if txt.as_bytes().get(j) == Some(&b'"') {
        let s = j + 1;
        let e = txt[s..].find('"')? + s;
        Some((e, txt[s..e].to_string()))
    } else {
        let e = txt[j..].find(|c: char| c == ',' || c == '}')? + j;
        Some((e, txt[j..e].trim().to_string()))
    }
}

fn models_from_registry(root: &PathBuf) -> Vec<ModelRec> {
    let regp = root.join("GGUF").join("llm-collection").join("tools").join("gguf-play").join("registry.json");
    let txt = match fs::read_to_string(&regp) { Ok(t) => t, Err(_) => return vec![] };
    let coll = root.join("GGUF").join("llm-collection");
    let mi = match txt.find("\"models\"") { Some(x) => x, None => return vec![] };
let ei = txt[mi..].find("\"engines\"").map(|x| x + mi).unwrap_or(txt.len());
    // skip the section header itself: scanning from the "models" token would parse the
    // header as an entry named "models" and swallow the first real model (retain() hides the
    // bogus row, so the loss was silent). Start after the header's opening brace.
    let body_start = txt[mi..ei].find('{').map(|x| x + mi + 1).unwrap_or(mi);
    let body = &txt[body_start..ei];
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        // find next "<name>": {  where name is a quoted key followed by colon+brace
        let q1 = match body[pos..].find('"') { Some(x) => x + pos, None => break };
        let q2 = match body[q1 + 1..].find('"') { Some(x) => x + q1 + 1, None => break };
        let name = &body[q1 + 1..q2];
        let mut j = q2 + 1;
        while j < body.len() && (body.as_bytes()[j] as char).is_whitespace() { j += 1; }
        if j >= body.len() || body.as_bytes()[j] != b':' { pos = q2 + 1; continue; }
        j += 1;
        while j < body.len() && (body.as_bytes()[j] as char).is_whitespace() { j += 1; }
        if j >= body.len() || body.as_bytes()[j] != b'{' { pos = q2 + 1; continue; }
        // model object: from j.. to matching "}" (first-level; our registry objects are flat)
        let end = body[j..].find('}').map(|x| x + j).unwrap_or(body.len());
        let obj = &body[j..end];
        let gguf = find_str(obj, 0, "path").or_else(|| find_str(obj, 0, "gguf")).map(|(_, v)| v).unwrap_or_default();
        let if_gguf = gguf.ends_with(".gguf");
        let engine = find_str(obj, 0, "engine").map(|(_, v)| v).unwrap_or_else(|| "-".into());
        let port = find_str(obj, 0, "port").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(0);
        let think = find_str(obj, 0, "thinking").or_else(|| find_str(obj, 0, "think")).map(|(_, v)| v).unwrap_or_default();
        let status = find_str(obj, 0, "status").map(|(_, v)| v).unwrap_or_default();
        let full = coll.join(gguf.replace('/', "\\"));
        let (size_gb, exists) = fs::metadata(&full).map(|m| (m.len() as f64 / 1e9, true)).unwrap_or((0.0, false));
        out.push(ModelRec {
            name: name.to_string(),
            gguf_rel: if if_gguf { gguf } else { String::new() },
            engine, port, think, status, exists, size_gb,
        });
        pos = end;
    }
    out.retain(|m| m.name != "models" && (!m.gguf_rel.is_empty() || m.port > 0));
    out
}

// ---------- doctor sweep parse (real file produced by gguf-doctor) ----------
fn doctor_summary(root: &PathBuf) -> (usize, usize, usize) {
    // returns (sections, errors, warnings) from latest sweep file if present
    let dir = root.to_path_buf();
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("doctor-sweep-") && n.ends_with(".txt") {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if latest.as_ref().map(|(lt, _)| t > *lt).unwrap_or(true) { latest = Some((t, e.path())); }
            }
        }
    }
    let Some((_, p)) = latest else { return (0, 0, 0) };
    let txt = fs::read_to_string(p).unwrap_or_default();
    let sections = txt.matches("########## ").count();
    let errs = txt.matches("[ERR ]").count();
    let warns = txt.matches("[WARN]").count() + txt.matches("[WARN]").count(); // single pattern
    (sections, errs, warns / 2)
}



// ---------- M2: engine-flip process manager ----------
// llab owns llama-server lifecycles on the reserved port map. Production path
// (00-CPU-BENCH-MASTER\serve\serve-model.ps1) untouched. BelowNormal at spawn.
use std::os::windows::process::CommandExt;
use std::sync::Mutex;
use std::collections::HashMap;

const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;

fn engine_exe(root: &PathBuf, engine: &str) -> Option<PathBuf> {
    let rel = match engine {
        "stock" => r"llama.cpp-original\build_cpu_original\bin\Release",
        "prism" => r"llama.cpp-prism\build_cpu_prism\bin\Release",
        "mod" => r"llama.cpp-abelianIUT\build_cpu_mod\bin\Release",
        _ => return None,
    };
    let p = root.join(r"..\shared").join(rel).join("llama-server.exe");
    let p = p.components().collect::<PathBuf>();
    if p.exists() { Some(p) } else { None }
}

// pinned engine law from the campaign: granite->stock, bonsai->prism
fn allowed_engine(model: &str, want: &str) -> bool {
    let pinned = if model.contains("granite") { "stock" }
        else if model.contains("bonsai") { "prism" }
        else { "any" };
    pinned == "any" || pinned == want
}

static MANAGER: Mutex<Option<HashMap<u16, (u32, String)>>> = Mutex::new(None); // port -> (pid, engine)

fn mgr() -> std::sync::MutexGuard<'static, Option<HashMap<u16, (u32, String)>>> {
    let mut g = MANAGER.lock().unwrap();
    if g.is_none() { *g = Some(HashMap::new()); }
    g
}

fn pid_listening_on(port: u16) -> Option<u32> {
    let out = std::process::Command::new("netstat").args(["-ano", "-p", "tcp"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.contains("LISTENING") && line.contains(&format!(":{}", port)) {
            if let Some(pid) = line.split_whitespace().last().and_then(|t| t.parse::<u32>().ok()) {
                return Some(pid);
            }
        }
    }
    None
}

fn kill_port(port: u16) -> bool {
    if let Some(pid) = pid_listening_on(port) {
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
        true
    } else { false }
}

// POST /api/engine/flip {model, engine} -> guards, kill old on that port, spawn pinned
// POST /api/engine/stop {port}
fn engine_flip(body: &str, root: &PathBuf) -> String {
    let model = find_str(body, 0, "model").map(|(_, v)| v);
    let engine = find_str(body, 0, "engine").map(|(_, v)| v);
    let threads = find_str(body, 0, "threads").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(6);
    let ctx = find_str(body, 0, "ctx").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(2048);
    let taps = find_str(body, 0, "taps").map(|(_, v)| v == "true" || v == "1").unwrap_or(false);
    let (Some(model), Some(engine)) = (model, engine) else { return r#"{"error":"need model+engine"}"#.into(); };
    if !allowed_engine(&model, &engine) {
        return r#"{"error":"engine pin violation (granite->stock, bonsai->prism)"}"#.into();
    }
    let models = models_from_registry(root);
    let Some(m) = models.iter().find(|m| m.name == model) else { return r#"{"error":"unknown model"}"#.into(); };
    if m.gguf_rel.is_empty() { return r#"{"error":"model has no gguf in registry"}"#.into(); }
    let gguf = root.join("GGUF").join("llm-collection").join(m.gguf_rel.replace('/', "\\"));
    if !gguf.exists() { return r#"{"error":"gguf file missing on disk"}"#.into(); }
    let Some(exe) = engine_exe(root, &engine) else { return r#"{"error":"engine binary missing"}"#.into(); };
    // RAM guard: require 1.35x file size + 0.5GB slope free; when re-flipping the
    // same port, the incumbent server (same model family size) will be killed first.
    let (_, mut avail, _) = ram();
    let port = m.port as u16;
    if probe_port(port) {
        let est = (m.size_gb * 1.35 * 1e9) as u64; // approx recoverable from kill
        if est > (m.size_gb * 1e9) as u64 { avail = avail.saturating_add(est); }
    }
    let need = (m.size_gb * 1.35 + 0.5) * 1e9;
    if (avail as f64) < need {
        return format!(r#"{{"error":"ram guard: need {:.1}GB free, have {:.1}GB"}}"#, need / 1e9, avail as f64 / 1e9);
    }
    if probe_port(port) { kill_port(port); std::thread::sleep(Duration::from_millis(600)); }
    let args = format!("-m \"{}\" --host 127.0.0.1 --port {} -c {} -t {} -ngl 0",
        gguf.display(), port, ctx, threads);
    let mut cmd = std::process::Command::new(&exe);
    cmd.raw_arg(&args)
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | 0x00000008); // DETACHED_PROCESS
    if taps { cmd.env("DAAVFX_DSPARK_TAPS", "1"); }
    let child = cmd.spawn();
    match child {
        Ok(c) => {
            mgr().as_mut().unwrap().insert(port, (c.id(), engine.clone()));
            format!(r#"{{"spawned":true,"pid":{},"port":{},"engine":"{}","model":"{}","below_normal":true,"taps":{}}}"#,
                c.id(), port, jesc(&engine), jesc(&model), taps)
        }
        Err(e) => format!(r#"{{"error":"spawn failed: {}"}}"#, e),
    }
}

fn engine_state(root: &PathBuf) -> String {
    let models = models_from_registry(root);
    let g = mgr();
    let tbl = g.as_ref().unwrap();
    let mut items = vec![];
    for m in models.iter().filter(|m| m.port > 0) {
        let port = m.port;
        let alive = probe_port(port as u16);
        let managed = tbl.get(&(port as u16)).cloned();
        let state = if alive {
            match managed {
                Some((pid, eng)) => format!(r#"{{"alive":true,"managed":true,"pid":{},"engine":"{}"}}"#, pid, jesc(&eng)),
                None => r#"{"alive":true,"managed":false}"#.to_string(),
            }
        } else if managed.is_some() { r#"{"alive":false,"managed":true,"crashed_or_stopped":true}"#.into() }
        else { r#"{"alive":false,"managed":false}"#.into() };
        items.push(format!(r#"{{"model":"{}","port":{},{} }}"#, jesc(&m.name), port, state.trim_matches(|c| c == '{' || c == '}')));
    }
    format!("[{}]", items.join(","))
}

fn engine_stop(body: &str) -> String {
    let Some(port) = find_str(body, 0, "port").and_then(|(_, v)| v.parse::<u16>().ok()) else {
        return r#"{"error":"need port"}"#.into();
    };
    let ok = kill_port(port);
    mgr().as_mut().unwrap().remove(&port);
    format!(r#"{{"stopped":{}}}"#, ok)
}

// ---------- M3: speed suite (llama-bench runner -> results\speed_rows.ndjson) ----------
fn file_fingerprint(p: &PathBuf) -> u64 {
    use std::io::Seek;
    let Ok(mut f) = fs::File::open(p) else { return 0; };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut h: u64 = 0xcbf29ce484222325 ^ len;
    let mut head = [0u8; 65536];
    if let Ok(n) = f.read(&mut head) {
        for b in &head[..n] { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); }
    }
    if len > 70000 { let _ = f.seek(std::io::SeekFrom::Start(len - 65536)); let mut tail = [0u8; 65536];
        if let Ok(n) = f.read(&mut tail) { for b in &tail[..n] { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); } } }
    h
}

static BENCHES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None); // key -> status json

// ---------- DSpark SHM reader (fork observability) ----------
// The mod engine (llama.cpp-abelianIUT) publishes tap stats to
// "Local\daavfx_dspark_<pid>" when spawned with DAAVFX_DSPARK_TAPS=1.
// Wire format owned by src\daavfx_per_layer_hooks.h (daavfx_dspark_stats_t).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DsparkStats {
    version: u32,
    enabled: u32,
    seq: u64,
    captures: u64,
    bytes: u64,
    total_ns: u64,
    norms: [f32; 5],
    last_ns: f32,
    taps: [i32; 5],
    hidden: i32,
    epoch: u32,
    _pad: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
    fn MapViewOfFile(handle: isize, access: u32, off_hi: u32, off_lo: u32, bytes: usize) -> *mut core::ffi::c_void;
    fn UnmapViewOfFile(p: *const core::ffi::c_void) -> i32;
    fn CloseHandle(h: isize) -> i32;
}

fn taps_read(pid: u32) -> Option<DsparkStats> {
    let name: Vec<u16> = format!("Local\\daavfx_dspark_{}\0", pid).encode_utf16().collect();
    unsafe {
        let h = OpenFileMappingW(0x0004, 0, name.as_ptr()); // FILE_MAP_READ
        if h == 0 { return None; }
        let p = MapViewOfFile(h, 0x0004, 0, 0, 0);
        if p.is_null() { CloseHandle(h); return None; }
        let s = p as *const DsparkStats;
        let mut out = DsparkStats::default();
        let mut ok = false;
        for _ in 0..5 {
            let e1 = std::ptr::read_volatile(std::ptr::addr_of!((*s).epoch));
            if e1 % 2 == 1 { continue; } // writer mid-update
            out = std::ptr::read_volatile(s);
            let e2 = std::ptr::read_volatile(std::ptr::addr_of!((*s).epoch));
            if e1 == e2 { ok = true; break; }
        }
        UnmapViewOfFile(p);
        CloseHandle(h);
        if ok { Some(out) } else { None }
    }
}

// one netstat pass -> port -> pid map (cheaper than per-port probes)
fn listening_pids() -> HashMap<u16, u32> {
    let mut out = HashMap::new();
    if let Ok(o) = std::process::Command::new("netstat").args(["-ano", "-p", "tcp"]).output() {
        let text = String::from_utf8_lossy(&o.stdout);
        for line in text.lines() {
            if !line.contains("LISTENING") { continue; }
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 { continue; }
            if let Some(port) = cols[1].rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
                if let Some(pid) = cols[cols.len() - 1].parse::<u32>().ok() {
                    out.insert(port, pid);
                }
            }
        }
    }
    out
}

fn taps_json(root: &PathBuf) -> String {
    let models = models_from_registry(root);
    let pids = listening_pids();
    let mut items = Vec::new();
    for m in models.iter().filter(|m| m.port > 0) {
        let Some(pid) = pids.get(&(m.port as u16)).copied() else { continue; };
        let base = format!(r#"{{"model":"{}","port":{},"engine":"{}","pid":{}"#,
            jesc(&m.name), m.port, jesc(&m.engine), pid);
        match taps_read(pid) {
            Some(s) => {
                let norms: Vec<String> = s.norms.iter().map(|v| format!("{:.3}", v)).collect();
                let taps: Vec<String> = s.taps.iter().map(|v| v.to_string()).collect();
                items.push(format!(
                    r#"{base},"shm":true,"enabled":{},"seq":{},"captures":{},"bytes":{},"total_ns":{},"last_ns":{:.0},"norms":[{}],"taps":[{}],"hidden":{}}}"#,
                    s.enabled, s.seq, s.captures, s.bytes, s.total_ns, s.last_ns,
                    norms.join(","), taps.join(","), s.hidden));
            }
            None => items.push(format!(r#"{base},"shm":false}}"#)),
        }
    }
    format!("[{}]", items.join(","))
}

fn benches() -> std::sync::MutexGuard<'static, Option<HashMap<String, String>>> {
    let mut g = BENCHES.lock().unwrap();
    if g.is_none() { *g = Some(HashMap::new()); }
    g
}

fn bench_run(body: &str, root: &PathBuf) -> String {
    let model = find_str(body, 0, "model").map(|(_, v)| v);
    let engine = find_str(body, 0, "engine").map(|(_, v)| v);
    let threads = find_str(body, 0, "threads").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(6);
    let pp = find_str(body, 0, "pp").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(512);
    let tg = find_str(body, 0, "tg").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(128);
    let reps = find_str(body, 0, "reps").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(3);
    let taps = find_str(body, 0, "taps").map(|(_, v)| v == "true" || v == "1").unwrap_or(false);
    let engine_req = engine;
    let Some(model) = model else { return r#"{"error":"need model"}"#.into(); };
    let models = models_from_registry(root);
    let Some(m) = models.iter().find(|m| m.name == model) else { return r#"{"error":"unknown model"}"#.into(); };
    if m.gguf_rel.is_empty() { return r#"{"error":"no gguf path in registry"}"#.into(); }
    let engine = engine_req.unwrap_or_else(|| m.engine.clone());
    if !allowed_engine(&m.name, &engine) { return r#"{"error":"engine pin violation"}"#.into(); }
    let gguf = root.join("GGUF").join("llm-collection").join(m.gguf_rel.replace('/', "\\"));
    let Some(exe) = engine_exe(root, &engine).and_then(|e| {
        let b = e.with_file_name("llama-bench.exe"); if b.exists() { Some(b) } else { None }
    }) else { return r#"{"error":"engine llama-bench missing"}"#.into(); };
    // lane guard: refuse when machine busy (smoothed cpu > 55) or ram short
    let mg = CPU_SMOOTH.lock().unwrap();
    let cpu_now: f64 = { *mg }; drop(mg);
    let (_, avail, _) = ram();
    let need = (m.size_gb * 1.35 + 0.5) * 1e9;
    if (avail as f64) < need { return format!(r#"{{"error":"ram guard need {:.1}>free {:.1}"}}"#, need / 1e9, avail as f64 / 1e9); }
    if cpu_now > 55.0 { return format!(r#"{{"error":"lane busy: cpu {:.0}% > 55%, wait or STOP work"}}"#, cpu_now); }
    let key = format!("{}|{}|t{}", model, engine, threads);
    if benches().as_ref().unwrap().get(&key).map(|s| s.contains("\"state\":\"running\"")).unwrap_or(false) {
        return r#"{"error":"already running"}"#.into();
    }
    benches().as_mut().unwrap().insert(key.clone(), r#"{"state":"running"}"#.to_string());
    let keyc = key.clone();
    let rootc = root.clone();
     std::thread::spawn(move || {
        let outdir = rootc.join("results"); let _ = fs::create_dir_all(&outdir);
        let stamp = now_stamp();
        let args = format!("-m \"{}\" -t {} -p {} -n {} -r {} -ngl 0 -o jsonl",
            gguf.display(), threads, pp, tg, reps);
        let mut bench_cmd = std::process::Command::new(&exe);
        bench_cmd.raw_arg(&args)
            .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | 0x00000008)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if taps { bench_cmd.env("DAAVFX_DSPARK_TAPS", "1"); }
        let out = bench_cmd.spawn()
            .and_then(|mut c| { let mut so = String::new(); let mut se = String::new();
                let mut st = c.stdout.take().unwrap(); let mut se2 = c.stderr.take().unwrap();
                let h1 = std::thread::spawn(move || { let _ = st.read_to_string(&mut so); so }); // bare read ok
                let h2 = std::thread::spawn(move || { let _ = se2.read_to_string(&mut se); se });
                let _ = c.wait();
                Ok((h1.join().unwrap_or_default(), h2.join().unwrap_or_default(), c.try_wait().ok().flatten()))
            });
        let state = match out {
            Ok((stdout_txt, stderr_txt, status)) => {
                let fp_gg = file_fingerprint(&gguf);
                let fp_ex = file_fingerprint(&PathBuf::from(exe.clone()));
                let mut rowsf = fs::OpenOptions::new().create(true).append(true)
                    .open(outdir.join("speed_rows.ndjson")).unwrap();
                let mut n = 0u32;
                for line in stdout_txt.lines().filter(|l| l.trim_start().starts_with('{')) {
                    let avg = find_str(line, 0, "avg_ts").map(|(_, v)| v).unwrap_or_default();
                    let typ = find_str(line, 0, "test").or_else(|| find_str(line, 0, "type")).map(|(_, v)| v).unwrap_or_default();
                    let n_gen = find_str(line, 0, "n_gen").map(|(_, v)| v).unwrap_or_default();
                    let n_prompt = find_str(line, 0, "n_prompt").map(|(_, v)| v).unwrap_or_default();
                    // fresh builds carry no "test" key: derive the kind from n_prompt/n_gen
                    let gen_n: i64 = n_gen.parse().unwrap_or(0);
                    let kind = if !typ.is_empty() { typ }
                        else if gen_n > 0 { format!("tg{}", n_gen) }
                        else { format!("pp{}", n_prompt) };
                    let n_tok = if gen_n > 0 { n_gen.clone() } else { n_prompt.clone() };
                    let row = format!(r#"{{"ts":"{}","model":"{}","engine":"{}","threads":{},"kind":"{}","n":{},"avg_ts":{},"gguf_fp":"{:x}","exe_fp":"{:x}","taps":{}}}"#,
                        stamp, jesc(&model), jesc(&engine), threads, jesc(&kind), n_tok, avg, fp_gg, fp_ex, taps);
                    let _ = writeln!(rowsf, "{}", row); n += 1;
                }
                let tail: String = stderr_txt.chars().rev().take(300).collect::<String>().chars().rev().collect();
                if n > 0 || status.map(|s| s.success()).unwrap_or(false) {
                    format!(r#"{{"state":"done","rows":{},"stderr_tail":"{}"}}"#, n, jesc(&tail))
                } else {
                    format!(r#"{{"state":"error","stderr_tail":"{}"}}"#, jesc(&tail))
                }
            }
            Err(e) => format!(r#"{{"state":"error","spawn":"{}"}}"#, e),
        };
        let mut g = benches(); g.as_mut().unwrap().insert(keyc, state);
    });
    format!(r#"{{"started":true,"key":"{}"}}"#, jesc(&key))
}

fn servers_json(root: &PathBuf) -> String {
    let models = models_from_registry(root);
    let mut items = Vec::new();
    for m in models.iter().filter(|m| m.port > 0) {
        let alive = probe_port(m.port as u16);
        let color = if alive { "alive" } else { "down" };
        items.push(format!(r#"{{"model":"{}","port":{},"engine":"{}","state":"{}"}}"#, jesc(&m.name), m.port, jesc(&m.engine), color));
    }
    format!("[{}]", items.join(","))
}

// ---------- M1: chat proxy (humans + agents share this exact endpoint) ----------
fn jesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => { o.push_str(&format!("\\u{:04x}", c as u32)); }
            c => o.push(c),
        }
    }
    o
}

fn probe_port(port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let addr = format!("127.0.0.1:{}", port);
    addr.to_socket_addrs().ok()
        .and_then(|mut it| it.next())
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(120)).is_ok()).unwrap_or(false)
}

// POST /api/chat {port, messages[], actor, max_tokens, model?} -> proxies to a live
// llama-server, measures wall clock, logs a row to results\chat_sessions.ndjson
fn chat_proxy(body: &str, root: &PathBuf) -> String {
    let port = find_str(body, 0, "port").and_then(|(_, v)| v.parse::<u32>().ok());
    let prompt = find_str(body, 0, "prompt").map(|(_, v)| v);
    let max_tokens = find_str(body, 0, "max_tokens").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(256);
    let seed = find_str(body, 0, "seed").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(42);
    let actor = find_str(body, 0, "actor").map(|(_, v)| v).unwrap_or_else(|| "human".into());
    let (Some(port), Some(prompt)) = (port, prompt) else {
        return r#"{"error":"need port + prompt"}"#.to_string();
    };
    let Some(addr) = format!("127.0.0.1:{}", port).to_socket_addr() else {
        return r#"{"error":"bad port"}"#.to_string();
    };
    let req_body = format!(
        r#"{{"messages":[{{"role":"user","content":"{}"}}],"max_tokens":{},"temperature":0.0,"seed":{}}}"#,
        jesc(&prompt), max_tokens, seed);
    let t0 = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3));
    let Ok(mut stream) = stream else { return r#"{"error":"server not listening on that port"}"#.to_string(); };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let http = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        req_body.len(), req_body);
    if stream.write_all(http.as_bytes()).is_err() { return r#"{"error":"write failed"}"#.to_string(); }
    let mut resp = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => { resp.push_str(&String::from_utf8_lossy(&buf[..n])); if resp.len() > 1_500_000 { break; } }
            Err(_) => break,
        }
    }
    let wall_ms = t0.elapsed().as_millis() as u64;
    let json_part = resp.find("\r\n\r\n").map(|i| &resp[i + 4..]).unwrap_or(&resp);
    let mut reply = find_str(json_part, 0, "content").map(|(_, v)| v).unwrap_or_default();
    let reasoning = find_str(json_part, 0, "reasoning_content").map(|(_, v)| v).unwrap_or_default();
    if reply.trim().is_empty() && !reasoning.trim().is_empty() { reply = reasoning.clone(); }
    let ctok = find_str(json_part, 0, "completion_tokens").and_then(|(_, v)| v.parse::<f64>().ok()).unwrap_or(0.0);
    let ptok = find_str(json_part, 0, "prompt_tokens").and_then(|(_, v)| v.parse::<f64>().ok()).unwrap_or(0.0);
    let finish = find_str(json_part, 0, "finish_reason").map(|(_, v)| v).unwrap_or_default();
    let tps = if wall_ms > 0 { ctok * 1000.0 / wall_ms as f64 } else { 0.0 };
    // append-only row (the store is the database)
    let dir = root.join("results");
    let _ = fs::create_dir_all(&dir);
    let row = format!(
        r#"{{"ts":"{}","actor":"{}","session":"dash","port":{},"actor_max_tokens":{},"seed":{},"wall_ms":{},"prompt_tokens":{},"completion_tokens":{},"tps":{:.1},"finish":"{}","prompt_sha":"{:x}","reply_len":{}}}"#,
        now_stamp(), jesc(&actor), port, max_tokens, seed, wall_ms, ptok, ctok, tps, jesc(&finish),
        fnv1a(&prompt), reply.chars().count());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("chat_sessions.ndjson")) {
        let _ = writeln!(f, "{}", row);
    }
    let has_reasoning = !reasoning.trim().is_empty();
    format!(r#"{{"reply":"{}","reasoning_split":{},"wall_ms":{},"completion_tokens":{},"prompt_tokens":{},"tps":{:.1},"finish":"{}","logged":true}}"#,
        jesc(&reply), has_reasoning, wall_ms, ctok, ptok, tps, jesc(&finish))
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); }
    h
}
fn now_stamp() -> String {
    // seconds since epoch as decimal (no chrono dep); humans read rows via UI anyway
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{}", secs)
}

trait ToSocketAddrExt { fn to_socket_addr(&self) -> Option<std::net::SocketAddr>; }
impl ToSocketAddrExt for String {
    fn to_socket_addr(&self) -> Option<std::net::SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs().ok().and_then(|mut it| it.next())
    }
}

// ---------- HTTP ----------
fn read_request(s: &mut TcpStream) -> String {
    let _ = s.set_read_timeout(Some(Duration::from_millis(3000)));
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let mut want_body = 0usize;
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if want_body == 0 {
                    if let Some(hd) = find_substr(&raw, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..hd]).to_string();
                        for line in head.lines() {
                            let l = line.to_lowercase();
                            if l.starts_with("content-length:") {
                                want_body = l[15..].trim().parse().unwrap_or(0);
                            }
                        }
                        let have = raw.len() - (hd + 4);
                        if have >= want_body { break; }
                    } else if raw.len() > 1_000_000 { break; }
                } else if buf.len() < 8192 { break; } // drain mode: we have length
            }
        }
        if raw.len() > 2_000_000 { break; }
    }
    String::from_utf8_lossy(&raw).to_string()
}
fn find_substr(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn handle(mut s: TcpStream, root: &PathBuf) {
    let req = read_request(&mut s);
    if req.is_empty() { return; }
    let method = req.split_whitespace().next().unwrap_or("GET");
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let (status, body, ctype) = match (method, path) {
        ("GET", "/api/metrics") => ("200 OK", metrics_json(), "application/json"),
        ("GET", p) if p.starts_with("/api/servers") => ("200 OK", servers_json(root), "application/json"),
        ("GET", p) if p.starts_with("/api/models") => ("200 OK", models_json(root), "application/json"),
        ("GET", p) if p.starts_with("/api/doctor") => ("200 OK", doctor_json(root), "application/json"),
        ("GET", p) if p.starts_with("/api/taps") => ("200 OK", taps_json(root), "application/json"),
        ("POST", "/api/chat") => {
            let body = req.find("\r\n\r\n").map(|i| req[i + 4..].to_string()).unwrap_or_default();
            ("200 OK", chat_proxy(&body, root), "application/json")
        }
        ("POST", "/api/engine/flip") => {
            let body = req.find("\r\n\r\n").map(|i| req[i + 4..].to_string()).unwrap_or_default();
            ("200 OK", engine_flip(&body, root), "application/json")
        }
        ("POST", "/api/engine/stop") => {
            let body = req.find("\r\n\r\n").map(|i| req[i + 4..].to_string()).unwrap_or_default();
            ("200 OK", engine_stop(&body), "application/json")
        }
        ("GET", p) if p.starts_with("/api/engine/state") => ("200 OK", engine_state(root), "application/json"),
        ("POST", "/api/bench/run") => {
            let body = req.find("\r\n\r\n").map(|i| req[i + 4..].to_string()).unwrap_or_default();
            ("200 OK", bench_run(&body, root), "application/json")
        }
        ("GET", p) if p.starts_with("/api/bench/state") => {
            let g = benches();
            let items: Vec<String> = g.as_ref().unwrap().iter().map(|(k, v)| format!(r#"{{"key":"{}","result":{}}}"#, jesc(k), v)).collect();
            ("200 OK", format!("[{}]", items.join(",")), "application/json")
        }
        ("GET", p) if p.starts_with("/api/rows") => {
            let store = req.split("store=").nth(1).and_then(|s| s.split([' ', '&', '\r', '\n']).next()).unwrap_or("speed_rows");
            let p2 = root.join("results").join(format!("{}.ndjson", store));
            let txt = fs::read_to_string(&p2).unwrap_or_default();
            let lines: Vec<&str> = txt.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail: Vec<String> = lines.into_iter().rev().take(25).map(|s| s.to_string()).collect();
            ("200 OK", format!("[{}]", tail.join(",")), "application/json")
        }
        _ => ("200 OK", dashboard(root), "text/html; charset=utf-8"),
    };
    let head = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", status, ctype, body.len());
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

static CPU_SMOOTH: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);

fn metrics_json() -> String {
    let (tot, avail, load) = ram();
    let raw = cpu_pct_pdh(); // Task-Manager-identical counter
    // EWMA(0.35): smooth out 1-sample spikes; converges in ~3 refreshes
    let mut sm = CPU_SMOOTH.lock().unwrap();
    *sm = if *sm == 0.0 { raw } else { *sm + 0.35 * (raw - *sm) };
    let pct = *sm;
    format!("{{\"cpu_pct\":{:.1},\"ram_total_gb\":{:.2},\"ram_avail_gb\":{:.2},\"ram_load_pct\":{}}}",
            pct, tot as f64 / 1e9, avail as f64 / 1e9, load)
}

// ---------- dashboard: carbon-platinum SPA (design law borrowed from quantum_dashboard-fx) ----------
// The whole UI lives in src/dash.html so the design can evolve without touching Rust.
// Every number on that page comes from /api/* endpoints below - no theater, no fixtures.
const DASH: &str = include_str!("dash.html");

fn dashboard(_root: &PathBuf) -> String { DASH.to_string() }

// full registry rows for the fleet grid (client-side merges live state from /api/servers)
fn models_json(root: &PathBuf) -> String {
    let models = models_from_registry(root);
    let items: Vec<String> = models.iter().map(|m| format!(
        r#"{{"model":"{}","gguf":"{}","engine":"{}","port":{},"size_gb":{:.2},"think":"{}","status":"{}","exists":{}}}"#,
        jesc(&m.name), jesc(&m.gguf_rel), jesc(&m.engine), m.port, m.size_gb,
        jesc(&m.think), jesc(&m.status), m.exists
    )).collect();
    format!("[{}]", items.join(","))
}

// latest doctor-sweep-*.txt counters (scanned / errors / warnings)
fn doctor_json(root: &PathBuf) -> String {
    let (s, e, w) = doctor_summary(root);
    format!("{{\"scanned\":{},\"errs\":{},\"warns\":{}}}", s, e, w)
}

fn main() {
    let root = env::var("LLAB_ROOT").unwrap_or_else(|_| {
        r"F:\DAAVILE_ECOSYSTEM\trading_ecosystem_11\trading_ecosystem_9.0\local_llm".to_string()
    });
    let root = PathBuf::from(root);
    let port: u16 = env::var("LLAB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(7575);
    let lis = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("bind 127.0.0.1:{port}: {e} (port occupied? set LLAB_PORT)");
        std::process::exit(1);
    });
    println!("llab live at http://127.0.0.1:{port}  root={}  (ctrl+c to stop)", root.display());
    for s in lis.incoming() {
        if let Ok(s) = s { handle(s, &root); }
    }
}
