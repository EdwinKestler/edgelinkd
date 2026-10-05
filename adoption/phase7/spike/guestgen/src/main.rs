//! `guestgen <in-dir> <out-dir>`: every `*.wat` in `in-dir` becomes `<name>.wasm` in `out-dir`,
//! plus a generated `bulk.wasm` (~1,500 functions) that stands in for a realistic compiled
//! Rust guest when measuring compile time and translated-code memory.
use std::{env, fmt::Write, fs, path::Path};

const BULK_FUNCS: usize = 1500;

fn bulk_wat() -> String {
    let mut w = String::from(
        "(module\n  (import \"edgelink:node/v1\" \"emit\" (func $emit (param i32 i32 i32) (result i32)))\n  \
         (memory (export \"memory\") 1 1)\n  \
         (func (export \"el_abi_version\") (result i32) i32.const 1)\n  \
         (func (export \"el_alloc\") (param i32) (result i32) i32.const 1024)\n",
    );
    for i in 0..BULK_FUNCS {
        let _ = write!(
            w,
            "  (func $f{i} (param $a i32) (param $b i32) (result i32) (local $t i32)\n    \
             (local.set $t (i32.add (local.get $a) (i32.const {i})))\n    \
             (if (i32.gt_u (local.get $t) (local.get $b)) (then (local.set $t (i32.sub (local.get $t) (local.get $b)))))\n    \
             (local.set $t (i32.xor (i32.mul (local.get $t) (i32.const 31)) (i32.shr_u (local.get $t) (i32.const 3))))\n    \
             (i32.store8 (i32.add (i32.const 2048) (i32.and (local.get $t) (i32.const 1023))) (local.get $t))\n    \
             (i32.add (local.get $t) (i32.load8_u (i32.add (i32.const 2048) (i32.and (local.get $a) (i32.const 1023))))))\n"
        );
    }
    w.push_str("  (func (export \"el_on_input\") (param $ptr i32) (param $len i32) (result i32) (local $acc i32)\n");
    for i in (0..BULK_FUNCS).step_by(50) {
        let _ = writeln!(w, "    (local.set $acc (call $f{i} (local.get $acc) (local.get $len)))");
    }
    w.push_str("    (call $emit (i32.const 0) (local.get $ptr) (local.get $len))))\n");
    w
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    fs::create_dir_all(dst)?;
    let mut sources: Vec<(String, String)> = vec![("bulk".into(), bulk_wat())];
    for entry in fs::read_dir(src)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wat") {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            sources.push((name, fs::read_to_string(&path)?));
        }
    }
    for (name, text) in sources {
        let bytes = wat::parse_str(&text)?;
        let out = dst.join(format!("{name}.wasm"));
        fs::write(&out, &bytes)?;
        println!("{} {} bytes", out.display(), bytes.len());
    }
    Ok(())
}
