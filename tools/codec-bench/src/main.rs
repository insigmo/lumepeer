//! codec-bench: per-frame encode time, CPU load and bitstream of one encoder
//! on one recorded clip, fed in real time (docs/research/software-av1.md).
//!
//! The clip is raw BGRA, read ahead on its own thread. Frames are released
//! at the session frame rate; each one is converted to I420 with the
//! converter lumepeer's openh264 path uses (except for openh264 itself, which
//! goes through lumepeer's own wrapper and converts inside it), then encoded.
//!
//!   codec-bench encode --input clip.bgra --width 1920 --height 1080
//!       --encoder svt --speed 11 --kbps 4000 --screen 1
//!       --out run.ivf --csv run.csv --json run.json
//!   codec-bench ref --input clip.bgra --width 1920 --height 1080 --out ref.yuv
//!   codec-bench null ...   (read + convert only: the harness's own CPU cost)

mod cpu;
mod enc;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use enc::{Codec, Input};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

struct Args {
    mode: String,
    input: String,
    width: usize,
    height: usize,
    frames: usize,
    start: usize,
    fps: u32,
    encoder: String,
    kbps: u32,
    speed: i32,
    screen: bool,
    threads: i32,
    tiles: i32,
    min_q: i32,
    max_q: i32,
    extra: String,
    paced: bool,
    out: Option<String>,
    csv: Option<String>,
    json: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mode = it.next().ok_or("usage: codec-bench <encode|ref|null> --input ...")?;
    let mut a = Args {
        mode,
        input: String::new(),
        width: 0,
        height: 0,
        frames: 0,
        start: 0,
        fps: 30,
        encoder: "openh264".into(),
        kbps: 4000,
        speed: 10,
        screen: false,
        threads: 0,
        tiles: -1,
        min_q: 10,
        max_q: 56,
        extra: String::new(),
        paced: true,
        out: None,
        csv: None,
        json: None,
    };
    while let Some(k) = it.next() {
        if k == "--unpaced" {
            a.paced = false;
            continue;
        }
        let v = it.next().ok_or(format!("{k} needs a value"))?;
        let num = |v: &str| v.parse::<i64>().map_err(|e| format!("{k}: {e}"));
        match k.as_str() {
            "--input" => a.input = v,
            "--width" => a.width = num(&v)? as usize,
            "--height" => a.height = num(&v)? as usize,
            "--frames" => a.frames = num(&v)? as usize,
            "--start" => a.start = num(&v)? as usize,
            "--fps" => a.fps = num(&v)? as u32,
            "--encoder" => a.encoder = v,
            "--kbps" => a.kbps = num(&v)? as u32,
            "--speed" => a.speed = num(&v)? as i32,
            "--screen" => a.screen = num(&v)? != 0,
            "--threads" => a.threads = num(&v)? as i32,
            "--tiles" => a.tiles = num(&v)? as i32,
            "--minq" => a.min_q = num(&v)? as i32,
            "--maxq" => a.max_q = num(&v)? as i32,
            "--extra" => a.extra = v,
            "--out" => a.out = Some(v),
            "--csv" => a.csv = Some(v),
            "--json" => a.json = Some(v),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if a.input.is_empty() || a.width == 0 || a.height == 0 {
        return Err("--input, --width and --height are required".into());
    }
    Ok(a)
}

type Reader = (mpsc::Receiver<Vec<u8>>, mpsc::Sender<Vec<u8>>, std::thread::JoinHandle<Duration>);

/// Reads frames ahead of the encode loop and hands buffers back and forth, so
/// neither disk reads nor 8 MB allocations land in the timed path. Returns the
/// reader thread's own CPU time when it ends, to be taken off the total.
fn reader(path: &str, frame_bytes: usize, start: usize, count: usize) -> Result<Reader, String> {
    let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    file.seek(SeekFrom::Start((start * frame_bytes) as u64)).map_err(|e| e.to_string())?;
    let (full_tx, full_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (empty_tx, empty_rx) = mpsc::channel::<Vec<u8>>();
    for _ in 0..6 {
        empty_tx.send(vec![0u8; frame_bytes]).map_err(|e| e.to_string())?;
    }
    let handle = std::thread::spawn(move || {
        for _ in 0..count {
            let Ok(mut buf) = empty_rx.recv() else { break };
            buf.resize(frame_bytes, 0);
            if file.read_exact(&mut buf).is_err() || full_tx.send(buf).is_err() {
                break;
            }
        }
        cpu::thread()
    });
    Ok((full_rx, empty_tx, handle))
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn stats(name: &str, mut v: Vec<f64>) -> String {
    v.retain(|x| x.is_finite());
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = if v.is_empty() { f64::NAN } else { v.iter().sum::<f64>() / v.len() as f64 };
    let num = |x: f64| if x.is_finite() { format!("{x:.3}") } else { "null".to_owned() };
    format!(
        "\"{name}\": {{\"mean\": {}, \"p50\": {}, \"p95\": {}, \"p99\": {}, \"max\": {}}}",
        num(mean),
        num(pct(&v, 0.5)),
        num(pct(&v, 0.95)),
        num(pct(&v, 0.99)),
        num(v.last().copied().unwrap_or(f64::NAN))
    )
}

fn ivf_header(w: usize, h: usize, fps: u32, frames: u32) -> [u8; 32] {
    let mut hd = [0u8; 32];
    hd[0..4].copy_from_slice(b"DKIF");
    hd[6..8].copy_from_slice(&32u16.to_le_bytes());
    hd[8..12].copy_from_slice(b"AV01");
    hd[12..14].copy_from_slice(&(w as u16).to_le_bytes());
    hd[14..16].copy_from_slice(&(h as u16).to_le_bytes());
    hd[16..20].copy_from_slice(&fps.to_le_bytes());
    hd[20..24].copy_from_slice(&1u32.to_le_bytes());
    hd[24..28].copy_from_slice(&frames.to_le_bytes());
    hd
}

fn convert(buf: &[u8], w: usize, h: usize) -> YUVBuffer {
    YUVBuffer::from_bgra8_source(BgraSliceU8::new(buf, (w, h)))
}

fn write_yuv(out: &mut impl Write, yuv: &YUVBuffer) -> std::io::Result<()> {
    let (w, h) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    for r in 0..h {
        out.write_all(&yuv.y()[r * ys..r * ys + w])?;
    }
    for r in 0..h / 2 {
        out.write_all(&yuv.u()[r * us..r * us + w / 2])?;
    }
    for r in 0..h / 2 {
        out.write_all(&yuv.v()[r * vs..r * vs + w / 2])?;
    }
    Ok(())
}

#[derive(Default, Clone)]
struct Rec {
    sched_ms: f64,
    start_ms: f64,
    conv_ms: f64,
    call_ms: f64,
    done_ms: f64,
    out_ms: f64,
    bytes: usize,
    key: bool,
}

fn run(a: &Args) -> Result<(), String> {
    let (w, h) = (a.width, a.height);
    let frame_bytes = w * h * 4;
    let file_frames = std::fs::metadata(&a.input).map_err(|e| e.to_string())?.len() as usize / frame_bytes;
    let count = if a.frames == 0 { file_frames - a.start } else { a.frames.min(file_frames - a.start) };
    let ncpu = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

    if a.mode == "ref" {
        let (rx, back, handle) = reader(&a.input, frame_bytes, a.start, count)?;
        let path = a.out.clone().ok_or("--out is required for ref")?;
        let mut out = BufWriter::new(File::create(&path).map_err(|e| e.to_string())?);
        let mut conv = Vec::with_capacity(count);
        for _ in 0..count {
            let buf = rx.recv().map_err(|_| "clip ended early")?;
            let t = Instant::now();
            let yuv = convert(&buf, w, h);
            conv.push(t.elapsed().as_secs_f64() * 1e3);
            write_yuv(&mut out, &yuv).map_err(|e| e.to_string())?;
            let _ = back.send(buf);
        }
        drop(back);
        let _ = handle.join();
        out.flush().map_err(|e| e.to_string())?;
        eprintln!("ref: {count} frames -> {path}; {}", stats("convert_ms", conv));
        return Ok(());
    }

    let params = enc::Params {
        width: w,
        height: h,
        fps: a.fps,
        kbps: a.kbps,
        speed: a.speed,
        screen: a.screen,
        threads: a.threads,
        tiles: a.tiles,
        min_q: a.min_q,
        max_q: a.max_q,
        extra: a.extra.clone(),
    };
    let null = a.mode == "null";
    let t_init = Instant::now();
    let mut encoder = if null { None } else { Some(enc::build(&a.encoder, &params)?) };
    let init_ms = t_init.elapsed().as_secs_f64() * 1e3;
    let wants_i420 = encoder.as_ref().is_none_or(|e| e.wants_i420());
    let codec = encoder.as_ref().map_or(Codec::Av1, |e| e.codec());
    let describe = encoder.as_ref().map_or_else(|| "null (read + convert)".to_owned(), |e| e.describe());

    let mut bitstream = match (&a.out, null) {
        (Some(p), false) => Some(BufWriter::new(File::create(p).map_err(|e| e.to_string())?)),
        _ => None,
    };
    if let (Some(bs), Codec::Av1) = (bitstream.as_mut(), codec) {
        bs.write_all(&ivf_header(w, h, a.fps, count as u32)).map_err(|e| e.to_string())?;
    }
    let write_packet = |bs: &mut Option<BufWriter<File>>, pkt: &enc::Packet| -> Result<(), String> {
        let Some(bs) = bs.as_mut() else { return Ok(()) };
        if pkt.data.is_empty() {
            return Ok(());
        }
        if codec == Codec::Av1 {
            bs.write_all(&(pkt.data.len() as u32).to_le_bytes()).map_err(|e| e.to_string())?;
            bs.write_all(&pkt.frame.to_le_bytes()).map_err(|e| e.to_string())?;
        }
        bs.write_all(&pkt.data).map_err(|e| e.to_string())
    };

    let (rx, back, handle) = reader(&a.input, frame_bytes, a.start, count)?;
    // Let the reader fill its queue before the clock starts.
    std::thread::sleep(Duration::from_millis(300));

    let interval = Duration::from_secs_f64(1.0 / f64::from(a.fps));
    let mut recs = vec![Rec::default(); count];
    let mut arrivals: HashMap<u64, (f64, usize, bool)> = HashMap::new();

    let cpu0 = cpu::process();
    let t0 = Instant::now();
    let ms = |t: Instant| t.duration_since(t0).as_secs_f64() * 1e3;
    for i in 0..count {
        let sched = t0 + interval * i as u32;
        if a.paced {
            let now = Instant::now();
            if sched > now {
                std::thread::sleep(sched - now);
            }
        }
        let mut buf = rx.recv().map_err(|_| format!("clip ended early at frame {i}"))?;
        let start = Instant::now();
        let yuv = if wants_i420 { Some(convert(&buf, w, h)) } else { None };
        let converted = Instant::now();
        let mut out = Vec::new();
        if let Some(e) = encoder.as_mut() {
            let input = match &yuv {
                Some(y) => Input::I420(y),
                None => Input::Bgra(&mut buf),
            };
            out = e.encode(input, i as i64).map_err(|err| format!("frame {i}: {err}"))?;
        }
        let done = Instant::now();
        for pkt in &out {
            arrivals.insert(pkt.frame, (ms(done), pkt.data.len(), pkt.key));
            write_packet(&mut bitstream, pkt)?;
        }
        recs[i] = Rec {
            sched_ms: if a.paced { ms(sched) } else { ms(start) },
            start_ms: ms(start),
            conv_ms: converted.duration_since(start).as_secs_f64() * 1e3,
            call_ms: done.duration_since(converted).as_secs_f64() * 1e3,
            done_ms: ms(done),
            ..Rec::default()
        };
        if null {
            // The null run's "output" is the converted picture itself.
            arrivals.insert(i as u64, (ms(done), 0, false));
        }
        let _ = back.send(buf);
    }
    if let Some(e) = encoder.as_mut() {
        let flushed = e.flush()?;
        let done = Instant::now();
        for pkt in &flushed {
            arrivals.insert(pkt.frame, (ms(done), pkt.data.len(), pkt.key));
            write_packet(&mut bitstream, pkt)?;
        }
    }
    let wall = t0.elapsed();
    let cpu_total = cpu::process() - cpu0;
    drop(back);
    let reader_cpu = handle.join().unwrap_or_default();
    if let Some(bs) = bitstream.as_mut() {
        bs.flush().map_err(|e| e.to_string())?;
    }

    let mut total_bytes = 0usize;
    let mut keys = 0usize;
    let mut empty = 0usize;
    for (i, r) in recs.iter_mut().enumerate() {
        if let Some(&(t, bytes, key)) = arrivals.get(&(i as u64)) {
            r.out_ms = t;
            r.bytes = bytes;
            r.key = key;
            total_bytes += bytes;
            keys += usize::from(key);
            empty += usize::from(bytes == 0 && !null);
        } else {
            r.out_ms = f64::NAN;
            empty += 1;
        }
    }

    if let Some(p) = &a.csv {
        let mut c = BufWriter::new(File::create(p).map_err(|e| e.to_string())?);
        let io = |e: std::io::Error| e.to_string();
        writeln!(c, "frame,sched_ms,start_ms,conv_ms,call_ms,done_ms,out_ms,latency_ms,bytes,key").map_err(io)?;
        for (i, r) in recs.iter().enumerate() {
            writeln!(
                c,
                "{i},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{}",
                r.sched_ms,
                r.start_ms,
                r.conv_ms,
                r.call_ms,
                r.done_ms,
                r.out_ms,
                r.out_ms - r.sched_ms,
                r.bytes,
                u8::from(r.key)
            )
            .map_err(io)?;
        }
        c.flush().map_err(io)?;
    }

    let secs = count as f64 / f64::from(a.fps);
    let enc_cpu = cpu_total.saturating_sub(reader_cpu);
    let cpu_pct = enc_cpu.as_secs_f64() / wall.as_secs_f64() / ncpu as f64 * 100.0;
    let frame_ms: Vec<f64> = recs.iter().map(|r| r.conv_ms + r.call_ms).collect();
    let late = recs.iter().filter(|r| r.start_ms - r.sched_ms > interval.as_secs_f64() * 1e3).count();
    let json = format!(
        "{{\"mode\": \"{}\", \"encoder\": \"{}\", \"describe\": \"{}\", \"input\": \"{}\", \"width\": {w}, \"height\": {h}, \
         \"fps\": {}, \"paced\": {}, \"frames\": {count}, \"kbps_target\": {}, \"kbps_actual\": {:.1}, \"bytes\": {total_bytes}, \
         \"keyframes\": {keys}, \"empty_frames\": {empty}, \"late_frames\": {late}, \"init_ms\": {init_ms:.1}, \
         \"wall_s\": {:.3}, \"cpu_s\": {:.3}, \"reader_cpu_s\": {:.3}, \"ncpu\": {ncpu}, \"cpu_pct_all_cores\": {cpu_pct:.2}, \
         \"cpu_cores_used\": {:.3}, {}, {}, {}, {}}}",
        a.mode,
        a.encoder,
        describe.replace('"', "'"),
        a.input.replace('\\', "/"),
        a.fps,
        a.paced,
        a.kbps,
        total_bytes as f64 * 8.0 / secs / 1000.0,
        wall.as_secs_f64(),
        cpu_total.as_secs_f64(),
        reader_cpu.as_secs_f64(),
        enc_cpu.as_secs_f64() / wall.as_secs_f64(),
        stats("frame_ms", frame_ms),
        stats("conv_ms", recs.iter().map(|r| r.conv_ms).collect()),
        stats("call_ms", recs.iter().map(|r| r.call_ms).collect()),
        stats("latency_ms", recs.iter().map(|r| r.out_ms - r.sched_ms).collect()),
    );
    if let Some(p) = &a.json {
        std::fs::write(p, &json).map_err(|e| e.to_string())?;
    }
    println!("{json}");
    Ok(())
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
