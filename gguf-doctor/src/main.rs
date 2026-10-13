// gguf-doctor: GGUF metadata inspector + institutional-knowledge checklist.
// std-only. Reads header + kv only; never mmap-loads tensors.

use std::env;
use std::fs::File;
use std::io::{BufReader, Read};

struct R {
    r: BufReader<File>,
}
impl R {
    fn u8(&mut self) -> u8 { let mut b = [0u8; 1]; self.r.read_exact(&mut b).unwrap(); b[0] }
    fn u16(&mut self) -> u16 { let mut b = [0u8; 2]; self.r.read_exact(&mut b).unwrap(); u16::from_le_bytes(b) }
    fn u32(&mut self) -> u32 { let mut b = [0u8; 4]; self.r.read_exact(&mut b).unwrap(); u32::from_le_bytes(b) }
    fn i32(&mut self) -> i32 { self.u32() as i32 }
    fn u64(&mut self) -> u64 { let mut b = [0u8; 8]; self.r.read_exact(&mut b).unwrap(); u64::from_le_bytes(b) }
    fn i64(&mut self) -> i64 { self.u64() as i64 }
    fn f32(&mut self) -> f32 { f32::from_le_bytes(self.u32().to_le_bytes()) }
    fn f64(&mut self) -> f64 { f64::from_le_bytes(self.u64().to_le_bytes()) }
    fn string(&mut self) -> String {
        let n = self.u64() as usize;
        let mut b = vec![0u8; n];
        self.r.read_exact(&mut b).unwrap();
        String::from_utf8_lossy(&b).to_string()
    }
}

#[derive(Debug, Clone)]
enum V {
    U8(u8), I8(i8), U16(u16), I16(i16), U32(u32), I32(i32), F32(f32),
    Bool(bool), Str(String), Arr(Vec<V>), U64(u64), I64(i64), F64(f64),
}

fn read_val(r: &mut R, t: u32) -> V {
    match t {
        0 => V::U8(r.u8()),
        1 => V::I8(r.u8() as i8),
        2 => V::U16(r.u16()),
        3 => V::I16(r.u16() as i16),
        4 => V::U32(r.u32()),
        5 => V::I32(r.i32()),
        6 => V::F32(r.f32()),
        7 => V::Bool(r.u8() != 0),
        8 => V::Str(r.string()),
        9 => {
            let et = r.u32();
            let n = r.u64() as usize;
            (0..n).map(|_| read_val(r, et)).collect::<Vec<_>>().pipe(V::Arr)
        }
        10 => V::U64(r.u64()),
        11 => V::I64(r.i64()),
        12 => V::F64(r.f64()),
        other => panic!("unknown gguf value type {other}"),
    }
}

trait Pipe: Sized { fn pipe<T, F: FnOnce(Self) -> T>(self, f: F) -> T { f(self) } }
impl<T> Pipe for T {}

fn get<'a>(kv: &'a [(String, V)], key: &str) -> Option<&'a V> {
    kv.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}
fn as_u64(v: Option<&V>) -> Option<u64> {
    match v {
        Some(V::U32(x)) => Some(*x as u64),
        Some(V::I32(x)) => Some(*x as u64),
        Some(V::U64(x)) => Some(*x),
        Some(V::I64(x)) => Some(*x as u64),
        _ => None,
    }
}
fn as_str<'a>(v: Option<&'a V>) -> Option<&'a str> {
    if let Some(V::Str(s)) = v { Some(s.as_str()) } else { None }
}

const KNOW_PRE_TOKENS: &[&str] = &[
    "default","gpt-2","llama3","llama-bpe","deepseek-llm","deepseek-coder","falcon",
    "mpt","starcoder","phi-3","command-r","qwen2","stablelm2","smollm2","codeshell",
    "jais2","smaug","dbrx","exaone","minerva","boss","chatglm-bpe","pangu","refact",
    "tekken","olmo","lfm2","exaone4","laguna","granite-docling","minimax-m2","qwen35",
];

fn main() {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: gguf-doctor <model.gguf> [--full]");
        std::process::exit(2);
    });
    let full = args.any(|a| a == "--full");
    let t0 = std::time::Instant::now();
    let file = File::open(&path).unwrap_or_else(|e| { eprintln!("open {path}: {e}"); std::process::exit(1); });
    let file_size = file.metadata().unwrap().len();
    let mut r = R { r: BufReader::with_capacity(1 << 20, file) };
    let mut magic = [0u8; 4];
    r.r.read_exact(&mut magic).unwrap();
    if &magic != b"GGUF" { eprintln!("NOT A GGUF FILE"); std::process::exit(1); }
    let version = r.u32();
    let n_tensors = r.u64();
    let n_kv = r.u64();
    let mut kv: Vec<(String, V)> = Vec::with_capacity(n_kv as usize, );
    for _ in 0..n_kv {
        let k = r.string();
        let t = r.u32();
        let v = read_val(&mut r, t);
        kv.push((k, v));
    }
    let mut tensor_names: Vec<String> = Vec::new();
    let mut total_bytes_est: u128 = 0;
    if full {
        for _ in 0..n_tensors {
            let name = r.string();
            let nd = r.u32() as usize;
            let mut dims: Vec<u64> = Vec::with_capacity(nd);
            for _ in 0..nd { dims.push(r.u64()); }
            let tt = r.u32();
            let _ = tt;
            let _off = r.u64();
            tensor_names.push(name);
            let mut elems: u128 = 1;
            for d in &dims { elems = elems.saturating_mul(*d as u128); }
            total_bytes_est += elems; // rough element count only at --full
        }
    }
    let read_ms = t0.elapsed().as_millis();

    let arch = as_str(get(&kv, "general.architecture")).unwrap_or("?");
    let name = as_str(get(&kv, "general.name")).unwrap_or("-");
    let ctx = as_u64(get(&kv, &format!("{arch}.context_length")));
    let vocab = as_u64(get(&kv, &format!("{arch}.vocab_size")));
    let n_blocks = as_u64(get(&kv, &format!("{arch}.block_count")));
    let rope_theta = get(&kv, &format!("{arch}.rope.freq_base"));
    let bos = as_u64(get(&kv, "tokenizer.ggml.bos_token_id"));
    let eos = as_u64(get(&kv, "tokenizer.ggml.eos_token_id"));
    let add_bos = get(&kv, "tokenizer.ggml.add_bos_token");
    let tok_model = as_str(get(&kv, "tokenizer.ggml.model"));
    let tok_pre = as_str(get(&kv, "tokenizer.ggml.pre"));
    let tmpl = get(&kv, "tokenizer.chat_template");
    let ftype = as_u64(get(&kv, "general.file_type"));

    println!("=== GGUF DOCTOR ===");
    println!("file        : {}", path);
    println!("size        : {:.2} GB ({} B)", file_size as f64 / 1e9, file_size);
    println!("gguf ver    : {}", version);
    println!("tensors     : {} (names read: {})", n_tensors, if full { tensor_names.len().to_string() } else { "no (use --full)".into() });
    println!("kv pairs    : {}", n_kv);
    println!("arch/name   : {} / {}", arch, name);
    println!("ctx         : {:?}", ctx);
    println!("vocab       : {:?}", vocab);
    println!("blocks      : {:?}", n_blocks);
    println!("rope theta  : {:?}", rope_theta.map(|_| "?".to_string()).unwrap_or("-".into()));
    println!("file_type   : {:?}", ftype);
    println!("tok model   : {:?}", tok_model);
    println!("tok pre     : {:?}", tok_pre);
    println!("bos/eos     : {:?} / {:?}", bos, eos);
    println!("add_bos     : {:?}", add_bos);
    println!("template    : {}", if tmpl.is_some() { "present" } else { "MISSING" });
    println!("parsed in   : {} ms", read_ms);

    println!("--- CHECKS ---");
    let mut errs = 0; let mut warns = 0;
    if eos.is_none() { println!("[ERR ] tokenizer.ggml.eos_token_id MISSING (baguettotron-2026-10-00 lesson: llama.cpp falls back eos<-bos => premature stops)"); errs += 1; }
    if bos.is_none() { println!("[WARN] bos_token_id missing"); warns += 1; }
    if let (Some(b), Some(e)) = (bos, eos) {
        if b == e { println!("[WARN] bos == eos == {} (start-token designs like Pleias; verify intent)", b); warns += 1; }
    }
    match tok_pre {
        Some(p) if KNOW_PRE_TOKENS.contains(&p) => println!("[OK  ] pre-tokenizer recognized: {}", p),
        Some(p) => { println!("[WARN] pre-tokenizer '{}' not in llama.cpp known list (converted-only paths - behavior drift risk)", p); warns += 1; }
        None => {
            if tok_model == Some("none") { println!("[INFO] tokenizer model 'none'") } else { println!("[WARN] no tokenizer.ggml.pre key"); warns += 1; }
        }
    }
    if tmpl.is_none() { println!("[WARN] no chat template"); warns += 1; }
    if let Some(t) = tmpl { if let V::Str(s) = t {
        if s.contains("<|im_start|>") { println!("[OK  ] ChatML"); }
        if s.contains("<think>") && !s.contains("enable_thinking") { println!("[WARN] hard-seeded <think> template, no enable_thinking switch (budget-fill risk; needs big max_tokens)"); warns += 1; }
        if s.contains("enable_thinking") { println!("[OK  ] enable_thinking switch in template"); }
        if s.contains("<start_of_turn>") { println!("[OK  ] gemma template"); }
    }}
    if let Some(c) = ctx { if c <= 2048 { println!("[WARN] ctx {} <= 2048: tight for thinking models (prompt+trace+answer share window)", c); warns += 1; } }
    if n_kv == 0 || n_tensors == 0 { println!("[ERR ] empty header fields"); errs += 1; }
    println!("--- VERDICT: {} error(s), {} warning(s) ---", errs, warns);
}
