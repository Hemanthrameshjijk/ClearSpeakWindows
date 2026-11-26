//! Groups saved fingerprints (`<call>.embs.bin` from `diarize_file --embs`) and prints one speaker number per window,
//! to compare the Rust grouping with the Python tuning bench: `cluster_embs <file.embs.bin> <speakers>`.
use asr_onnx::diarize;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let b = std::fs::read(&a[0]).expect("embs file");
    let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (n, dim) = (u(0), u(4));
    let mut embs = Vec::new();
    for k in 0..n {
        let o = 8 + k * (dim + 2) * 4;
        embs.push((0..dim).map(|d| f(o + 8 + d * 4)).collect::<Vec<f32>>());
    }
    let k: usize = a[1].parse().unwrap();
    let l = diarize::cluster(&embs, Some(k), None);
    println!("{}", l.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" "));
}
