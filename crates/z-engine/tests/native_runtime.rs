#![cfg(feature = "native")]

use rayon::prelude::*;
use z_engine::native::{default_native_threads, initialize_native_threads};

#[test]
fn runtime_child() {
    let Ok(case) = std::env::var("Z_STACK_TEST_THREAD_CASE") else {
        return;
    };
    match case.as_str() {
        "default" => {
            let info = initialize_native_threads(None).unwrap();
            assert_eq!(
                info.threads,
                Some(default_native_threads(info.logical_cpus))
            );
            assert_eq!(info.threads, Some(rayon::current_num_threads()));
        }
        "explicit" => {
            assert_eq!(initialize_native_threads(Some(2)).unwrap().threads, Some(2));
            assert_eq!(initialize_native_threads(None).unwrap().threads, Some(2));
            assert!(initialize_native_threads(Some(2)).is_ok());
            assert!(initialize_native_threads(Some(3)).is_err());
            assert_eq!(rayon::current_num_threads(), 2);
        }
        "environment" => {
            assert_eq!(initialize_native_threads(None).unwrap().threads, Some(3));
            assert_eq!(rayon::current_num_threads(), 3);
        }
        "existing" => {
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build_global()
                .unwrap();
            assert_eq!(initialize_native_threads(None).unwrap().threads, None);
            assert!(initialize_native_threads(Some(2)).is_err());
            assert_eq!(rayon::current_num_threads(), 2);
        }
        "invalid_environment" => {
            assert!(initialize_native_threads(None).is_err());
            assert_eq!(initialize_native_threads(Some(1)).unwrap().threads, Some(1));
            assert!(initialize_native_threads(None).is_ok());
        }
        "invalid_explicit" => {
            assert!(initialize_native_threads(Some(0)).is_err());
            assert!(initialize_native_threads(Some(65)).is_err());
            assert_eq!(initialize_native_threads(Some(1)).unwrap().threads, Some(1));
        }
        _ => panic!("unknown runtime qualification case"),
    }
    // Real Rayon work runs through the configured pool, preserving results.
    assert_eq!((0..1000u64).into_par_iter().sum::<u64>(), 499500);
}

#[test]
fn native_runtime_configuration_isolated_processes() {
    for (case, env) in [
        ("default", None),
        ("explicit", Some("4")),
        ("environment", Some("3")),
        ("existing", Some("3")),
        ("invalid_environment", Some("0")),
        ("invalid_explicit", None),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "runtime_child", "--nocapture"])
            .env("Z_STACK_TEST_THREAD_CASE", case)
            .env_remove("RAYON_NUM_THREADS");
        if let Some(value) = env {
            command.env("RAYON_NUM_THREADS", value);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
