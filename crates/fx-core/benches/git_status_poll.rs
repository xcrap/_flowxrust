//! Measures status parsing, which is the hot path in git polling: it runs for
//! every project every few seconds, and it holds the only string work between
//! two subprocesses.
//!
//! Unlike the Swift original, this needs no opt-in environment flag — parsing
//! real captured output is cheap to run and there is nothing environment
//! dependent about it.

use criterion::{Criterion, criterion_group, criterion_main};
use fx_core::git::{GitStatusParser, LineCounts};
use std::collections::HashMap;

use std::hint::black_box;

/// A working tree with a mix of statuses, including the awkward records:
/// renames, copies, binary files, untracked paths, and a detached HEAD.
fn realistic_status_output(file_count: usize) -> String {
    let mut records =
        vec!["## feature/rich-diff...origin/feature/rich-diff [ahead 12, behind 3]".to_string()];

    for i in 0..file_count {
        match i % 7 {
            0 => records.push(format!("M  src/module_{i}.rs")),
            1 => records.push(format!(" M src/other_{i}.rs")),
            2 => records.push(format!("MM src/both_{i}.rs")),
            3 => records.push(format!("A  src/added_{i}.rs")),
            4 => records.push(format!("D  src/removed_{i}.rs")),
            5 => records.push(format!("?? untracked_{i}.txt")),
            // A rename consumes two records: the entry, then its source path.
            _ => {
                records.push(format!("R  src/new_name_{i}.rs"));
                records.push(format!("src/old_name_{i}.rs"));
            }
        }
    }

    let mut out = records.join("\0");
    out.push('\0');
    out
}

fn realistic_numstat_output(file_count: usize) -> String {
    let mut records = Vec::new();
    for i in 0..file_count {
        // Every seventh file is binary, which shifts its path into a third record.
        if i % 7 == 6 {
            records.push("-\t-\t".to_string());
            records.push(format!("assets/binary_{i}.png"));
        } else {
            records.push(format!("{}\t{}\tsrc/module_{i}.rs", i % 23 + 1, i % 11));
        }
    }
    let mut out = records.join("\0");
    out.push('\0');
    out
}

fn parse_status(c: &mut Criterion) {
    let mut group = c.benchmark_group("git/status");

    for size in [50usize, 500, 5_000] {
        let output = realistic_status_output(size);
        group.bench_function(format!("status_{size}_files"), |b| {
            b.iter(|| GitStatusParser::parse_status(black_box(&output)))
        });
    }

    group.finish();
}

fn parse_numstat(c: &mut Criterion) {
    let mut group = c.benchmark_group("git/numstat");

    for size in [50usize, 500, 5_000] {
        let output = realistic_numstat_output(size);
        group.bench_function(format!("numstat_{size}_files"), |b| {
            b.iter(|| GitStatusParser::parse_numstat(black_box(&output)))
        });
    }

    group.finish();
}

fn assemble_info(c: &mut Criterion) {
    // Once parsing is done, assembling the snapshot is pure map work over the
    // already-parsed records.
    let output = realistic_status_output(2_000);
    let status = GitStatusParser::parse_status(&output);
    let numstat: HashMap<String, LineCounts> = status
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            (
                f.path.clone(),
                LineCounts {
                    additions: i % 23,
                    deletions: i % 11,
                },
            )
        })
        .collect();

    c.bench_function("make_info_2000_files", |b| {
        b.iter(|| {
            GitStatusParser::make_info(
                black_box(&status),
                black_box(&numstat),
                black_box(&numstat),
                true,
                black_box(0xABCD_1234),
            )
        })
    });
}

criterion_group!(benches, parse_status, parse_numstat, assemble_info);
criterion_main!(benches);
