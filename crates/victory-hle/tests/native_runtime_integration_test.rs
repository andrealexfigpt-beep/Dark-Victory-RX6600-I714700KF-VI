//! Integration tests for the unified native runtime.

mod native_runtime_integration_tests {
    use std::time::Duration;

    use victory_hle::native_runtime::{
        initialize_runtime, runtime, is_native_runtime_active, ExecutionMode, RuntimeConfig,
    };

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            execution_mode: ExecutionMode::Native,
            enable_shader_cache: true,
            enable_pipeline_cache: true,
            enable_permutation_persistence: true,
            shader_cache_dir: Some(std::env::temp_dir().join("victory_test_shader_cache")),
            pipeline_cache_dir: Some(std::env::temp_dir().join("victory_test_pipeline_cache")),
            permutation_cache_dir: Some(std::env::temp_dir().join("victory_test_perm_cache")),
            memory_budget: None,
            enable_profiling: true,
            profiling_interval: Duration::from_millis(50),
            background_task_budget_ms: 16,
            streaming_budget_mb: 256,
            max_background_threads: 2,
        }
    }

    #[test]
    fn test_runtime_initialization() {
        let config = test_config();
        let result = initialize_runtime(config);
        
        assert!(result.is_ok(), "Runtime initialization failed: {:?}", result.err());
        
        let rt = result.unwrap();
        assert!(is_native_runtime_active(), "Runtime should be active after init");
        assert_eq!(rt.execution_mode(), ExecutionMode::Native);
        
        // Check hardware capabilities were detected
        let caps = rt.hardware_capabilities();
        // On Windows with a GPU, these should be non-zero
        // (may be 0 in CI without GPU)
        println!("Hardware caps: {:?}", caps);
    }

    #[test]
    fn test_execution_mode_switching() {
        let config = test_config();
        let _ = initialize_runtime(config);
        
        let rt = runtime().expect("Runtime should be available");
        assert_eq!(rt.execution_mode(), ExecutionMode::Native);
        
        rt.set_execution_mode(ExecutionMode::Legacy);
        assert_eq!(rt.execution_mode(), ExecutionMode::Legacy);
        
        rt.set_execution_mode(ExecutionMode::Hybrid);
        assert_eq!(rt.execution_mode(), ExecutionMode::Hybrid);
    }

    #[test]
    fn test_background_task_scheduling() {
        let config = test_config();
        let _ = initialize_runtime(config);
        
        let rt = runtime().expect("Runtime should be available");
        
        let task_id = rt.schedule_background_task(
            "test_task".to_string(),
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(10));
            }),
            1,
            10,
        ).expect("Failed to schedule task");
        
        assert!(task_id > 0);
        
        // Give task time to execute
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn test_profiling_collection() {
        let config = test_config();
        let _ = initialize_runtime(config);
        
        let rt = runtime().expect("Runtime should be available");
        
        // Wait for profiling interval
        std::thread::sleep(Duration::from_millis(200));
        
        let profile = rt.latest_profile();
        assert!(profile.is_some(), "Profile should be available after interval");
        
        let profile = profile.unwrap();
        println!("Profile: {:?}", profile);
    }

    #[test]
    fn test_memory_budget_status() {
        let config = test_config();
        let _ = initialize_runtime(config);
        
        let rt = runtime().expect("Runtime should be available");
        
        let budget = rt.memory_budget_status();
        // May be None in CI without GPU
        if let Some(budget) = budget {
            println!("Memory budget: VRAM={} MB, Shared={} MB", 
                budget.vram_budget / (1024*1024), 
                budget.shared_budget / (1024*1024));
        }
    }

    #[test]
    fn test_multiple_initialization_fails() {
        let config = test_config();
        let _ = initialize_runtime(config.clone());
        
        // Second initialization should fail
        let result = initialize_runtime(config);
        assert!(result.is_err(), "Second initialization should fail");
    }
}

#[cfg(test)]
mod native_runtime_unit_tests {
    use victory_hle::native_runtime::{
        ExecutionMode, RuntimeConfig, HardwareCapabilities, SubsystemProfile, RuntimeProfile,
    };

    #[test]
    fn test_execution_mode_variants() {
        assert_eq!(ExecutionMode::Native as u32, 0);
        assert_eq!(ExecutionMode::Legacy as u32, 1);
        assert_eq!(ExecutionMode::Hybrid as u32, 2);
        assert_eq!(ExecutionMode::default(), ExecutionMode::Native);
    }

    #[test]
    fn test_runtime_config_defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config.execution_mode, ExecutionMode::Native);
        assert!(config.enable_shader_cache);
        assert!(config.enable_pipeline_cache);
        assert!(config.enable_permutation_persistence);
        assert!(config.enable_profiling);
        assert_eq!(config.profiling_interval.as_millis(), 100);
        assert_eq!(config.background_task_budget_ms, 16);
        assert_eq!(config.streaming_budget_mb, 512);
        assert_eq!(config.max_background_threads, 4);
    }

    #[test]
    fn test_hardware_capabilities_default() {
        let caps = HardwareCapabilities::default();
        assert_eq!(caps.vendor_id, 0);
        assert_eq!(caps.device_id, 0);
        assert_eq!(caps.vram_budget, 0);
        assert_eq!(caps.max_wave_lane_count, 0);
        assert!(!caps.supports_mesh_shaders);
        assert!(!caps.is_rdna);
    }

    #[test]
    fn test_subsystem_profile_default() {
        let profile = SubsystemProfile::default();
        assert_eq!(profile.cpu_time_ms, 0.0);
        assert_eq!(profile.gpu_time_ms, 0.0);
        assert_eq!(profile.cache_hits, 0);
        assert_eq!(profile.cache_misses, 0);
    }

    #[test]
    fn test_runtime_profile_default() {
        let profile = RuntimeProfile::default();
        assert_eq!(profile.memory.cpu_time_ms, 0.0);
        assert_eq!(profile.shader.cpu_time_ms, 0.0);
        assert_eq!(profile.graphics.cpu_time_ms, 0.0);
        assert_eq!(profile.platform.cpu_time_ms, 0.0);
        assert_eq!(profile.background.cpu_time_ms, 0.0);
    }
}