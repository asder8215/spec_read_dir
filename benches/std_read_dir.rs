#![feature(try_blocks)]
#![allow(dead_code)]
#![allow(unused)]
use std::{fs::ReadDir, hint::black_box};

use criterion::{Criterion, criterion_group, criterion_main};

fn recursive_read_dir(read_dir: ReadDir) {
    for child in read_dir {
        let _ = try {
            let child = child?;
            if child.file_type()?.is_dir() {
                recursive_read_dir(std::fs::read_dir(child.path())?);
            }
        };
    }
}

fn bench_read_dir(c: &mut Criterion) {
    let home_path = "put_dir_path_here";

    c.bench_function("Std Read Dir", |b| {
        b.iter(|| {
            recursive_read_dir(std::fs::read_dir(home_path).unwrap())
        })
    });
}

criterion_group!(benches, bench_read_dir);
criterion_main!(benches);