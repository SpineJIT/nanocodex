use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::Duration,
};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nanocodex_agent::{Nanocodex, OpenAi, Thinking};
use nanocodex_eval::{Evaluator, Sweep, Task, harbor::Harbor, vm::VmBackend};

const REPRESENTATIVE_TASK_ENV: &str = "NANOCODEX_EVAL_BENCH_TASK";

fn benchmark_eval_runtime(criterion: &mut Criterion) {
    let tasks_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tasks");
    let task_paths =
        ["extract-todos", "uppercase-message", "write-greeting"].map(|name| tasks_root.join(name));
    let tasks = task_paths
        .iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()
        .expect("checked-in benchmark tasks");
    let agent = Nanocodex::builder(OpenAi::new("benchmark-only").expect("static API key"))
        .instructions(
            "Work directly in the provided workspace. Complete the requested task, \
             verify your changes, and keep the final answer concise.",
        )
        .thinking(Thinking::Medium);
    let sweep = build_sweep(&tasks, &agent);

    let mut group = criterion.benchmark_group("eval_runtime");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("load_terminal_bench_task", |bencher| {
        bencher.iter(|| {
            black_box(Task::load(black_box(&task_paths[2])).expect("load benchmark task"));
        });
    });

    group.bench_function("validate_checked_in_smoke_task_package", |bencher| {
        bencher.iter(|| {
            black_box(&tasks[2])
                .validate_package()
                .expect("validate benchmark task");
        });
    });

    // Point this at a retained TB2.1 package for the release performance gate.
    // Keeping it opt-in avoids presenting the tiny checked-in smoke task as a
    // representative package-size measurement.
    if let Some(path) = std::env::var_os(REPRESENTATIVE_TASK_ENV).map(PathBuf::from) {
        let bytes = packaged_file_bytes(&path);
        let task = Task::load(&path).unwrap_or_else(|error| {
            panic!(
                "{REPRESENTATIVE_TASK_ENV}={} is not a loadable task: {error}",
                path.display()
            )
        });
        group.throughput(Throughput::Bytes(bytes));
        group.bench_function(
            "validate_representative_terminal_bench_task_package",
            |bencher| {
                bencher.iter(|| {
                    black_box(&task)
                        .validate_package()
                        .expect("validate representative benchmark task");
                });
            },
        );
        group.throughput(Throughput::Bytes(bytes.saturating_mul(4)));
        group.bench_function("validate_representative_harbor_identity_stack", |bencher| {
            bencher.iter(|| {
                Harbor::validate_task_package(black_box(&task))
                    .expect("validate representative Harbor task identity");
            });
        });
    }

    group.throughput(Throughput::Elements(sweep.attempt_count() as u64));
    group.bench_function("plan_3x4x5_sweep", |bencher| {
        bencher.iter(|| {
            black_box(build_sweep(black_box(&tasks), black_box(&agent)));
        });
    });

    let output = tempfile::tempdir().expect("benchmark output");
    let initial = Evaluator::builder(agent.clone(), VmBackend::builder().build())
        .output_directory(output.path())
        .resume_incomplete(sweep.clone())
        .build()
        .expect("initialize resumable job");
    drop(initial);
    group.throughput(Throughput::Elements(1));
    group.bench_function("reopen_incomplete_job", |bencher| {
        bencher.iter(|| {
            let evaluator = Evaluator::builder(agent.clone(), VmBackend::builder().build())
                .output_directory(output.path())
                .resume_incomplete(sweep.clone())
                .build()
                .expect("resume benchmark job");
            black_box(evaluator.directory());
            drop(evaluator);
        });
    });

    group.finish();
}

fn packaged_file_bytes(root: &Path) -> u64 {
    const FILES: [&str; 3] = ["task.toml", "instruction.md", "README.md"];
    const DIRECTORIES: [&str; 4] = ["environment", "tests", "solution", "steps"];

    FILES
        .into_iter()
        .map(|name| file_bytes(&root.join(name)))
        .chain(
            DIRECTORIES
                .into_iter()
                .map(|name| directory_file_bytes(&root.join(name))),
        )
        .fold(0_u64, u64::saturating_add)
}

fn directory_file_bytes(directory: &Path) -> u64 {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!(
            "read representative benchmark directory {}: {error}",
            directory.display()
        ),
    };
    entries
        .map(|entry| {
            let path = entry
                .unwrap_or_else(|error| panic!("read benchmark directory entry: {error}"))
                .path();
            if path.is_dir() {
                directory_file_bytes(&path)
            } else {
                file_bytes(&path)
            }
        })
        .fold(0_u64, u64::saturating_add)
}

fn file_bytes(path: &Path) -> u64 {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!(
            "read representative benchmark file {}: {error}",
            path.display()
        ),
    }
}

fn build_sweep(tasks: &[Task], agent: &nanocodex_agent::NanocodexBuilder) -> nanocodex_eval::Sweep {
    Sweep::builder()
        .tasks(tasks.to_vec())
        .trials(5)
        .agent("medium-defaults", agent.clone())
        .expect("valid agent identity")
        .agent("medium-web", agent.clone())
        .expect("valid agent identity")
        .agent("high-defaults", agent.clone())
        .expect("valid agent identity")
        .agent("high-web", agent.clone())
        .expect("valid agent identity")
        .build()
        .expect("valid benchmark sweep")
}

criterion_group!(benches, benchmark_eval_runtime);
criterion_main!(benches);
