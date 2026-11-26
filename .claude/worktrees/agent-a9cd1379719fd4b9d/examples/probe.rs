//! Dev tool: run an Everything query over IPC and print the item count.
//! cargo run --release --example probe -- "file: dm:today"

#[path = "../src/everything.rs"]
mod everything;
#[path = "../src/i18n.rs"]
#[allow(dead_code)]
mod i18n;

fn main() {
    for q in std::env::args().skip(1) {
        let t = std::time::Instant::now();
        let mut n = 0u32;
        let mut first = Vec::new();
        let r = everything::for_each_item(
            &q,
            |it| {
                n += 1;
                if first.len() < 3 {
                    first.push(format!("{}\\{}", String::from_utf16_lossy(it.path), String::from_utf16_lossy(it.name)));
                }
            },
            |_, _| {},
        );
        println!("{q:40} -> {n} items in {:?} {:?} {:?}", t.elapsed(), r.err(), first);
    }
}
